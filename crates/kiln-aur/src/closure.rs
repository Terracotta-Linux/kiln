//! The AUR dependency closure.
//!
//! > **AUR dependency resolution** is recursive over `Depends`/`MakeDepends`
//! > that are not in the official repos, with a cycle check and a depth cap.
//! > Every transitively pulled AUR package appears in the lock and in
//! > `kiln check` output, explicitly marked as *pulled in by* whatever required
//! > it. Nothing enters the image anonymously.

use crate::rpc::{self, Info};
use crate::transport::Transport;
use kiln_manifest::Hash;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// "a cycle check and a depth cap".
///
/// The cycle check makes the cap unnecessary for correctness, so what the cap
/// actually guards is a *chain* — a legitimate but absurd dependency ladder, or
/// a hostile one built to make Kiln issue requests forever. Ten is far past
/// anything real and far short of anything painful.
pub const MAX_DEPTH: usize = 10;

/// One AUR package, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub pkgbase: String,
    pub version: String,
    /// The resolved `HEAD`, or the commit the configuration pinned.
    pub commit: String,
    /// blake3 of the RPC's own description of the package. A change here at an
    /// unchanged commit means the AUR's metadata moved under us, which is worth
    /// noticing even though it should not happen.
    pub srcinfo_hash: Hash,
    /// `None` when the configuration named it. Nothing enters the image
    /// anonymously.
    pub pulled_in_by: Option<String>,
    /// How deep in the closure it was found. Reported, not hashed.
    pub depth: usize,
    pub maintainer: Option<String>,
    /// Flagged by the AUR itself. Not an error — plenty of working packages are
    /// marked out of date — but the user should hear it once.
    pub out_of_date: bool,
}

/// The whole closure, plus what could not be settled without fetching.
#[derive(Debug, Clone, Default)]
pub struct Closure {
    /// Sorted by name, so the plan does not depend on traversal order.
    pub packages: Vec<Resolved>,
    /// VCS packages, whose `pkgver()` runs upstream code and produces a
    /// version only after fetching.
    pub volatile: Vec<(String, String)>,
}

impl Closure {
    pub fn get(&self, name: &str) -> Option<&Resolved> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// The chain from something the configuration asked for down to `name`.
    /// `kiln check` prints this rather than a bare list.
    pub fn chain_to(&self, name: &str) -> Vec<String> {
        let mut chain = vec![name.to_string()];
        let mut at = name.to_string();
        // The closure is a tree by construction — each package records the one
        // that first reached it — so this walk terminates without a seen-set.
        while let Some(parent) = self.get(&at).and_then(|p| p.pulled_in_by.clone()) {
            chain.push(parent.clone());
            at = parent;
        }
        chain.reverse();
        chain
    }
}

/// What the configuration asked for.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Package name → the commit it is pinned to, if any.
    /// `{ name = "foo-git", commit = "a81fc2e" }` pins it.
    pub wanted: BTreeMap<String, Option<String>>,
}

impl Request {
    pub fn new(wanted: impl IntoIterator<Item = (String, Option<String>)>) -> Request {
        Request {
            wanted: wanted.into_iter().collect(),
        }
    }
}

/// Resolve the closure.
///
/// `in_official_repos` answers whether a dependency name is satisfied by the
/// official repositories — which is `kiln-alpm`'s question, not this crate's,
/// so it arrives as a closure. That keeps `kiln-aur` free of libalpm and makes
/// the recursion testable with a set literal.
pub fn resolve(
    request: &Request,
    transport: &dyn Transport,
    in_official_repos: &dyn Fn(&str) -> bool,
) -> Result<Closure, Error> {
    let mut resolved: BTreeMap<String, Resolved> = BTreeMap::new();
    let mut volatile: Vec<(String, String)> = Vec::new();

    // (name, who wanted it, depth). Breadth-first so the recorded parent is the
    // *shortest* explanation, which is the one a person wants to read.
    let mut frontier: VecDeque<(String, Option<String>, usize)> = request
        .wanted
        .keys()
        .map(|name| (name.clone(), None, 0))
        .collect();
    let mut seen: BTreeSet<String> = request.wanted.keys().cloned().collect();

    while !frontier.is_empty() {
        // **batched**. Everything at the current depth goes in one
        // request, so a closure costs one round trip per level rather than one
        // per package.
        let level: Vec<(String, Option<String>, usize)> = frontier.drain(..).collect();
        let depth = level.first().map(|(_, _, d)| *d).unwrap_or(0);
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep {
                depth,
                at: level.iter().map(|(n, ..)| n.clone()).collect(),
            });
        }

        let names: Vec<String> = level.iter().map(|(n, ..)| n.clone()).collect();
        let body = transport.get(&rpc::url(&names)).map_err(Error::Transport)?;
        let infos = rpc::parse(&body).map_err(Error::Rpc)?;

        for (name, pulled_in_by, depth) in level {
            let Some(info) = infos.get(&name) else {
                return Err(Error::NotFound { name, pulled_in_by });
            };

            let commit = match request.wanted.get(&name).and_then(Clone::clone) {
                // A pin is a statement about what to build, so it is used as
                // given rather than checked against HEAD: the whole point of
                // pinning is that HEAD moving does not matter.
                Some(pinned) => pinned,
                None => transport
                    .head_of(&crate::repository(&info.package_base))
                    .map_err(Error::Transport)?,
            };

            if is_vcs(&name) {
                volatile.push((
                    name.clone(),
                    format!(
                        "`{name}` has a pkgver() that runs upstream code, so its version is \
                         only known after fetching"
                    ),
                ));
            }

            resolved.insert(
                name.clone(),
                Resolved {
                    name: name.clone(),
                    pkgbase: info.package_base.clone(),
                    version: info.version.clone(),
                    commit,
                    srcinfo_hash: fingerprint(info),
                    pulled_in_by,
                    depth,
                    maintainer: info.maintainer.clone(),
                    out_of_date: info.out_of_date.is_some(),
                },
            );

            for dependency in info.all_dependencies() {
                // A dependency spec can carry a version constraint or a
                // description; the closure walks names.
                let dep = bare_name(dependency);
                // The official repositories satisfy most of them, and those are
                // libalpm's problem rather than the AUR's.
                if in_official_repos(dep) || !seen.insert(dep.to_string()) {
                    continue;
                }
                frontier.push_back((dep.to_string(), Some(name.clone()), depth + 1));
            }
        }
    }

    let mut packages: Vec<Resolved> = resolved.into_values().collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    volatile.sort();
    volatile.dedup();
    Ok(Closure { packages, volatile })
}

/// `foo>=1.2` / `foo=1.2` / `foo: for bar` → `foo`.
pub fn bare_name(spec: &str) -> &str {
    let spec = spec.split(':').next().unwrap_or(spec).trim();
    spec.split(['<', '>', '=']).next().unwrap_or(spec).trim()
}

/// VCS packages have a `pkgver()` that runs upstream code.
///
/// The suffix is a naming convention rather than a guarantee, so this is a
/// heuristic — but it errs toward *marking something volatile that is not*,
/// which costs a line in `kiln check` output. Erring the other way would mean
/// reporting a version that turns out to be wrong, and Kiln is explicit that
/// an untrustworthy check is worse than no check.
pub fn is_vcs(name: &str) -> bool {
    ["-git", "-svn", "-hg", "-bzr", "-cvs", "-nightly"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// A stable fingerprint of the RPC's description of a package.
fn fingerprint(info: &Info) -> Hash {
    use kiln_manifest::{Canon, Canonical};
    Hash::of(
        &Canon::map([
            ("name", Canon::str(&info.name)),
            ("pkgbase", Canon::str(&info.package_base)),
            ("version", Canon::str(&info.version)),
            ("depends", info.depends.canon()),
            ("makedepends", info.make_depends.canon()),
            ("checkdepends", info.check_depends.canon()),
            ("provides", info.provides.canon()),
            ("conflicts", info.conflicts.canon()),
        ])
        .to_bytes(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NotFound {
        name: String,
        pulled_in_by: Option<String>,
    },
    TooDeep {
        depth: usize,
        at: Vec<String>,
    },
    Transport(crate::transport::Error),
    Rpc(rpc::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound { name, pulled_in_by } => match pulled_in_by {
                Some(by) => write!(f, "the AUR has no `{name}`, which `{by}` requires"),
                None => write!(f, "the AUR has no `{name}`"),
            },
            Error::TooDeep { depth, at } => write!(
                f,
                "the AUR dependency chain is more than {MAX_DEPTH} deep (reached {depth} at \
                 {}) — this is almost certainly not a real dependency graph",
                at.join(", ")
            ),
            Error::Transport(e) => write!(f, "{e}"),
            Error::Rpc(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}
