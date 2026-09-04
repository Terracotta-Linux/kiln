//! The two-phase build.
//!
//! The split exists for one reason: `makepkg` needs the network to fetch
//! sources, and giving arbitrary build scripts the network makes builds
//! unreproducible and hard to audit. So the thing worth asserting is *which
//! phase has the network*, and that is a property of the specs — checkable
//! without root, a network, or a real PKGBUILD taking four minutes to compile.

use kiln_build::build::{Builder, OUTPUT_DIR, RECIPE_DIR, SOURCE_CACHE_DIR, SOURCE_DIR};
use kiln_build::{srcinfo, Recipe};
use kiln_manifest::Hash;
use kiln_sandbox::{BindMode, Network, SandboxUser};
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/test-roots")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn recipe(dir: &Path) -> Recipe {
    let text = "\
pkgbase = mytool
\tpkgver = 1.2.0
\tpkgrel = 3
\tmakedepends = gcc
\tsource = https://example.invalid/mytool-1.2.0.tar.gz
\tsha256sums = aaaa
\tsource = mytool.patch
\tsha256sums = bbbb
";
    Recipe {
        dir: dir.join("recipe"),
        name: "pkgbuilds/mytool".into(),
        tree: Hash("b3:dd12".into()),
        meta: srcinfo::parse(text, "x86_64").unwrap(),
    }
}

/// The single most important assertion in this crate.
#[test]
fn fetching_has_the_network_and_building_does_not() {
    let dir = scratch("build-network");
    let builder = Builder::new(&dir);
    let recipe = recipe(&dir);

    assert_eq!(
        builder.fetch_spec(&recipe).network,
        Network::Enabled,
        "phase 1 must be able to download sources"
    );
    assert_eq!(
        builder.build_spec(&recipe, &dir.join("root")).network,
        Network::Disabled,
        "phase 2 must not — a PKGBUILD reaching for the network in build() has to fail"
    );
}

/// Elsewhere, builds run as root, and that is about the *image* transaction,
/// where ownership and capabilities must land as packages declare them.
/// `makepkg` is the opposite case: `build()` is a stranger's shell script, and
/// makepkg refuses to run as root anyway.
#[test]
fn both_phases_drop_privileges() {
    let dir = scratch("build-user");
    let builder = Builder::new(&dir);
    let recipe = recipe(&dir);

    for spec in [
        builder.fetch_spec(&recipe),
        builder.build_spec(&recipe, &dir.join("root")),
    ] {
        assert!(
            matches!(spec.user, SandboxUser::Unprivileged { .. }),
            "a PKGBUILD must never run as root"
        );
    }
}

/// A build that could write to the shared source cache could poison every later
/// build on the machine.
#[test]
fn the_source_cache_is_writable_while_fetching_and_read_only_while_building() {
    let dir = scratch("build-source-cache");
    let builder = Builder::new(&dir);
    let recipe = recipe(&dir);

    let mode_of = |spec: &kiln_sandbox::SandboxSpec, target: &str| {
        spec.binds
            .iter()
            .find(|b| b.target == Path::new(target))
            .unwrap_or_else(|| panic!("no bind at {target}"))
            .mode
    };

    assert_eq!(
        mode_of(&builder.fetch_spec(&recipe), SOURCE_DIR),
        BindMode::ReadWrite,
        "phase 1 fills the cache, so SRCDEST is the cache itself"
    );
    // Phase 2 does not mount the cache at `SRCDEST` at all. `makepkg` refuses
    // to start when `$SRCDEST` is not writable — before it looks at whether
    // there is anything to write — so a read-only bind there stops the build
    // with "You do not have write permission for the directory $SRCDEST". What
    // it gets instead is a directory of its own holding symlinks into the
    // cache, which is mounted read-only somewhere else.
    let building = builder.build_spec(&recipe, &dir.join("root"));
    assert_eq!(mode_of(&building, SOURCE_CACHE_DIR), BindMode::ReadOnly);
    assert!(
        !building
            .binds
            .iter()
            .any(|b| b.target == Path::new(SOURCE_DIR)),
        "SRCDEST must belong to the build root, not to the shared cache"
    );
    assert!(
        !building
            .binds
            .iter()
            .any(|b| b.source == builder.source_cache && b.mode == BindMode::ReadWrite),
        "nothing in phase 2 may write to the shared cache"
    );
    // The recipe itself is never writable: a build that edits its own PKGBUILD
    // would make `recipe_tree_hash` — and so `build_key` — a lie.
    assert_eq!(
        mode_of(&builder.build_spec(&recipe, &dir.join("root")), RECIPE_DIR),
        BindMode::ReadOnly
    );
    assert_eq!(
        mode_of(&builder.build_spec(&recipe, &dir.join("root")), OUTPUT_DIR),
        BindMode::ReadWrite
    );
}

/// Phase 1 must not run build code. `--verifysource` fetches and checks;
/// `--nobuild` would extract and run `prepare()`, which is build code running
/// while the network is up — the exact thing the split exists to prevent.
#[test]
fn fetching_verifies_sources_without_running_build_code() {
    let dir = scratch("build-fetch-flags");
    let command = Builder::new(&dir).fetch_spec(&recipe(&dir)).command;
    assert!(
        command.contains(&"--verifysource".to_string()),
        "{command:?}"
    );
    assert!(!command.contains(&"--nobuild".to_string()), "{command:?}");
    assert!(!command.contains(&"--noextract".to_string()), "{command:?}");
}

/// Phase 2 must not try to install anything: its dependencies are already in
/// the build root, and it has no network to fetch them with.
///
/// Integrity is deliberately *not* skipped even though phase 1 checked it.
/// Re-hashing costs milliseconds and catches a corrupted source cache — which
/// is precisely the failure that would otherwise yield a wrong artifact under a
/// right key.
#[test]
fn building_resolves_nothing_and_still_checks_integrity() {
    let dir = scratch("build-build-flags");
    let command = Builder::new(&dir)
        .build_spec(&recipe(&dir), &dir.join("root"))
        .command;
    assert!(command.contains(&"--nodeps".to_string()), "{command:?}");
    assert!(!command.contains(&"--skipinteg".to_string()), "{command:?}");
    assert!(!command.contains(&"--syncdeps".to_string()), "{command:?}");
}

/// a build must not be able to tell what time it is.
#[test]
fn the_build_cannot_see_the_clock() {
    let dir = scratch("build-epoch");
    let spec = Builder::new(&dir).build_spec(&recipe(&dir), &dir.join("root"));
    assert_eq!(
        spec.env.get("SOURCE_DATE_EPOCH").map(String::as_str),
        Some("0")
    );
}

/// A cache hit must skip both phases entirely — no sandbox is even asked to
/// run. This is the single largest speed win in the system.
#[test]
fn a_cache_hit_runs_nothing() {
    let dir = scratch("build-cache-hit");
    let builder = Builder::new(&dir);
    let key = Hash("b3:cc41aa".into());

    let prebuilt = dir.join("mytool-1.2.0-3-x86_64.pkg.tar.zst");
    std::fs::write(&prebuilt, "not really a package\n").unwrap();
    builder.cache.store(&key, &[prebuilt]).unwrap();

    // A sandbox that panics if it is used at all: the assertion is that the
    // build never reaches it.
    struct NeverRuns;
    impl kiln_sandbox::Sandbox for NeverRuns {
        fn name(&self) -> &'static str {
            "never-runs"
        }
        fn argv(&self, _: &kiln_sandbox::SandboxSpec) -> kiln_sandbox::Result<Vec<String>> {
            panic!("a cache hit must not build anything")
        }
        fn run(
            &self,
            _: &kiln_sandbox::SandboxSpec,
        ) -> kiln_sandbox::Result<kiln_sandbox::Outcome> {
            panic!("a cache hit must not build anything")
        }
    }

    let realized = builder
        .realize(&recipe(&dir), &key, &dir.join("root"), &NeverRuns)
        .unwrap();
    assert!(realized.from_cache);
    assert_eq!(realized.artifacts.len(), 1);
}

/// build failure is normal and must be pleasant. The log path belongs in
/// the message, not somewhere the user has to go looking for it.
#[test]
fn a_failed_build_names_the_recipe_the_phase_and_the_log() {
    let dir = scratch("build-failure");
    let builder = Builder::new(&dir);
    std::fs::create_dir_all(dir.join("recipe")).unwrap();

    struct AlwaysFails;
    impl kiln_sandbox::Sandbox for AlwaysFails {
        fn name(&self) -> &'static str {
            "always-fails"
        }
        fn argv(&self, _: &kiln_sandbox::SandboxSpec) -> kiln_sandbox::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn run(
            &self,
            spec: &kiln_sandbox::SandboxSpec,
        ) -> kiln_sandbox::Result<kiln_sandbox::Outcome> {
            Err(kiln_sandbox::Error::Failed {
                command: spec.command.join(" "),
                status: 4,
                stderr: "==> ERROR: A failure occurred in build().".into(),
            })
        }
    }

    let key = Hash("b3:beef".into());
    let err = builder
        .realize(&recipe(&dir), &key, &dir.join("root"), &AlwaysFails)
        .unwrap_err()
        .to_string();

    assert!(err.contains("pkgbuilds/mytool"), "the recipe: {err}");
    assert!(err.contains("fetching sources"), "the phase: {err}");
    assert!(err.contains("==> ERROR"), "makepkg's own words: {err}");
    assert!(err.contains("beef.log"), "the log path: {err}");
}

/// A recipe with only local sources has nothing to fetch, so phase 1 is skipped
/// altogether — and with it the only moment a build has a network.
#[test]
fn a_recipe_with_no_remote_sources_never_opens_the_network() {
    let dir = scratch("build-local-only");
    let text = "\
pkgbase = local-only
\tpkgver = 1
\tpkgrel = 1
\tsource = local.patch
\tsha256sums = aaaa
";
    let recipe = Recipe {
        dir: dir.join("recipe"),
        name: "pkgbuilds/local-only".into(),
        tree: Hash("b3:0001".into()),
        meta: srcinfo::parse(text, "x86_64").unwrap(),
    };
    assert!(recipe.remote_sources().is_empty());

    struct RecordsPhases(std::cell::RefCell<Vec<Network>>);
    impl kiln_sandbox::Sandbox for RecordsPhases {
        fn name(&self) -> &'static str {
            "records"
        }
        fn argv(&self, _: &kiln_sandbox::SandboxSpec) -> kiln_sandbox::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn run(
            &self,
            spec: &kiln_sandbox::SandboxSpec,
        ) -> kiln_sandbox::Result<kiln_sandbox::Outcome> {
            self.0.borrow_mut().push(spec.network);
            Err(kiln_sandbox::Error::Failed {
                command: String::new(),
                status: 1,
                stderr: String::new(),
            })
        }
    }

    let sandbox = RecordsPhases(std::cell::RefCell::new(Vec::new()));
    let _ = Builder::new(&dir).realize(
        &recipe,
        &Hash("b3:0002".into()),
        &dir.join("root"),
        &sandbox,
    );
    assert_eq!(
        sandbox.0.into_inner(),
        [Network::Disabled],
        "only the build phase should have run, and with no network"
    );
}
