//! The build root.
//!
//! **Privileged.** A build root is an alpm transaction into a directory, so it
//! creates files owned by root with the modes the packages declare — the same
//! reason, unchanged. Ignored by default; `sudo -E cargo test -- --ignored`.
//!
//! Against `tests/repo-fixture`, never the network. The fixture ships a
//! package genuinely called `base-devel`, because every build root needs exactly
//! that name, and a fixture that called it something else would be
//! testing a different code path.

use kiln_alpm::{mirrors, Config, RepoSpec, Session, Trust};
use kiln_build::root::{BuildRoot, Sources, BASE_DEVEL};
use std::path::{Path, PathBuf};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

fn repo() -> PathBuf {
    // Once per test binary, not once per test: the script is idempotent and
    // stamps itself, but two concurrent runs that both decide to rebuild would
    // delete each other's output.
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

fn scratch(name: &str) -> PathBuf {
    let dir = workspace().join("target/test-roots").join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(2)?
                .parse::<u32>()
                .ok()
        })
        == Some(0)
}

/// A state directory with the fixture's metadata already refreshed into it,
/// which is what `Sources::syncdb_from` points at in a real build.
fn sources(state: &Path) -> Sources {
    let repos = vec![RepoSpec::new(
        "fixture",
        vec![mirrors::file(&repo())],
        Trust::Unsigned,
    )];
    let mut session =
        Session::open(Config::for_resolution(state, "x86_64").with_repos(repos.clone()))
            .expect("opening the resolution session");
    session.refresh(true).expect("refreshing the fixture");

    Sources {
        repos,
        arch: "x86_64".into(),
        cache: state.join("cache/pkg"),
        gpgdir: state.join("keyring"),
        syncdb_from: state.join("cache/syncdb"),
    }
}

/// *a build root that contains only `base-devel` plus the resolved
/// `makedepends`, installed from the same pinned repository snapshot as the
/// image itself.*
#[test]
#[ignore = "privileged: installing into a build root needs root"]
fn a_build_root_holds_base_devel_and_what_the_recipe_asked_for() {
    if !is_root() {
        eprintln!("skipped: assembling a build root needs root");
        return;
    }
    let base = scratch("buildroot-basic");
    let sources = sources(&base.join("state"));
    let dir = base.join("root");

    let root = BuildRoot::assemble(&dir, &["fixture-libfoo".to_string()], &[], &sources)
        .expect("assembling the build root");

    let session = Session::open(Config::for_root(&root.dir, "x86_64")).unwrap();
    let installed: Vec<String> = session.installed().into_iter().map(|(n, _)| n).collect();
    assert!(installed.contains(&BASE_DEVEL.to_string()), "{installed:?}");
    assert!(
        installed.contains(&"fixture-libfoo".to_string()),
        "the recipe's own dependency: {installed:?}"
    );
    // The header it would build against is exactly the file the makedepend
    // ships, so a module recipe reading `/usr/lib/modules/*/build` finds it.
    assert!(root.dir.join("usr/include/foo.h").is_file());
    drop(session);

    // The four fixed paths the sandbox uses. The build user is a real
    // unprivileged user — the sandbox drops privileges rather than remapping
    // root onto them — so the directories `makepkg` writes to have to
    // belong to it, or it stops at "BUILDDIR is not writable".
    for path in ["build/recipe", "build/sources", "build/out", "build/work"] {
        let at = root.dir.join(path);
        assert!(at.is_dir(), "{} is missing", at.display());
        assert_eq!(
            owner(&at),
            (kiln_build::build::BUILD_UID, kiln_build::build::BUILD_GID),
            "{} does not belong to the build user",
            at.display()
        );
    }
    root.discard();
}

/// A dependency Kiln built rather than downloaded goes in as a file. This is
/// the AUR-package-depends-on-AUR-package case: no mirror has it, and asking
/// libalpm for it by name would fail with "no package named".
#[test]
#[ignore = "privileged: installing into a build root needs root"]
fn a_prebuilt_dependency_goes_in_from_disk() {
    if !is_root() {
        eprintln!("skipped: assembling a build root needs root");
        return;
    }
    let base = scratch("buildroot-prebuilt");
    let sources = sources(&base.join("state"));

    // Standing in for something realization built a moment ago: a real package
    // archive, handed over as a path and never as a name.
    let artifact = std::fs::read_dir(repo())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("fixture-sysuser-"))
        })
        .expect("fixture-sysuser in the fixture repository");

    let root = BuildRoot::assemble(&base.join("root"), &[], &[artifact], &sources)
        .expect("assembling the build root");

    let session = Session::open(Config::for_root(&root.dir, "x86_64")).unwrap();
    let installed: Vec<String> = session.installed().into_iter().map(|(n, _)| n).collect();
    assert!(
        installed.contains(&"fixture-sysuser".to_string()),
        "the handed-over artifact: {installed:?}"
    );
    drop(session);
    root.discard();
}

/// Assembly step 1's rule, applied here for the same reason: a root left behind by a
/// failed build is a root that inherited state.
#[test]
#[ignore = "privileged: installing into a build root needs root"]
fn a_build_root_is_built_from_nothing_even_when_one_is_already_there() {
    if !is_root() {
        eprintln!("skipped: assembling a build root needs root");
        return;
    }
    let base = scratch("buildroot-fresh");
    let sources = sources(&base.join("state"));
    let dir = base.join("root");

    std::fs::create_dir_all(dir.join("usr/bin")).unwrap();
    std::fs::write(dir.join("usr/bin/left-over"), b"from a previous build\n").unwrap();

    let root = BuildRoot::assemble(&dir, &[], &[], &sources).expect("assembling");
    assert!(
        !root.dir.join("usr/bin/left-over").exists(),
        "the previous build's leftovers survived into this one"
    );
    root.discard();
    assert!(!dir.exists(), "discard leaves nothing behind");
}

#[cfg(unix)]
fn owner(path: &Path) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    (meta.uid(), meta.gid())
}
