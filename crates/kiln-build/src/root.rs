//! The build root.
//!
//! > `makepkg --noextract --nodeps` runs as an unprivileged build user against
//! > a build root that contains only `base-devel` plus the resolved
//! > `makedepends`, installed from the same pinned repository snapshot as the
//! > image itself.
//!
//! One root per `build_key`, not one shared by every recipe in a build. A
//! shared root is faster and wrong in a way that only shows up later: a recipe
//! with an under-declared dependency builds fine because some *other* recipe
//! happened to pull the missing package in, and then fails on a machine where
//! the two are built in the other order — or, worse, links against something
//! its `build_key` does not mention, which is exactly the silently-wrong
//! artifact exists to prevent.
//!
//! What goes in is `base-devel`, the recipe's `makedepends` and
//! `checkdepends` — and its `depends` too. Convention names only the first two, but
//! a build that cannot link against its own runtime dependencies is not a
//! build; `makepkg --nodeps` will not install them, and there is no network to
//! install them from. `Ingredients::makedeps` carries the same set, so what a
//! package was compiled against is what its key records.

use kiln_alpm::{Config, Mounts, RepoSpec, Session, Transaction};
use std::fmt;
use std::path::{Path, PathBuf};

/// A real package in current Arch — it was a package *group* until 2022,
/// which `find_satisfier` would not resolve.
pub const BASE_DEVEL: &str = "base-devel";

/// Where the two-phase build's fixed paths are rooted inside the sandbox. The
/// directories have to exist in the root before bubblewrap is asked to mount
/// over them, and `makepkg` needs `BUILDDIR` to be somewhere it can write.
const INSIDE: [&str; 4] = [
    crate::build::RECIPE_DIR,
    crate::build::SOURCE_DIR,
    crate::build::OUTPUT_DIR,
    crate::build::WORK_DIR,
];

/// What a build root needs to know about the world it is installed from.
///
/// The repositories are the image's own, deliberately: this makes "the same
/// repository snapshot as the image" the property that keeps a build key
/// honest, and a build root resolved from anywhere else would record a
/// toolchain the image does not contain.
#[derive(Debug, Clone)]
pub struct Sources {
    pub repos: Vec<RepoSpec>,
    pub arch: String,
    /// The shared package cache.
    pub cache: PathBuf,
    /// Kiln's own pacman keyring.
    pub gpgdir: PathBuf,
    /// The resolution session's database directory, whose `sync/` already holds
    /// the repository metadata. Copied in rather than refreshed, exactly as
    /// assembly does it (step 4).
    pub syncdb_from: PathBuf,
}

pub struct BuildRoot {
    pub dir: PathBuf,
}

impl BuildRoot {
    /// Install `base-devel` plus `wanted` into a fresh directory.
    ///
    /// `artifacts` are `.pkg.tar.zst` files that must go in from disk rather
    /// than from a repository — an AUR package that another AUR package
    /// build-depends on, which by definition no mirror has.
    ///
    /// Needs root, for the same reason every other transaction does:
    /// as an ordinary user libalpm fails every `chown`, logs a warning, and
    /// reports success.
    pub fn assemble(
        dir: &Path,
        wanted: &[String],
        artifacts: &[PathBuf],
        sources: &Sources,
    ) -> Result<BuildRoot, Error> {
        // Assembly step 1's rule, for the same reason: a root left behind by a
        // failed build is a root that inherited state.
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            doing: "creating the build root",
            path: dir.to_path_buf(),
            source,
        })?;
        import_sync_databases(dir, &sources.syncdb_from)?;

        let mut names: Vec<String> = vec![BASE_DEVEL.to_string()];
        names.extend(wanted.iter().cloned());
        names.sort();
        names.dedup();

        let transaction = Transaction::new(names).with_locals(artifacts.to_vec());

        // The network, once, and only here. `makepkg` itself never gets to
        // install anything: the two-phase build's phase 2 runs `--nodeps` in a namespace with
        // no interfaces at all.
        let config = Config::for_root(dir, &sources.arch)
            .with_repos(sources.repos.clone())
            .with_cache(&sources.cache)
            .with_gpgdir(&sources.gpgdir);
        let mut session = Session::open(config).map_err(Error::Alpm)?;
        session.fetch(&transaction).map_err(Error::Alpm)?;

        {
            // `base-devel` drags in `pacman` and `systemd`, so a build root
            // runs the same scriptlets and hooks a staging root does — and
            // libalpm runs them chrooted, where `/proc` is an empty directory
            // until this. Without it the install dies on
            // `21-systemd-tmpfiles.hook`, several hundred megabytes in, with a
            // message about a catalog file (see `kiln_alpm::mounts`).
            //
            // The guard unmounts on the error path too, which matters more here
            // than in assembly: a failed build root is deleted, and
            // `remove_dir_all` over a live `/proc` bind mount is not a thing to
            // find out about afterwards.
            let _mounted = Mounts::setup(dir).map_err(Error::Alpm)?;
            provide_hook_directories(dir)?;
            session.install(&transaction).map_err(Error::Alpm)?;
        }
        drop(session);

        for path in INSIDE {
            let at = dir.join(path.trim_start_matches('/'));
            std::fs::create_dir_all(&at).map_err(|source| Error::Io {
                doing: "creating a build directory",
                path: at.clone(),
                source,
            })?;
            // `makepkg` runs unprivileged — the sandbox's one exception — and genuinely as
            // that user — the sandbox drops privileges rather than remapping
            // root onto them — so the directories it writes to have to belong
            // to it. `BUILDDIR` is the one that is not a bind mount, and
            // without this makepkg stops at "BUILDDIR is not writable".
            own(&at)?;
        }
        Ok(BuildRoot {
            dir: dir.to_path_buf(),
        })
    }

    /// Delete the root. A build root is several hundred megabytes and is worth
    /// nothing once the artifact is in the cache.
    pub fn discard(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Hand a directory to the build user.
///
/// `chown`, not `chmod 0777`: the build user is a real user now, and a
/// world-writable directory inside a root that also runs package scriptlets is
/// a wider statement than the one being made.
pub fn own(path: &Path) -> Result<(), Error> {
    let out = std::process::Command::new("chown")
        .arg(format!(
            "{}:{}",
            crate::build::BUILD_UID,
            crate::build::BUILD_GID
        ))
        .arg(path)
        .output()
        .map_err(|source| Error::Io {
            doing: "running chown for",
            path: path.to_path_buf(),
            source,
        })?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Io {
        doing: "giving the build user",
        path: path.to_path_buf(),
        source: std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    })
}

/// Directories a package hook writes a generated cache into, and that nothing
/// in a fresh root creates.
///
/// The same list assembly keeps, and for the same reason: Kiln does not run
/// `systemd-tmpfiles` over a root it is building, so the hooks that *would*
/// have created these have nothing to stand on. `journalctl --update-catalog`
/// is the one that makes it visible, and it fails naming its *input* — a file
/// that is present and readable — rather than the directory it cannot write.
const HOOK_OUTPUT_DIRS: &[&str] = &["var/lib/systemd/catalog"];

fn provide_hook_directories(root: &Path) -> Result<(), Error> {
    for dir in HOOK_OUTPUT_DIRS {
        let at = root.join(dir);
        std::fs::create_dir_all(&at).map_err(|source| Error::Io {
            doing: "creating a hook output directory",
            path: at,
            source,
        })?;
    }
    Ok(())
}

/// Copy the resolution session's `sync/` into the root's database directory.
///
/// The same move assembly makes, and for the same reason: libalpm cannot find a
/// package by name without sync databases, and refreshing them here would mean
/// resolving the build root against mirrors as they stand *now* rather than
/// against the snapshot the plan was resolved from.
fn import_sync_databases(root: &Path, from: &Path) -> Result<(), Error> {
    let to = root.join(kiln_alpm::session::DB_PATH).join("sync");
    std::fs::create_dir_all(&to).map_err(|source| Error::Io {
        doing: "creating the build root's database directory",
        path: to.clone(),
        source,
    })?;
    let entries = std::fs::read_dir(from.join("sync")).map_err(|source| Error::Io {
        doing: "reading the resolved repository metadata at",
        path: from.join("sync"),
        source,
    })?;
    let mut copied = 0;
    for entry in entries.flatten() {
        // Files only: a `sync/` directory holds `core.db` and its siblings, and
        // anything else in there is not a database.
        if !entry.path().is_file() {
            continue;
        }
        let target = to.join(entry.file_name());
        std::fs::copy(entry.path(), &target).map_err(|source| Error::Io {
            doing: "copying repository metadata to",
            path: target,
            source,
        })?;
        copied += 1;
    }
    if copied == 0 {
        return Err(Error::NoMetadata {
            looked_in: from.join("sync"),
        });
    }
    Ok(())
}

#[derive(Debug)]
pub enum Error {
    Alpm(kiln_alpm::Error),
    /// No repository databases to resolve the build root against. Resolution
    /// refreshes them and realization copies them in; going online here instead
    /// would resolve the build root against the mirrors as they stand *now*
    /// rather than against the snapshot the plan came from.
    NoMetadata {
        looked_in: PathBuf,
    },
    Io {
        doing: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Alpm(e) => write!(
                f,
                "the build root could not be assembled: {e}\n\n\
                 It holds `{BASE_DEVEL}` plus the recipe's own dependencies, resolved from \
                 the same repositories as the image."
            ),
            Error::NoMetadata { looked_in } => write!(
                f,
                "no repository databases in {}: resolution has not refreshed them, and a \
                 build root is resolved from the same snapshot as the image rather than \
                 from the mirrors as they stand now",
                looked_in.display()
            ),
            Error::Io {
                doing,
                path,
                source,
            } => write!(f, "{doing} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}
