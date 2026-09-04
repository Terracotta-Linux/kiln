//! The two-phase build.
//!
//! > The core problem: `makepkg` needs the network to fetch sources, and giving
//! > arbitrary build scripts the network makes builds unreproducible and hard to
//! > audit.
//!
//! **Phase 1 — fetch. Network on.** `makepkg --verifysource` downloads every
//! `source=()` entry and checks it against the recipe's own `sha256sums`.
//! Sources land in the content-addressed source cache.
//!
//! **Phase 2 — build. Network off.** A fresh sandbox with `CLONE_NEWNET` and no
//! interfaces at all. The sources are bind-mounted from the cache read-only,
//! and `makepkg` runs as an unprivileged user against a build root holding only
//! `base-devel` plus the resolved `makedepends`.
//!
//! A PKGBUILD that reaches for the network in `build()` fails, loudly. That is
//! the feature, and it is why the two specs below are built by separate
//! functions that a test can inspect one at a time.

use crate::cache::{Cache, Lookup};
use crate::recipe::Recipe;
use crate::SourcePin;
use kiln_manifest::Hash;
use kiln_sandbox::{Bind, Network, Sandbox, SandboxSpec, SandboxUser};
use std::fmt;
use std::path::{Path, PathBuf};

/// Where things live inside a build sandbox. Fixed paths rather than generated
/// ones, so a build log is the same on every machine and a `--keep-failed`
/// sandbox is navigable without a map.
pub const RECIPE_DIR: &str = "/build/recipe";
/// `SRCDEST`. In phase 1 this *is* the shared source cache, mounted writable,
/// because fetching is what fills it. In phase 2 it is a directory of the build
/// root's own, holding a symlink per source into `SOURCE_CACHE_DIR`.
///
/// The two-step exists because `makepkg` refuses to start when `$SRCDEST` is
/// not writable — it checks before it looks at whether there is anything to
/// write — and a writable shared cache in phase 2 is a build that can poison
/// every later build on the machine. Symlinks satisfy the check without
/// handing over the bytes: the directory is the build's, the sources are not.
pub const SOURCE_DIR: &str = "/build/sources";
/// The shared source cache, read-only, phase 2 only.
pub const SOURCE_CACHE_DIR: &str = "/build/source-cache";
pub const OUTPUT_DIR: &str = "/build/out";
pub const WORK_DIR: &str = "/build/work";

/// The unprivileged user a build runs as.
///
/// Elsewhere, builds run as root — that is about the *image* transaction, where
/// ownership and capabilities must land exactly as packages declare them.
/// `makepkg` is the opposite case: it refuses to run as root, and for once that
/// is the behaviour Kiln wants, because `build()` is a stranger's shell script.
pub const BUILD_UID: u32 = 1000;
pub const BUILD_GID: u32 = 1000;

pub struct Builder {
    pub cache: Cache,
    /// `<state_dir>/cache/src`: sources, shared across every recipe.
    pub source_cache: PathBuf,
    /// Where a build root is assembled and a sandbox's scratch lives.
    pub work_dir: PathBuf,
}

impl Builder {
    pub fn new(state_dir: impl AsRef<Path>) -> Builder {
        let state = state_dir.as_ref();
        Builder {
            cache: Cache::new(state),
            source_cache: state.join("cache/src"),
            work_dir: state.join("build"),
        }
    }

    /// **Phase 1.** Fetch and verify. The only step in a build with a network.
    ///
    /// `makepkg --verifysource` is deliberately not `--nobuild`: the latter
    /// extracts and runs `prepare()`, which is build code, and the whole point
    /// of the split is that no build code runs while the network is up. The one
    /// exception the design allows is `pkgver()`, which makepkg runs for VCS
    /// sources and which is why those packages are volatile.
    pub fn fetch_spec(&self, recipe: &Recipe) -> SandboxSpec {
        let spec = SandboxSpec::in_root(
            "/",
            [
                "makepkg".into(),
                "--verifysource".into(),
                "--noconfirm".into(),
            ],
        )
        .with_network(Network::Enabled)
        .with_bind(Bind::ro(&recipe.dir, RECIPE_DIR))
        .with_bind(Bind::rw(&self.source_cache, SOURCE_DIR))
        .with_user(SandboxUser::Unprivileged {
            uid: BUILD_UID,
            gid: BUILD_GID,
        })
        .with_env("SRCDEST", SOURCE_DIR)
        .with_env("BUILDDIR", WORK_DIR)
        // The default is `/root`, which the build user cannot write to now that
        // it is a real unprivileged user rather than a remapped root. `makepkg`
        // and the tools it calls treat `$HOME` as scratch.
        .with_env("HOME", WORK_DIR);
        SandboxSpec {
            workdir: Some(PathBuf::from(RECIPE_DIR)),
            ..spec
        }
    }

    /// **Phase 2.** Build, with no network at all.
    ///
    /// `root` is a build root already holding `base-devel` and the resolved
    /// `makedepends`, installed from the same repository snapshot as the image
    /// itself — so the toolchain a package is built against is the toolchain
    /// recorded in its `build_key`.
    ///
    /// Integrity is **not** skipped here even though phase 1 already checked
    /// it. Re-hashing costs milliseconds and catches a corrupted source cache,
    /// which is exactly the failure that would otherwise produce a wrong
    /// artifact under a right key.
    pub fn build_spec(&self, recipe: &Recipe, root: &Path) -> SandboxSpec {
        let spec = SandboxSpec::in_root(
            root,
            [
                "makepkg".into(),
                // Dependencies are already in the root; makepkg must not try to
                // install anything, which would need a network it does not have.
                "--nodeps".into(),
                "--noconfirm".into(),
            ],
        )
        // Explicit, though `in_root` already defaults this way: this makes the
        // absent network the constraint the rest of the model rests on, and
        // this is the single most important line in the file.
        .with_network(Network::Disabled)
        .with_bind(Bind::ro(&recipe.dir, RECIPE_DIR))
        // Read-only: a build that could write to the shared source cache could
        // poison every later build on the machine. `SRCDEST` points at the
        // build root's own directory of symlinks into it — see `SOURCE_DIR`.
        .with_bind(Bind::ro(&self.source_cache, SOURCE_CACHE_DIR))
        .with_bind(Bind::rw(self.output_dir(recipe), OUTPUT_DIR))
        .with_user(SandboxUser::Unprivileged {
            uid: BUILD_UID,
            gid: BUILD_GID,
        })
        .with_env("SRCDEST", SOURCE_DIR)
        .with_env("PKGDEST", OUTPUT_DIR)
        .with_env("BUILDDIR", WORK_DIR)
        // a build must not be able to tell what time it is.
        .with_env("SOURCE_DATE_EPOCH", "0")
        .with_env("HOME", WORK_DIR)
        .with_env("PACKAGER", "Kiln <kiln@localhost>");
        SandboxSpec {
            workdir: Some(PathBuf::from(RECIPE_DIR)),
            ..spec
        }
    }

    fn output_dir(&self, recipe: &Recipe) -> PathBuf {
        self.work_dir.join(&recipe.meta.pkgbase).join("out")
    }

    /// Put one symlink per fetched source into the build root's `SRCDEST`,
    /// pointing into the read-only cache.
    ///
    /// The link targets are paths *inside the sandbox*, so they only resolve
    /// there — which is the point: from the host they are dangling, and from
    /// inside they resolve onto a read-only mount. `makepkg` follows them for
    /// every test it makes (`-f`, `-d`) and copies out of them, and cannot
    /// write through them.
    fn link_sources(&self, recipe: &Recipe, root: &Path) -> Result<(), Error> {
        let dir = root.join(SOURCE_DIR.trim_start_matches('/'));
        std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
            doing: "preparing the build root's source directory",
            path: dir.clone(),
            source,
        })?;
        for source in recipe.remote_sources() {
            let name = source.filename();
            let link = dir.join(name);
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(format!("{SOURCE_CACHE_DIR}/{name}"), &link).map_err(
                |source| Error::Io {
                    doing: "linking a fetched source into",
                    path: link.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }

    /// Build a recipe, or return the cached artifacts if this exact build has
    /// happened before.
    ///
    /// The cache check comes first and is the point: this is the single
    /// largest speed win in the system, because rebuilding an image after
    /// changing one line of `system.toml` must not rebuild an out-of-tree
    /// NVIDIA module.
    pub fn realize(
        &self,
        recipe: &Recipe,
        key: &Hash,
        root: &Path,
        sandbox: &dyn Sandbox,
    ) -> Result<Realized, Error> {
        if let Lookup::Hit(artifacts) = self.cache.lookup(key) {
            return Ok(Realized {
                artifacts,
                from_cache: true,
                sources: Vec::new(),
            });
        }

        // Both belong to the build user: it writes the finished packages into
        // one and the fetched sources into the other, as itself — the one
        // exception — through a bind mount that carries the host's ownership
        // straight through.
        let output = self.output_dir(recipe);
        for dir in [&output, &self.source_cache] {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                doing: "preparing a build directory",
                path: dir.clone(),
                source,
            })?;
            crate::root::own(dir).map_err(|e| Error::Io {
                doing: "preparing a build directory",
                path: dir.clone(),
                source: std::io::Error::other(e.to_string()),
            })?;
        }

        // the full log is always written, and its path is printed on
        // failure. Both phases append to the one file, because the story of a
        // build is both of them.
        let log = self.cache.log_path(key);
        let _ = std::fs::remove_file(&log);

        if !recipe.remote_sources().is_empty() {
            sandbox
                .run(&self.fetch_spec(recipe).logging_to(&log))
                .map_err(|source| Error::Phase {
                    phase: "fetching sources",
                    recipe: recipe.name.clone(),
                    log: self.cache.log_path(key),
                    source,
                })?;
        }

        self.link_sources(recipe, root)?;
        sandbox
            .run(&self.build_spec(recipe, root).logging_to(&log))
            .map_err(|source| Error::Phase {
                phase: "building",
                recipe: recipe.name.clone(),
                log: self.cache.log_path(key),
                source,
            })?;

        let built = artifacts_in(&output).map_err(|source| Error::Io {
            doing: "collecting the built packages from",
            path: output.clone(),
            source,
        })?;
        if built.is_empty() {
            return Err(Error::NothingBuilt {
                recipe: recipe.name.clone(),
                looked_in: output,
            });
        }

        let artifacts = self.cache.store(key, &built).map_err(|source| Error::Io {
            doing: "storing the built packages in the cache",
            path: self.cache.log_path(key),
            source,
        })?;

        Ok(Realized {
            artifacts,
            from_cache: false,
            sources: pins(recipe),
        })
    }
}

/// What `realize` produced, and whether it had to do any work.
#[derive(Debug, Clone)]
pub struct Realized {
    pub artifacts: Vec<PathBuf>,
    /// worth reporting. "Nothing to do" is a success, and a user who
    /// waited zero seconds deserves to know why.
    pub from_cache: bool,
    pub sources: Vec<SourcePin>,
}

/// The source pins that go in the plan and the build record.
fn pins(recipe: &Recipe) -> Vec<SourcePin> {
    let mut out: Vec<SourcePin> = recipe
        .meta
        .sources
        .iter()
        .filter_map(|s| {
            Some(SourcePin {
                url: s.spec.clone(),
                sha256: s.sha256.clone()?,
            })
        })
        .collect();
    out.sort();
    out
}

fn artifacts_in(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".pkg.tar.zst"))
        .collect();
    out.sort();
    Ok(out)
}

#[derive(Debug)]
pub enum Error {
    /// build failure is normal and must be pleasant. The log path is
    /// part of the message, not something to go looking for.
    Phase {
        phase: &'static str,
        recipe: String,
        log: PathBuf,
        source: kiln_sandbox::Error,
    },
    /// makepkg exited successfully and produced no package. Almost always a
    /// `package()` that installed nothing.
    NothingBuilt { recipe: String, looked_in: PathBuf },
    Io {
        doing: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Phase {
                phase,
                recipe,
                log,
                source,
            } => write!(
                f,
                "`{recipe}` failed while {phase}\n{source}\n\nfull log: {}",
                log.display()
            ),
            Error::NothingBuilt { recipe, looked_in } => write!(
                f,
                "`{recipe}` built successfully but produced no package — check that its \
                 `package()` installs something into $pkgdir (looked in {})",
                looked_in.display()
            ),
            Error::Io {
                doing,
                path,
                source,
            } => {
                write!(f, "{doing} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {}
