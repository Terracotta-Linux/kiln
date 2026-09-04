//! Configuration discovery and the config-root security boundary.

use kiln_diag::{Diag, Origin, SourceFile, Src};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const ENTRY_FILE: &str = "system.toml";
pub const DEFAULT_CONFIG_DIR: &str = "/etc/kiln";
pub const DEFAULT_MODULE_DIR: &str = "/usr/share/kiln/modules";
pub const MODULE_PREFIX: &str = "@kiln/";

/// Where files may come from, and the rules for getting at them.
pub struct Loader {
    /// The directory containing the entry point. A security boundary.
    pub config_root: PathBuf,
    /// Shipped module library, `/usr/share/kiln/modules`.
    pub module_root: PathBuf,
    /// escaping the config root requires this, and warns.
    pub allow_external: bool,
    cache: BTreeMap<PathBuf, Src>,
    /// Every path that escaped the config root, for the warning.
    pub escapes: Vec<(PathBuf, Origin)>,
}

/// Resolved entry point plus its root.
pub struct Entry {
    pub path: PathBuf,
    pub config_root: PathBuf,
}

/// `--config` may name a file or a directory; a directory implies `system.toml`
/// inside it. With nothing given, `$KILN_CONFIG_DIR` then `/etc/kiln`.
pub fn entry_point(config: Option<&Path>) -> Result<Entry, Diag> {
    let candidate = match config {
        Some(p) => p.to_path_buf(),
        None => std::env::var_os("KILN_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR)),
    };

    let path = if candidate.is_dir() {
        candidate.join(ENTRY_FILE)
    } else {
        candidate
    };

    if !path.exists() {
        return Err(Diag::error(
            "kiln::discovery",
            format!("no configuration at {}", path.display()),
        )
        .help(if config.is_some() {
            "check the path given to --config".to_string()
        } else {
            format!("create {DEFAULT_CONFIG_DIR}/{ENTRY_FILE}, or run `kiln init`")
        }));
    }

    let path = canonical(&path)?;
    let config_root = path
        .parent()
        .ok_or_else(|| Diag::error("kiln::discovery", "entry point has no parent directory"))?
        .to_path_buf();
    Ok(Entry { path, config_root })
}

/// A root as it will be compared against the paths of files actually loaded.
///
/// Every file the loader opens is canonicalized, so a root that is not — a
/// relative `--module-root ./modules`, or a `..` left in by a test's
/// `repo_root().join("modules")` — never prefixes any of them. The visible
/// consequence was `kiln explain kernel.cmdline` naming
/// `/home/you/kiln/modules/gpu/nvidia-open.toml:14` where promises
/// `@kiln/gpu/nvidia-open:14`, and it is not only cosmetic: the same prefix
/// test is what tells the config root apart from outside it.
///
/// Falls back to the path as given when it does not exist, because the default
/// module root is absent on a machine where Kiln has not been installed, and
/// a missing module library is `resolve_include`'s diagnostic to give, not
/// this function's.
fn settled(p: PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or(p)
}

fn canonical(p: &Path) -> Result<PathBuf, Diag> {
    p.canonicalize().map_err(|e| {
        Diag::error(
            "kiln::discovery",
            format!("cannot resolve {}: {e}", p.display()),
        )
    })
}

impl Loader {
    pub fn new(config_root: PathBuf) -> Loader {
        Loader {
            config_root: settled(config_root),
            module_root: settled(
                std::env::var_os("KILN_MODULE_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_MODULE_DIR)),
            ),
            allow_external: false,
            cache: BTreeMap::new(),
            escapes: Vec::new(),
        }
    }

    pub fn with_module_root(mut self, p: PathBuf) -> Loader {
        self.module_root = settled(p);
        self
    }

    pub fn allow_external(mut self, yes: bool) -> Loader {
        self.allow_external = yes;
        self
    }

    /// How a path should be shown to the user: config-root-relative, or
    /// `@kiln/...` for a shipped module. Never absolute if we can help it.
    pub fn display_name(&self, path: &Path) -> String {
        if let Ok(rel) = path.strip_prefix(&self.config_root) {
            return rel.display().to_string();
        }
        if let Ok(rel) = path.strip_prefix(&self.module_root) {
            let s = rel.display().to_string();
            return format!("{MODULE_PREFIX}{}", s.strip_suffix(".toml").unwrap_or(&s));
        }
        path.display().to_string()
    }

    /// Resolve one `include` reference.
    pub fn resolve_include(&self, reference: &str, from: &Origin) -> Result<PathBuf, Diag> {
        if let Some(rest) = reference.strip_prefix(MODULE_PREFIX) {
            if rest.is_empty() || rest.contains("..") {
                return Err(Diag::error(
                    "kiln::graph",
                    format!("invalid module reference `{reference}`"),
                )
                .label(from, "here")
                .help("module references look like `@kiln/profiles/minimal`"));
            }
            let p = self.module_root.join(format!("{rest}.toml"));
            if !p.exists() {
                let available = self.available_modules();
                let names: Vec<&str> = available.iter().map(String::as_str).collect();
                return Err(
                    Diag::error("kiln::graph", format!("no such module `{reference}`"))
                        .label(from, "included here")
                        .maybe_help(
                            kiln_diag::did_you_mean(reference, names.iter().copied()).or_else(
                                || {
                                    Some(if available.is_empty() {
                                        format!(
                                            "the module library at {} is empty or missing",
                                            self.module_root.display()
                                        )
                                    } else {
                                        format!("the module library has: {}", available.join(", "))
                                    })
                                },
                            ),
                        ),
                );
            }
            return canonical(&p).map_err(|d| d.label(from, "included here"));
        }

        if reference.starts_with("git+") || reference.contains("://") {
            return Err(
                Diag::error("kiln::graph", "remote includes are not supported")
                    .label(from, "here")
                    .help(
                        "vendor the module into your config tree instead. \
                     A remote include is an unpinned supply-chain input that breaks offline \
                     builds.",
                    ),
            );
        }

        // Relative to the *including file's* directory.
        let base = from.path().parent().unwrap_or(&self.config_root);
        let joined = base.join(reference);
        if !joined.exists() {
            return Err(
                Diag::error("kiln::graph", format!("no such file `{reference}`"))
                    .label(from, "included here")
                    .help(format!("looked for {}", joined.display())),
            );
        }
        let resolved = canonical(&joined).map_err(|d| d.label(from, "included here"))?;
        self.check_boundary(&resolved, from, "include")?;
        Ok(resolved)
    }

    /// every `source`/`path` must resolve, after symlink resolution, inside
    /// the config root. This exists to make
    /// `source = "../../home/you/.ssh/id_ed25519"` a hard error rather than a leak.
    pub fn check_boundary(&self, resolved: &Path, at: &Origin, what: &str) -> Result<(), Diag> {
        if resolved.starts_with(&self.config_root) || resolved.starts_with(&self.module_root) {
            return Ok(());
        }
        if self.allow_external {
            return Ok(());
        }
        Err(
            Diag::error("kiln::security", format!("{what} escapes the config root"))
                .label(at, format!("resolves to {}", resolved.display()))
                .help(format!(
            "the config root is {}. Everything the image is built from must live inside it. \
             Pass --allow-external-sources if you really mean this.",
            self.config_root.display()
        )),
        )
    }

    /// Read a file once, keeping its text for diagnostics.
    pub fn load(&mut self, path: &Path, at: Option<&Origin>) -> Result<Src, Diag> {
        if let Some(src) = self.cache.get(path) {
            return Ok(src.clone());
        }
        let text = std::fs::read_to_string(path).map_err(|e| {
            let d = Diag::error(
                "kiln::discovery",
                format!("cannot read {}: {e}", path.display()),
            );
            match at {
                Some(o) => d.label(o, "included here"),
                None => d,
            }
        })?;
        let src = SourceFile::new(path, self.display_name(path), text);
        self.cache.insert(path.to_path_buf(), src.clone());
        Ok(src)
    }

    /// Every shipped module, as `@kiln/...` references, for did-you-mean.
    pub fn available_modules(&self) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                    if let Ok(rel) = p.strip_prefix(root) {
                        let s = rel.with_extension("").display().to_string();
                        out.push(format!("{MODULE_PREFIX}{s}"));
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.module_root, &self.module_root, &mut out);
        out.sort();
        out
    }

    /// Record a `source`/`path` that left the config root, so the run can warn
    /// once per path rather than per use.
    pub fn note_escape(&mut self, path: PathBuf, at: Origin) {
        if !self.escapes.iter().any(|(p, _)| *p == path) {
            self.escapes.push((path, at));
        }
    }
}
