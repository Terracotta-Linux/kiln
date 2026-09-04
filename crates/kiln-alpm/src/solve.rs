//! The dependency solver, run for its answer rather than its effect.
//!
//! resolution is cheap, networked, and metadata-only. Nothing here
//! downloads a package or touches the staging root — the transaction is
//! prepared, read, and released. That is what makes `kiln check` possible
//! without building, and it is the load-bearing half of the plan/realize split.

use crate::error::{Error, Result};
use crate::session::Session;
use alpm::{PrepareData, TransFlag};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// What the configuration asked for. Dep strings, not just names: `base` and
/// `linux>=6.19` are both legal, because that is what libalpm satisfies.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub want: Vec<String>,
    /// must not appear, **even as a dependency**.
    pub exclude: Vec<String>,
}

impl Request {
    pub fn new(want: impl IntoIterator<Item = String>) -> Request {
        Request {
            want: want.into_iter().collect(),
            exclude: Vec::new(),
        }
    }

    pub fn excluding(mut self, exclude: impl IntoIterator<Item = String>) -> Request {
        self.exclude = exclude.into_iter().collect();
        self
    }
}

/// One package the solver chose. Everything `ResolvedInput::RepoPackage`
/// needs, plus what the build record and `kiln check` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolvedPackage {
    pub name: String,
    /// epoch:version-rel, exactly as pacman writes it.
    pub version: String,
    pub repo: String,
    pub filename: String,
    /// recorded regardless, so a rebuild can be satisfied from the
    /// artifact store or the Archive after mirrors have moved on.
    pub sha256: Option<String>,
    pub download_size: i64,
    pub install_size: i64,
    /// Dependency *names*, version constraints stripped. Enough to answer
    /// "what pulled this in" without re-running the solver.
    pub depends: Vec<String>,
    pub provides: Vec<String>,
    /// Named in the configuration, as opposed to pulled in. Drives the install
    /// reason recorded in the image's pacman database.
    pub explicit: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Solution {
    /// Sorted by name: order is content-determined, never solver-determined,
    /// so two runs of the same plan hash the same.
    pub packages: Vec<SolvedPackage>,
    pub download_size: i64,
    pub install_size: i64,
}

impl Solution {
    pub fn get(&self, name: &str) -> Option<&SolvedPackage> {
        self.packages
            .binary_search_by(|p| p.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.packages[i])
    }

    pub fn names(&self) -> Vec<&str> {
        self.packages.iter().map(|p| p.name.as_str()).collect()
    }

    /// Which packages in the solution require `name`, directly. Sorted.
    /// The data behind `kiln why` and behind the `Excluded` diagnostic.
    pub fn dependents_of(&self, name: &str) -> Vec<String> {
        let target = match self.get(name) {
            Some(p) => p,
            None => return Vec::new(),
        };
        // A dependency can be satisfied by the package's own name or by
        // anything it provides, so both count as "requires it".
        let satisfied: BTreeSet<&str> = std::iter::once(target.name.as_str())
            .chain(target.provides.iter().map(String::as_str))
            .collect();
        let mut out: Vec<String> = self
            .packages
            .iter()
            .filter(|p| p.name != name && p.depends.iter().any(|d| satisfied.contains(d.as_str())))
            .map(|p| p.name.clone())
            .collect();
        out.sort();
        out
    }

    /// The shortest chain from an explicitly requested package to `name`.
    /// `kiln why neovim` wants "base → … → neovim", not a set.
    pub fn chain_to(&self, name: &str) -> Option<Vec<String>> {
        self.get(name)?;
        // Breadth-first from every explicit root, so the first arrival is the
        // shortest explanation rather than an arbitrary one.
        let mut came_from: BTreeMap<&str, &str> = BTreeMap::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for p in self.packages.iter().filter(|p| p.explicit) {
            queue.push_back(&p.name);
            seen.insert(&p.name);
        }
        while let Some(cur) = queue.pop_front() {
            if cur == name {
                let mut chain = vec![cur.to_string()];
                let mut at = cur;
                while let Some(prev) = came_from.get(at) {
                    chain.push(prev.to_string());
                    at = prev;
                }
                chain.reverse();
                return Some(chain);
            }
            let Some(pkg) = self.get(cur) else { continue };
            for dep in &pkg.depends {
                for next in self.providers_of(dep) {
                    if seen.insert(next) {
                        came_from.insert(next, cur);
                        queue.push_back(next);
                    }
                }
            }
        }
        None
    }

    /// Every package in the solution that satisfies `dep`, by name or by
    /// `provides`. `providers_of("init")` is how assembly asks whether this
    /// image has something to boot into.
    pub fn providers_of(&self, dep: &str) -> Vec<&str> {
        self.packages
            .iter()
            .filter(|p| p.name == dep || p.provides.iter().any(|pr| pr == dep))
            .map(|p| p.name.as_str())
            .collect()
    }
}

impl Session {
    /// Resolve `request` against the registered sync databases.
    ///
    /// The transaction is opened `NO_LOCK` because this is a read: `kiln check`
    /// must not need write access to the database directory, and must not lock
    /// out a build running beside it.
    pub fn solve(&mut self, request: &Request) -> Result<Solution> {
        self.alpm
            .trans_init(TransFlag::NO_LOCK)
            .map_err(|e| Error::alpm("starting resolution", e))?;

        let solution = self.solve_inner(request);

        // Released whether or not the solve worked: leaving a transaction open
        // poisons every later call on the handle with a confusing error.
        let _ = self.alpm.trans_release();

        let solution = solution?;
        check_excludes(&solution, &request.exclude)?;
        Ok(solution)
    }

    fn solve_inner(&mut self, request: &Request) -> Result<Solution> {
        let alpm = &mut self.alpm;
        let syncdbs = alpm.syncdbs();

        // The names the configuration asked for, so the solution can mark them
        // explicit. `find_satisfier` may answer with a different name — asking
        // for `sh` gets `bash` — and it is that answer that goes in the set.
        let mut explicit: BTreeSet<String> = BTreeSet::new();

        for want in &request.want {
            let pkg = syncdbs
                .find_satisfier(want.as_str())
                .ok_or_else(|| Error::NotFound { name: want.clone() })?;
            explicit.insert(pkg.name().to_string());
            if let Err(e) = alpm.trans_add_pkg(pkg) {
                // A package named twice — directly and via a module — is not a
                // mistake worth failing a build over.
                if e.error != alpm::Error::TransDupTarget {
                    return Err(Error::alpm("selecting a package", e.error));
                }
            }
        }

        if let Err(e) = alpm.trans_prepare() {
            return Err(prepare_error(&e));
        }

        let mut packages: Vec<SolvedPackage> = alpm
            .trans_add()
            .into_iter()
            .map(|p| SolvedPackage {
                name: p.name().to_string(),
                version: p.version().to_string(),
                repo: p.db().map(|d| d.name().to_string()).unwrap_or_default(),
                filename: p.filename().unwrap_or_default().to_string(),
                sha256: p.sha256sum().map(str::to_string),
                download_size: p.size(),
                install_size: p.isize(),
                depends: p
                    .depends()
                    .into_iter()
                    .map(|d| d.name().to_string())
                    .collect(),
                provides: p
                    .provides()
                    .into_iter()
                    .map(|d| d.name().to_string())
                    .collect(),
                explicit: explicit.contains(p.name()),
            })
            .collect();
        packages.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Solution {
            download_size: packages.iter().map(|p| p.download_size).sum(),
            install_size: packages.iter().map(|p| p.install_size).sum(),
            packages,
        })
    }
}

/// `exclude` means "must not appear, even as a dependency".
///
/// Enforced *after* the solve, deliberately. libalpm's `assume_installed` would
/// make the dependency vanish and produce an image missing a library something
/// links against — a broken image nobody asked for. Refusing, and naming what
/// pulled it in, leaves the decision with the person who wrote the config.
fn check_excludes(solution: &Solution, exclude: &[String]) -> Result<()> {
    for name in exclude {
        if solution.get(name).is_some() {
            return Err(Error::Excluded {
                name: name.clone(),
                pulled_in_by: solution.dependents_of(name),
            });
        }
    }
    Ok(())
}

pub(crate) fn prepare_error(e: &alpm::PrepareError<'_>) -> Error {
    match e.data() {
        Some(PrepareData::UnsatisfiedDeps(missing)) => missing
            .into_iter()
            .next()
            .map(|m| Error::Unsatisfied {
                // `target` is the package that has the unsatisfied dependency.
                // `causing_pkg` is a different question — which *removal*
                // broke it — and is null on the resolution path, so reading it
                // here would produce "nothing provides x" with the one useful
                // name dropped.
                wanted_by: Some(m.target().to_string()),
                dep: m.depend().to_string(),
            })
            .unwrap_or_else(|| Error::alpm("resolving dependencies", e.error())),
        Some(PrepareData::ConflictingDeps(conflicts)) => conflicts
            .into_iter()
            .next()
            .map(|c| Error::Conflict {
                first: c.package1().name().to_string(),
                second: c.package2().name().to_string(),
                reason: c.reason().to_string(),
            })
            .unwrap_or_else(|| Error::alpm("resolving dependencies", e.error())),
        Some(PrepareData::PkgInvalidArch(pkgs)) => pkgs
            .into_iter()
            .next()
            .map(|p| Error::WrongArch {
                name: p.name().to_string(),
                arch: p.arch().unwrap_or("unknown").to_string(),
            })
            .unwrap_or_else(|| Error::alpm("resolving dependencies", e.error())),
        None => Error::alpm("resolving dependencies", e.error()),
    }
}
