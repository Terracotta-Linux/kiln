//! Turning a snippet of TOML into a resolved plan, against the fixture
//! repository. never the network.
//!
//! Shared by every test binary in this crate, each of which compiles its own
//! copy — so anything one binary does not call looks dead to that binary.
#![allow(dead_code)]

use kiln_alpm::{mirrors, RepoSpec, Trust};
use kiln_manifest::Manifest;
use kiln_resolve::{BuildPlan, Inputs, Options};
use std::path::{Path, PathBuf};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

fn fixture_repo() -> PathBuf {
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

pub fn scratch(name: &str) -> PathBuf {
    let dir = workspace().join("target/test-roots").join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The smallest configuration that resolves: a kernel and an init, from the
/// fixture. Tests append to it rather than restating it.
pub const BOOTABLE: &str = r#"
kiln = 1

[image]
name = "fixture"
arch = "x86_64"

[kernel]
package = "fixture-linux"

[packages]
repo = ["fixture-linux", "fixture-init"]
"#;

/// Write `toml` as `system.toml` alongside `extra` files, run the whole
/// frontend over it, and return the Manifest.
///
/// Going through the real frontend rather than building a Manifest by hand is
/// deliberate: it is what keeps `local_digests` and `item_origins` — and so the
/// identities and diagnostics these tests assert on — honest.
pub fn manifest_with(name: &str, toml: &str, extra: &[(&str, &str)]) -> (Manifest, PathBuf) {
    let dir = scratch(name);
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("system.toml"), toml).unwrap();
    for (path, body) in extra {
        let at = config.join(path);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, body).unwrap();
    }

    let fe =
        kiln_config::load(Some(&config), &kiln_config::Options::default()).unwrap_or_else(|e| {
            panic!(
                "the test configuration did not validate:\n{}",
                kiln_diag::render_all(&e)
            )
        });
    (fe.manifest, dir)
}

pub fn manifest(name: &str, toml: &str) -> (Manifest, PathBuf) {
    manifest_with(name, toml, &[])
}

/// The fixture repository, registered as `fixture`. Unsigned on purpose: what
/// is under test is resolution, not GPG.
pub fn repos(subdir: &str) -> Vec<RepoSpec> {
    vec![RepoSpec::new(
        "fixture",
        vec![mirrors::file(&fixture_repo().join(subdir))],
        Trust::Unsigned,
    )]
}

/// Resolution options against one variant of the fixture repository.
///
/// The state directory is **keyed on `subdir`**, and that is not tidiness. The
/// sync databases libalpm downloads live under it and persist by design (
/// metadata `kiln check` re-reads constantly and re-downloads rarely), and the
/// two fixture repositories are both registered under the name `fixture`. A
/// shared state directory therefore means the second resolution reads the first
/// one's database and reports the packages of a repository it never looked at —
/// which made "a moved mirror changes `plan_id`" fail while the mirror had, in
/// fact, moved.
pub fn options(dir: &Path, subdir: &str) -> Options {
    let state = dir.join("state").join(subdir.replace('.', "current"));
    Options::new(state).with_repos(repos(subdir))
}

/// An AUR transport with nothing recorded in it. A configuration with no
/// `packages.aur` never reaches it, so a test that unexpectedly starts asking
/// the AUR fails loudly rather than going to the network.
pub fn no_aur() -> kiln_aur::Recorded {
    kiln_aur::Recorded::new()
}

/// Resolve, expecting success.
pub fn plan(name: &str, toml: &str) -> BuildPlan {
    try_plan(name, toml)
        .unwrap_or_else(|e| panic!("resolution failed:\n{}", kiln_diag::render_all(&e)))
}

pub fn try_plan(name: &str, toml: &str) -> Result<BuildPlan, kiln_diag::Errors> {
    let (m, dir) = manifest(name, toml);
    kiln_resolve::resolve(
        &m,
        &dir.join("config"),
        &options(&dir, "."),
        &Inputs::new(&no_aur()),
    )
}

/// Resolve a configuration that ships local files, expecting failure.
pub fn try_plan_with(
    name: &str,
    toml: &str,
    extra: &[(&str, &str)],
) -> Result<BuildPlan, kiln_diag::Errors> {
    let (m, dir) = manifest_with(name, toml, extra);
    kiln_resolve::resolve(
        &m,
        &dir.join("config"),
        &options(&dir, "."),
        &Inputs::new(&no_aur()),
    )
}

/// Resolve a configuration that also ships local files.
pub fn plan_with(name: &str, toml: &str, extra: &[(&str, &str)]) -> BuildPlan {
    let (m, dir) = manifest_with(name, toml, extra);
    kiln_resolve::resolve(
        &m,
        &dir.join("config"),
        &options(&dir, "."),
        &Inputs::new(&no_aur()),
    )
    .unwrap_or_else(|e| panic!("resolution failed:\n{}", kiln_diag::render_all(&e)))
}

/// Resolve against a transport that is not empty — for `packages.file`
/// entries whose `sha256` is itself a URL, which resolution has to fetch.
pub fn try_plan_with_transport(
    name: &str,
    toml: &str,
    extra: &[(&str, &str)],
    transport: &kiln_aur::Recorded,
) -> Result<BuildPlan, kiln_diag::Errors> {
    let (m, dir) = manifest_with(name, toml, extra);
    kiln_resolve::resolve(
        &m,
        &dir.join("config"),
        &options(&dir, "."),
        &Inputs::new(transport),
    )
}
