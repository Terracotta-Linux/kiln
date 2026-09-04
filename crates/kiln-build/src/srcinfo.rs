//! `.SRCINFO`: what a PKGBUILD declares, without running it.
//!
//! A PKGBUILD is a bash script, so reading its metadata means executing it.
//! `.SRCINFO` is the same information already flattened — no expansion, no
//! subshells, no `pkgver()` — which is why Kiln prefers it and only falls back
//! to `makepkg --printsrcinfo` in a sandbox when a recipe does not ship one.
//!
//! The format is `key = value`, indented under a `pkgbase` or `pkgname`
//! section, with architecture-suffixed variants (`source_x86_64`). Two things
//! about it are easy to get wrong and are the reason this is a real parser
//! rather than a few `lines().filter()` calls:
//!
//! - **Checksums correspond to sources positionally.** `sha256sums` is a
//!   parallel list, not a map, and the arch-suffixed lists are *separate*
//!   parallel lists. Zipping the wrong pair silently pins the wrong bytes.
//! - **`SKIP` is not a checksum.** It means the source is unverifiable, which
//!   makes the package volatile rather than merely unpinned.

use std::collections::BTreeMap;

/// One `source=()` entry with the checksum that sits opposite it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Source {
    /// As written: a URL, a `filename::url` rename, or a local filename.
    pub spec: String,
    /// `None` where the recipe wrote `SKIP`.
    pub sha256: Option<String>,
}

impl Source {
    /// The filename `makepkg` will save this as, honouring `name::url`.
    pub fn filename(&self) -> &str {
        match self.spec.split_once("::") {
            Some((name, _)) => name,
            None => self
                .spec
                .rsplit('/')
                .next()
                .unwrap_or(&self.spec)
                .split('?')
                .next()
                .unwrap_or(&self.spec),
        }
    }

    /// A local file in the recipe directory rather than something to fetch.
    pub fn is_local(&self) -> bool {
        let url = self.spec.split_once("::").map_or(&*self.spec, |(_, u)| u);
        !url.contains("://")
    }

    /// a VCS source produces a version only after fetching, so it cannot
    /// be resolved cheaply. `SKIP` says the same thing about its contents.
    pub fn is_volatile(&self) -> bool {
        let url = self.spec.split_once("::").map_or(&*self.spec, |(_, u)| u);
        self.sha256.is_none()
            || ["git+", "svn+", "hg+", "bzr+"]
                .iter()
                .any(|p| url.starts_with(p))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Srcinfo {
    pub pkgbase: String,
    pub pkgver: String,
    pub pkgrel: String,
    pub epoch: Option<String>,
    /// Every `pkgname` the recipe produces, in declaration order — a split
    /// package builds several at once and Kiln has to know all of them.
    pub pkgnames: Vec<String>,
    pub arch: Vec<String>,
    pub depends: Vec<String>,
    pub makedepends: Vec<String>,
    pub checkdepends: Vec<String>,
    pub provides: Vec<String>,
    /// Architecture-independent sources first, then this architecture's.
    pub sources: Vec<Source>,
}

impl Srcinfo {
    /// `epoch:pkgver-pkgrel`, the way pacman writes a version.
    pub fn evr(&self) -> String {
        match &self.epoch {
            Some(e) => format!("{e}:{}-{}", self.pkgver, self.pkgrel),
            None => format!("{}-{}", self.pkgver, self.pkgrel),
        }
    }

    /// A volatile recipe: one whose version or sources cannot be known without
    /// fetching. `pkgver()` is not visible in `.SRCINFO` at all, so a VCS
    /// source is the signal.
    pub fn is_volatile(&self) -> bool {
        self.sources.iter().any(Source::is_volatile)
    }

    pub fn volatile_sources(&self) -> Vec<&Source> {
        self.sources.iter().filter(|s| s.is_volatile()).collect()
    }
}

/// Parse `.SRCINFO` for `arch`.
///
/// Unknown keys are ignored rather than rejected: `.SRCINFO` is written by
/// `makepkg`, gains fields over time, and a Kiln that refuses to read a recipe
/// because pacman learned a new array would be a worse tool than one that
/// reads what it understands.
pub fn parse(text: &str, arch: &str) -> Result<Srcinfo, Error> {
    let mut out = Srcinfo::default();
    // Collected separately because the checksum lists are positional against
    // the source lists *of the same suffix*, and the two suffixes must not be
    // interleaved before they are zipped.
    let mut sources: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut sums: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut in_split_package = false;

    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(Error {
                line: number + 1,
                text: trimmed.to_string(),
                why: "expected `key = value`",
            });
        };
        let (key, value) = (key.trim(), value.trim());

        // A `pkgname` section overrides the base for the package it names.
        // Kiln builds whole recipes rather than individual split packages, so
        // it records the names and takes everything else from the base — which
        // is also what makepkg does for anything a split package leaves unset.
        if key == "pkgname" {
            in_split_package = true;
            out.pkgnames.push(value.to_string());
            continue;
        }
        if key == "pkgbase" {
            in_split_package = false;
            out.pkgbase = value.to_string();
            continue;
        }
        if in_split_package {
            continue;
        }

        let (bare, suffix) = split_arch_suffix(key);
        // A suffix for some other architecture is not this image's business.
        if suffix.is_some_and(|s| s != arch) {
            continue;
        }
        let suffix = suffix.unwrap_or("").to_string();

        match bare {
            "pkgver" => out.pkgver = value.to_string(),
            "pkgrel" => out.pkgrel = value.to_string(),
            "epoch" => out.epoch = Some(value.to_string()),
            "arch" => out.arch.push(value.to_string()),
            "depends" => out.depends.push(value.to_string()),
            "makedepends" => out.makedepends.push(value.to_string()),
            "checkdepends" => out.checkdepends.push(value.to_string()),
            "provides" => out.provides.push(value.to_string()),
            "source" => sources.entry(suffix).or_default().push(value.to_string()),
            "sha256sums" => sums.entry(suffix).or_default().push(value.to_string()),
            _ => {}
        }
    }

    if out.pkgbase.is_empty() {
        return Err(Error {
            line: 0,
            text: String::new(),
            why: "no `pkgbase`, so this is not a .SRCINFO",
        });
    }
    if out.pkgnames.is_empty() {
        out.pkgnames.push(out.pkgbase.clone());
    }

    // Zip each suffix's lists against each other, never across suffixes. The
    // unsuffixed group comes first so a plan's source order is stable.
    for (suffix, specs) in sources {
        let checksums = sums.get(&suffix).cloned().unwrap_or_default();
        for (index, spec) in specs.into_iter().enumerate() {
            let sha256 = match checksums.get(index).map(String::as_str) {
                // `SKIP` is the recipe saying "this cannot be verified", which
                // is a different fact from "no checksum was given" and travels
                // all the way to `kiln check` as a volatile input.
                Some("SKIP") | None => None,
                Some(s) => Some(s.to_string()),
            };
            out.sources.push(Source { spec, sha256 });
        }
    }
    Ok(out)
}

/// `source_x86_64` → `("source", Some("x86_64"))`.
fn split_arch_suffix(key: &str) -> (&str, Option<&str>) {
    const SUFFIXABLE: &[&str] = &[
        "source",
        "depends",
        "makedepends",
        "checkdepends",
        "provides",
        "conflicts",
        "replaces",
        "sha256sums",
        "sha512sums",
        "md5sums",
        "b2sums",
    ];
    for base in SUFFIXABLE {
        if let Some(rest) = key.strip_prefix(base) {
            if let Some(arch) = rest.strip_prefix('_') {
                return (base, Some(arch));
            }
        }
    }
    (key, None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub text: String,
    pub why: &'static str,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.why)
        } else {
            write!(f, "line {}: {} — `{}`", self.line, self.why, self.text)
        }
    }
}

impl std::error::Error for Error {}
