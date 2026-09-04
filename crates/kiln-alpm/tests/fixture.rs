//! Shared setup for the fixture-backed tests. Solver and
//! transaction tests use `tests/repo-fixture` — a real tiny local pacman repo
//! built in-tree — never the network.
//!
//! Shared by every test binary in this crate, each of which compiles its own
//! copy — so anything one binary does not call looks dead to that binary.
#![allow(dead_code)]

use kiln_alpm::{mirrors, Config, RepoSpec, Session, Trust};
use std::path::{Path, PathBuf};

/// The workspace root, found from this crate rather than from the working
/// directory, which cargo does not promise.
fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

/// Build the fixture repository if it is not current, and return its path.
/// The script is idempotent and stamps itself, so this costs nothing after the
/// first run — but tests run in parallel threads, so the build happens exactly
/// once per test binary rather than once per test.
pub fn repo() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = workspace().join("tests/repo-fixture");
    ONCE.call_once(|| {
        let out = std::process::Command::new(root.join("build.sh"))
            .output()
            .expect("tests/repo-fixture/build.sh must be runnable");
        assert!(
            out.status.success(),
            "the fixture repository failed to build:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    });
    root.join("repo")
}

/// A scratch root under `target/`, wiped first so a test never inherits the
/// last run's state. `target/` is already ignored by git and survives a
/// failure, which is what you want when a solver test disagrees with you.
pub fn scratch(name: &str) -> PathBuf {
    let dir = workspace().join("target/test-roots").join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A session over a **staging root** rather than a resolution root: packages
/// installed here land in `usr/lib/sysimage/pacman` and on disk, which is what
/// assembly does.
pub fn staging(name: &str) -> Session {
    staging_inner(name, None)
}

/// A staging session with an extra `HookDir` after the default one — the only
/// lever there is over package-shipped hooks.
pub fn staging_with_hookdir(name: &str, hooks: &Path) -> Session {
    staging_inner(name, Some(hooks))
}

fn staging_inner(name: &str, hooks: Option<&Path>) -> Session {
    let repo_dir = repo();
    let base = scratch(name);
    let root = base.join("root");
    let mut cfg = Config::for_root(&root, "x86_64")
        .with_cache(base.join("cache"))
        .with_repos(vec![RepoSpec::new(
            "fixture",
            vec![mirrors::file(&repo_dir)],
            Trust::Unsigned,
        )]);
    if let Some(dir) = hooks {
        // libalpm scans /usr/share/libalpm/hooks unconditionally; a registered
        // hookdir is *additional*, and later wins by filename.
        cfg = cfg.with_hookdir(dir);
    }
    let mut s = Session::open(cfg).expect("opening the staging session");
    s.refresh(true).expect("refreshing the fixture database");
    s
}

/// A session over the fixture repository, refreshed and ready to solve.
/// `subdir` selects `repo` or `repo/next` — the latter is the same package set
/// with `fixture-libfoo` upgraded, which is what change detection needs.
pub fn session(name: &str, subdir: &str) -> Session {
    let repo_dir = repo().join(subdir);
    let root = scratch(name);
    let cfg = Config::for_root(&root, "x86_64")
        .with_cache(root.join("cache"))
        .with_repos(vec![RepoSpec::new(
            "fixture",
            // The fixture is unsigned on purpose: signing it would mean
            // shipping a private key in the repository, and the thing under
            // test here is the solver, not GPG.
            vec![mirrors::file(&repo_dir)],
            Trust::Unsigned,
        )]);
    let mut s = Session::open(cfg).expect("opening the fixture session");
    s.refresh(true).expect("refreshing the fixture database");
    s
}
