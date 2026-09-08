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

/// Where phase 2 lives inside its build root. Fixed paths rather than
/// generated ones, so a build log is the same on every machine and a
/// `--keep-failed` sandbox is navigable without a map. Safe to invent
/// unconditionally here because `BuildRoot::assemble` creates them itself
/// before bubblewrap is asked to mount over them (see `root::INSIDE`) — the
/// root is a directory Kiln owns.
pub const RECIPE_DIR: &str = "/build/recipe";
/// `SRCDEST` in phase 2: a directory of the build root's own, holding a
/// symlink per source into `SOURCE_CACHE_DIR`.
///
/// Read-only would be simpler, but `makepkg` refuses to start when `$SRCDEST`
/// is not writable — it checks before it looks at whether there is anything
/// to write — and a writable shared cache in phase 2 is a build that can
/// poison every later build on the machine. Symlinks satisfy the check
/// without handing over the bytes: the directory is the build's, the sources
/// are not.
pub const SOURCE_DIR: &str = "/build/sources";
/// The shared source cache, read-only, phase 2 only.
pub const SOURCE_CACHE_DIR: &str = "/build/source-cache";
pub const OUTPUT_DIR: &str = "/build/out";
pub const WORK_DIR: &str = "/build/work";

/// Where phase 1 mounts things, inside the *live* root rather than a build
/// root Kiln owns — see `fetch_spec`. Two constraints shape these paths, and
/// each of them has been a shipped bug.
///
/// `/build` cannot be reused here: unlike `BuildRoot::assemble`, nothing
/// pre-creates a `/build` on the live root, and on an OSTree-deployed system
/// that root is immutable (`chattr +i`), so bubblewrap's own attempt to create
/// the mountpoint fails with `Can't mkdir parents for /build/recipe: Operation
/// not permitted` — not a permissions problem `sudo` can fix, since the flag
/// rejects the write regardless of privilege. `/tmp` always exists on the live
/// root, and `with_bind`'s `Bind::kernel_filesystems` default already remounts
/// it as a private tmpfs — 1777, for an unprivileged command — before any of
/// these binds are processed.
///
/// **And they are flat: exactly one component under `/tmp`, never
/// `/tmp/kiln-live/work`.** Bubblewrap creates the *target* of a bind, but it
/// creates that target's missing *parents* as mode 0700 owned by whoever runs
/// bubblewrap — root. Phase 1 then drops to `BUILD_UID`, which cannot traverse
/// a 0700 root-owned directory, so every absolute path underneath is
/// unreachable however the bind mount itself is set up: `makepkg` stops at
/// "Failed to create the directory $BUILDDIR" — its `mkdir -p` gets `EACCES`,
/// so the message names creation but the fault is the parent — before it ever
/// reaches a PKGBUILD. A bind mounted straight onto a child of the 1777 tmpfs
/// has no such parent, and takes its *source's* ownership.
///
/// `--chdir` is why this hid behind two other fixes: bubblewrap chdirs while
/// still root, so a command that only uses paths relative to its workdir —
/// `makepkg --printsrcinfo` in `recipe::generate_srcinfo` — runs happily from
/// inside a directory it could not have reached by name.
///
/// `LIVE_ROOT_BUILD_DIR` also needs a real bind mount, not just an env var:
/// bubblewrap creates the targets of the binds it is given and nothing else,
/// so a bare `--tmpfs /tmp` never produces a `$BUILDDIR` at all.
/// `Builder::fetch_work_dir` is the directory behind it.
pub const LIVE_ROOT_RECIPE_DIR: &str = "/tmp/kiln-live-recipe";
pub const LIVE_ROOT_SOURCE_DIR: &str = "/tmp/kiln-live-sources";
pub const LIVE_ROOT_BUILD_DIR: &str = "/tmp/kiln-live-work";

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
        .with_bind(Bind::ro(&recipe.dir, LIVE_ROOT_RECIPE_DIR))
        .with_bind(Bind::rw(&self.source_cache, LIVE_ROOT_SOURCE_DIR))
        .with_bind(Bind::rw(self.fetch_work_dir(recipe), LIVE_ROOT_BUILD_DIR))
        .with_user(SandboxUser::Unprivileged {
            uid: BUILD_UID,
            gid: BUILD_GID,
        })
        .with_env("SRCDEST", LIVE_ROOT_SOURCE_DIR)
        .with_env("BUILDDIR", LIVE_ROOT_BUILD_DIR)
        // Every directory makepkg might write to is named here, and the
        // invariant is what matters more than any one of them: nothing in
        // phase 1 may be left to default, because the default is `$startdir` —
        // the recipe directory, bound read-only precisely so that a fetch
        // cannot edit the PKGBUILD its `build_key` was computed from.
        //
        // `PKGDEST` is the one that bites: makepkg checks it for writability
        // up front, before it works out that this run will not produce a
        // package, so `--verifysource` stops at "You do not have write
        // permission for the directory $PKGDEST" without fetching anything.
        // `SRCPKGDEST` and `LOGDEST` are only checked under `--source` and
        // `-L`, which phase 1 does not pass — they are set so that stays a
        // fact about makepkg's flags rather than something this spec depends
        // on. All three point at the scratch phase 1 is allowed to dirty; the
        // real artifacts are phase 2's `OUTPUT_DIR`, which phase 1 must not be
        // able to write into at all.
        .with_env("PKGDEST", LIVE_ROOT_BUILD_DIR)
        .with_env("SRCPKGDEST", LIVE_ROOT_BUILD_DIR)
        .with_env("LOGDEST", LIVE_ROOT_BUILD_DIR)
        // The default is `/root`, which the build user cannot write to now that
        // it is a real unprivileged user rather than a remapped root. `makepkg`
        // and the tools it calls treat `$HOME` as scratch.
        .with_env("HOME", LIVE_ROOT_BUILD_DIR);
        SandboxSpec {
            workdir: Some(PathBuf::from(LIVE_ROOT_RECIPE_DIR)),
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

    /// `BUILDDIR`'s real backing directory for phase 1 — see
    /// `LIVE_ROOT_BUILD_DIR`. Per-recipe, like `output_dir`, so two recipes
    /// realized one after another never share scratch state.
    fn fetch_work_dir(&self, recipe: &Recipe) -> PathBuf {
        self.work_dir.join(&recipe.meta.pkgbase).join("fetch-work")
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

        // All three belong to the build user: it writes the finished packages
        // into the first, the fetched sources into the second, and phase 1's
        // `$BUILDDIR`/`$HOME` scratch into the third, as itself — the one
        // exception — through a bind mount that carries the host's ownership
        // straight through.
        let output = self.output_dir(recipe);
        let fetch_work = self.fetch_work_dir(recipe);
        // Both are scratch this build owns exclusively, keyed only by
        // `pkgbase` rather than `key` — so a `.pkg.tar.zst` left behind by an
        // earlier attempt at a different key (a changed version, a changed
        // dependency closure) survives into this one otherwise. Left in
        // place, it either makes `makepkg` refuse to run at all ("A package
        // has already been built") or, worse, gets swept up by
        // `artifacts_in` below and stored into the cache as this build's
        // output though nothing this build did produced it.
        for dir in [&output, &fetch_work] {
            let _ = std::fs::remove_dir_all(dir);
        }
        for dir in [&output, &self.source_cache, &fetch_work] {
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
