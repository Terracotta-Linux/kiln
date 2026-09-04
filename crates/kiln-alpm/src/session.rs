//! The alpm handle, built programmatically.
//!
//! There is no `pacman.conf` anywhere in this path. Generating one and shelling
//! out would mean parsing pacman's output to find out what happened; building
//! the handle directly means the solver's answer arrives as data.

use crate::error::{Error, Result};
use crate::repo::RepoSpec;
use std::path::{Path, PathBuf};

/// Where the package database lives in a Kiln image. Part of `/usr`,
/// read-only at runtime, and survives into the deployed system so `pacman -Q`,
/// `kiln why` and `kiln owns` work offline against a booted image.
///
/// Named here rather than inlined so that the one place Kiln decides this is
/// greppable — the decision is duplicated in the image's own `pacman.conf`
/// and the two must not drift.
pub const DB_PATH: &str = "usr/lib/sysimage/pacman";

#[derive(Debug, Clone)]
pub struct Config {
    /// The staging root, or `/` for querying the live system.
    pub root: PathBuf,
    pub dbpath: PathBuf,
    /// Content-addressed package cache.
    pub cachedirs: Vec<PathBuf>,
    /// Kiln's own GPG home, never the host's `/etc/pacman.d/gnupg`,
    /// because builds must work on a non-Arch host and in CI.
    pub gpgdir: Option<PathBuf>,
    /// Later entries shadow earlier ones by filename — the only lever there is
    /// over package-shipped hooks.
    pub hookdirs: Vec<PathBuf>,
    pub arch: String,
    pub repos: Vec<RepoSpec>,
    pub logfile: Option<PathBuf>,
}

impl Config {
    /// A handle over a staging root being assembled. The DB path is not a
    /// parameter: Kiln fixed it.
    pub fn for_root(root: impl Into<PathBuf>, arch: impl Into<String>) -> Config {
        let root = root.into();
        Config {
            dbpath: root.join(DB_PATH),
            root,
            cachedirs: Vec::new(),
            gpgdir: None,
            hookdirs: Vec::new(),
            arch: arch.into(),
            repos: Vec::new(),
            logfile: None,
        }
    }

    /// A handle for **resolution** rather than assembly.
    ///
    /// The root is an empty directory, deliberately. Kiln rebuilds the tree
    /// from scratch every time, so the solver must answer "what does a
    /// fresh install of this configuration contain" — resolving against the
    /// host's `/` would treat everything already installed there as satisfied
    /// and produce a plan missing most of the image.
    ///
    /// The sync databases live under the state directory and persist, because
    /// they are metadata that `kiln check` re-reads constantly and re-downloads
    /// rarely.
    pub fn for_resolution(state_dir: impl AsRef<Path>, arch: impl Into<String>) -> Config {
        let state = state_dir.as_ref();
        Config {
            root: state.join("resolve-root"),
            dbpath: state.join("cache/syncdb"),
            cachedirs: vec![state.join("cache/pkg")],
            gpgdir: Some(state.join("keyring")),
            hookdirs: Vec::new(),
            arch: arch.into(),
            repos: Vec::new(),
            logfile: None,
        }
    }

    pub fn with_repos(mut self, repos: Vec<RepoSpec>) -> Config {
        self.repos = repos;
        self
    }

    pub fn with_cache(mut self, dir: impl Into<PathBuf>) -> Config {
        self.cachedirs.push(dir.into());
        self
    }

    pub fn with_gpgdir(mut self, dir: impl Into<PathBuf>) -> Config {
        self.gpgdir = Some(dir.into());
        self
    }

    pub fn with_hookdir(mut self, dir: impl Into<PathBuf>) -> Config {
        self.hookdirs.push(dir.into());
        self
    }
}

pub struct Session {
    pub(crate) alpm: alpm::Alpm,
    pub(crate) config: Config,
}

impl Session {
    pub fn open(config: Config) -> Result<Session> {
        // libalpm creates neither the root nor the database directory, and its
        // error for a missing one — "could not find or read directory" — names
        // no path at all.
        for (dir, doing) in [
            (&config.root, "creating the target root"),
            (&config.dbpath, "creating the package database directory"),
        ] {
            std::fs::create_dir_all(dir).map_err(|e| Error::Alpm {
                doing,
                message: format!("{}: {e}", dir.display()),
            })?;
        }

        let mut alpm = alpm::Alpm::new(path_arg(&config.root)?, path_arg(&config.dbpath)?)
            .map_err(|e| Error::alpm("opening the package database", e))?;

        // Both the image's architecture and `any`: an arch-independent package
        // is valid in every image, and leaving it out makes half of Arch
        // invisible to the solver.
        alpm.add_architecture(config.arch.as_str())
            .and_then(|_| alpm.add_architecture("any"))
            .map_err(|e| Error::alpm("setting the architecture", e))?;

        for dir in &config.cachedirs {
            std::fs::create_dir_all(dir).ok();
            alpm.add_cachedir(path_arg(dir)?)
                .map_err(|e| Error::alpm("setting the package cache", e))?;
        }
        for dir in &config.hookdirs {
            alpm.add_hookdir(path_arg(dir)?)
                .map_err(|e| Error::alpm("setting a hook directory", e))?;
        }
        if let Some(g) = &config.gpgdir {
            alpm.set_gpgdir(path_arg(g)?)
                .map_err(|e| Error::alpm("setting the keyring", e))?;
        }
        if let Some(l) = &config.logfile {
            if let Some(parent) = l.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            alpm.set_logfile(path_arg(l)?)
                .map_err(|e| Error::alpm("setting the log file", e))?;
        }

        // Kiln never writes to syslog: a build's log belongs to the build
        // itself, not to the machine that happened to run it.
        alpm.set_use_syslog(false);
        // `check_space` statfs()es the target, which is meaningless for a
        // staging root that may be on a different filesystem than the image
        // will be, and fails outright inside some containers.
        alpm.set_check_space(false);

        let mut session = Session { alpm, config };
        session.register_repos()?;
        Ok(session)
    }

    fn register_repos(&mut self) -> Result<()> {
        // Registration order is priority order (see `mirrors::OFFICIAL`), so this
        // walks the configured list rather than a set.
        let repos = self.config.repos.clone();
        for repo in &repos {
            let db = self
                .alpm
                .register_syncdb_mut(repo.name.as_str(), repo.trust.siglevel())
                .map_err(|e| Error::alpm("registering a repository", e))?;
            for server in &repo.servers {
                db.add_server(server.as_str())
                    .map_err(|e| Error::alpm("setting repository servers", e))?;
            }
        }
        Ok(())
    }

    /// Refresh every registered sync database. This is metadata only, a few MB,
    /// and the most expensive thing `kiln check` does.
    ///
    /// Returns whether anything moved upstream, which is what makes "nothing to
    /// do" a cheap answer. libalpm updates the whole list in one call so that
    /// the downloads are parallel; that is also why the result is one boolean
    /// rather than a list of repositories.
    pub fn refresh(&mut self, force: bool) -> Result<bool> {
        let names: Vec<String> = self.config.repos.iter().map(|r| r.name.clone()).collect();
        self.alpm
            .syncdbs_mut()
            .update(force)
            .map(|up_to_date| !up_to_date)
            .map_err(|e| Error::Refresh {
                repo: names.join(", "),
                message: e.to_string(),
            })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Which package owns a path, against this session's local database.
    /// `kiln owns`. The path is image-absolute, with or without a leading
    /// slash — the pacman file list stores it without one.
    pub fn owns(&self, path: &str) -> Option<String> {
        let needle = path.trim_start_matches('/');
        self.alpm
            .localdb()
            .pkgs()
            .into_iter()
            .find(|p| p.files().contains(needle).is_some())
            .map(|p| p.name().to_string())
    }

    /// Do the registered repositories provide `dep`, by name or by `provides`?
    ///
    /// Distinct from asking whether something is in a *solution*: a build-time
    /// dependency of an AUR package is very often an official package that the
    /// image itself does not contain, and answering that question with the
    /// image's own package set would send every one of them to the AUR.
    pub fn provides(&self, dep: &str) -> bool {
        self.alpm.syncdbs().find_satisfier(dep).is_some()
    }

    /// Every package name the registered repositories hold, sorted and
    /// deduplicated. Built only when a resolution has already failed and a
    /// "did you mean" suggestion needs a namespace to draw from — it is a few
    /// thousand strings, and the succeeding path has no use for it.
    pub fn package_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .alpm
            .syncdbs()
            .into_iter()
            .flat_map(|db| db.pkgs().into_iter().map(|p| p.name().to_string()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// What the local database knows about one installed package, for
    /// `kiln why`.
    ///
    /// Answered from the image's *own* database rather than from the build
    /// record, and the difference matters: the record says what Kiln put in,
    /// and this says what the finished image contains and how its pieces need
    /// each other. "What pulled `libxkbcommon` in" is a question about the
    /// dependency graph, which only the database has.
    pub fn installed_package(&self, name: &str) -> Option<Installed> {
        let db = self.alpm.localdb();
        // By name first, then by `provides`: asking why `sh` is installed is a
        // reasonable question, and the answer is `bash`.
        let pkg = db.pkg(name).ok().or_else(|| {
            db.pkgs()
                .into_iter()
                .find(|p| p.provides().into_iter().any(|d| d.name() == name))
        })?;
        let mut required_by: Vec<String> = pkg.required_by().into_iter().collect();
        let mut optional_for: Vec<String> = pkg.optional_for().into_iter().collect();
        required_by.sort();
        optional_for.sort();
        Some(Installed {
            name: pkg.name().to_string(),
            version: pkg.version().to_string(),
            // Assembly step 4 marks the packages the configuration named as
            // explicit, which is what makes `pacman -Qe` on a booted image mean
            // something — and what lets this distinguish "you asked for it"
            // from "something needed it".
            explicit: pkg.reason() == alpm::PackageReason::Explicit,
            asked_for: pkg.name() == name,
            required_by,
            optional_for,
        })
    }

    /// Every package installed in this root, with its version. Used to compare
    /// a built image against its own record.
    pub fn installed(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .alpm
            .localdb()
            .pkgs()
            .into_iter()
            .map(|p| (p.name().to_string(), p.version().to_string()))
            .collect();
        out.sort();
        out
    }
}

fn path_arg(p: &Path) -> Result<&str> {
    p.to_str().ok_or_else(|| Error::Alpm {
        doing: "reading a path",
        message: format!("{} is not valid UTF-8", p.display()),
    })
}

/// One installed package, as `kiln why` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub version: String,
    /// Recorded as explicitly installed: the configuration named it.
    pub explicit: bool,
    /// The query matched this package's own name rather than one of its
    /// `provides`. `kiln why sh` finds `bash`, and the report has to say so
    /// instead of pretending the user asked about `bash`.
    pub asked_for: bool,
    pub required_by: Vec<String>,
    pub optional_for: Vec<String>,
}
