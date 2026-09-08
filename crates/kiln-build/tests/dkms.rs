//! DKMS packages as synthesized recipes.
//!
//! Nothing here compiles anything — that is `makepkg`'s job, in the same
//! sandbox every other package goes through. What is worth asserting is that
//! the recipe Kiln writes is a recipe Kiln can then *read*, that it is bash
//! `bash -n` will accept, and that the three facts the whole design turns on
//! are actually in it: the DKMS package goes in as a build-time dependency and
//! not as image content, the modules land where `dkms` itself would put them,
//! and a build that compiles nothing fails rather than shipping an image
//! missing a driver.

use kiln_build::{dkms, Recipe};
use kiln_manifest::Hash;
use kiln_sandbox::{Outcome, Sandbox, SandboxSpec};
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

/// The sandbox must never be reached: a synthesized recipe ships its own
/// `.SRCINFO`.
struct NeverRuns;
impl Sandbox for NeverRuns {
    fn name(&self) -> &'static str {
        "never-runs"
    }
    fn argv(&self, _: &SandboxSpec) -> kiln_sandbox::Result<Vec<String>> {
        panic!("a synthesized recipe must ship its own .SRCINFO")
    }
    fn run(&self, _: &SandboxSpec) -> kiln_sandbox::Result<Outcome> {
        panic!("a synthesized recipe must ship its own .SRCINFO")
    }
}

fn materialize(into: &str) -> PathBuf {
    let base = scratch(into);
    dkms::materialize(
        "nvidia-open-dkms",
        "580.95.05-1",
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
    .expect("materializing the recipe")
}

fn pkgbuild(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("PKGBUILD")).unwrap()
}

#[test]
fn a_synthesized_recipe_reads_back_as_a_recipe() {
    let dir = materialize("dkms-recipe");
    let recipe = Recipe::read(
        &dir,
        "nvidia-open-dkms-modules",
        Hash("b3:aa".into()),
        "x86_64",
        &NeverRuns,
    )
    .expect("reading the synthesized recipe");

    assert_eq!(recipe.meta.pkgbase, "nvidia-open-dkms-modules");
    assert_eq!(recipe.meta.pkgnames, ["nvidia-open-dkms-modules"]);
    // The three names the build root has to hold. The DKMS package is among
    // them, which is the whole mechanism: `makedepends` is what installs it
    // into a root that is thrown away, so its sources never reach the image.
    assert_eq!(
        recipe.meta.makedepends,
        ["dkms", "linux-headers", "nvidia-open-dkms"]
    );
    // Nothing to fetch. The sources arrive as a package, so phase 1 — the only
    // phase with a network — has no work to do.
    assert!(recipe.remote_sources().is_empty());
    assert!(!recipe.meta.is_volatile());
}

/// The name is neither the DKMS package's nor the prebuilt package's. Claiming
/// to be `nvidia-open-dkms` while containing no sources would make `pacman -Q`
/// on a booted image lie; claiming to be `nvidia-open` would collide with a
/// real package in the repositories.
#[test]
fn the_built_package_is_named_for_what_it_contains() {
    assert_eq!(
        dkms::package_name("nvidia-open-dkms"),
        "nvidia-open-dkms-modules"
    );
    assert_eq!(
        dkms::package_name("v4l2loopback-dkms"),
        "v4l2loopback-dkms-modules"
    );
}

/// `pkgver` may contain neither `-` nor `:`, and a package EVR can contain
/// both. `makepkg` rejects the recipe outright rather than warning.
#[test]
fn the_package_version_is_sanitized_into_something_makepkg_accepts() {
    let dir = materialize("dkms-pkgver");
    assert!(
        pkgbuild(&dir).contains("\npkgver=580.95.05_1\n"),
        "{}",
        pkgbuild(&dir)
    );
}

/// `updates/dkms` is where `dkms install` would have put these on Arch —
/// `override_dest_module_location` rewrites every `DEST_MODULE_LOCATION` to it
/// — and inside the kernel's own directory, because Kiln finds the kernel by
/// looking for `/usr/lib/modules/*/pkgbase` and refuses to guess when it finds
/// two.
#[test]
fn modules_land_where_dkms_itself_would_put_them() {
    let dir = materialize("dkms-dest");
    let text = pkgbuild(&dir);
    assert!(
        text.contains("$pkgdir/usr/lib/modules/$kver/updates/dkms"),
        "{text}"
    );
    for line in text.lines().filter(|l| l.contains("$pkgdir")) {
        assert!(
            line.contains("$pkgdir/usr/lib/modules/$kver/"),
            "writes outside the kernel's module directory: {line}"
        );
    }
}

/// `dkms` will happily report success for a `make` that compiled nothing.
/// Shipping an image whose driver is silently absent is the failure this
/// exists to prevent, and it is only pleasant if it happens at build time.
#[test]
fn a_dkms_package_that_produces_no_module_fails_the_build() {
    assert!(pkgbuild(&materialize("dkms-empty")).contains("==> ERROR:"));
}

/// Signing would generate a machine-owned key on first use and sign with it,
/// which is a secret invented mid-build and therefore not one of the image's
/// inputs. `dkms` decides this from its framework config, which an exported
/// value it does not override supplies.
#[test]
fn module_signing_is_turned_off_rather_than_left_to_a_guess() {
    assert!(pkgbuild(&materialize("dkms-signing")).contains("export try_sign_modules=false"));
}

/// The recipe is bash, and a syntax error in it is a build that dies after the
/// build root has been assembled — several hundred megabytes and a minute in,
/// for a mistake a parse would have caught here.
#[test]
fn the_synthesized_recipe_is_valid_bash() {
    let dir = materialize("dkms-syntax");
    let out = std::process::Command::new("bash")
        .arg("-n")
        .arg(dir.join("PKGBUILD"))
        .output()
        .expect("running bash -n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
