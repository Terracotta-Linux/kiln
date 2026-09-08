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
        &dkms::Sources::Package {
            name: "nvidia-open-dkms",
            evr: "580.95.05-1",
        },
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
    .expect("materializing the recipe")
}

/// A DKMS source tree as a user would write one: a `dkms.conf` naming the
/// module, and whatever `MAKE[0]` needs.
fn tree(at: &Path, conf: &str) -> PathBuf {
    let dir = at.join("src/my-driver");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "obj-m := my-driver.o\n").unwrap();
    std::fs::write(dir.join("my-driver.c"), "/* a driver */\n").unwrap();
    if !conf.is_empty() {
        std::fs::write(dir.join("dkms.conf"), conf).unwrap();
    }
    dir
}

const DKMS_CONF: &str = "PACKAGE_NAME=\"my-driver\"\nPACKAGE_VERSION=\"1.0\"\n\
                         BUILT_MODULE_NAME[0]=\"my-driver\"\n\
                         DEST_MODULE_LOCATION[0]=\"/kernel/drivers/misc\"\n";

fn materialize_tree(into: &str, conf: &str) -> Result<PathBuf, dkms::Error> {
    let base = scratch(into);
    let from = tree(&base, conf);
    dkms::materialize(
        &dkms::Sources::Tree {
            name: "my-driver",
            dir: &from,
        },
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
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

/// A DKMS tree of your own reaches the same build. What differs is where the
/// sources come from, and therefore what the build root has to hold: `dkms` and
/// the headers, and no third package, because there is no package.
#[test]
fn a_source_tree_reads_back_as_a_recipe_and_asks_for_no_package() {
    let dir = materialize_tree("dkms-tree", DKMS_CONF).expect("materializing");
    let recipe = Recipe::read(
        &dir,
        "my-driver-modules",
        Hash("b3:aa".into()),
        "x86_64",
        &NeverRuns,
    )
    .expect("reading the synthesized recipe");

    assert_eq!(recipe.meta.pkgnames, ["my-driver-modules"]);
    assert_eq!(recipe.meta.makedepends, ["dkms", "linux-headers"]);
    assert!(recipe.remote_sources().is_empty());
}

/// The driver's own source is carried over, and the recipe Kiln wrote does not
/// leak into it — the same contract `[[kernel.module]]` has.
#[test]
fn the_source_tree_is_copied_and_the_configuration_tree_is_not_touched() {
    let base = scratch("dkms-copy");
    let from = tree(&base, DKMS_CONF);
    let dir = dkms::materialize(
        &dkms::Sources::Tree {
            name: "my-driver",
            dir: &from,
        },
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
    .unwrap();

    assert!(dir.join("dkms.conf").is_file());
    assert!(dir.join("my-driver.c").is_file());
    assert!(dir.join("PKGBUILD").is_file());
    // The config root is somewhere Kiln reads and never writes.
    assert!(!from.join("PKGBUILD").exists());
    assert!(!from.join(".SRCINFO").exists());

    let text = pkgbuild(&dir);
    assert!(
        text.contains("rm -f \"$staged/PKGBUILD\""),
        "the synthesized recipe must not become part of the driver's source: {text}"
    );
    // `dkms add` insists the sources sit at `$source_tree/$module-$version`,
    // and /usr/src is not writable by the build user.
    assert!(
        text.contains("_sourcetree() {\n  echo \"$srcdir/src\""),
        "{text}"
    );
}

/// A tree with no `dkms.conf` is an ordinary out-of-tree module, and the two
/// are built by different tools. Saying so here costs nothing; discovering it
/// after a build root has been assembled costs a minute and a confusing error
/// out of `dkms`.
#[test]
fn a_tree_without_a_dkms_conf_is_refused_before_anything_is_built() {
    let err = materialize_tree("dkms-no-conf", "").expect_err("must not materialize");
    let text = err.to_string();
    assert!(text.contains("no dkms.conf"), "{text}");
    assert!(text.contains("[[kernel.module]]"), "{text}");
}

/// Both shapes produce a `pkgver` `makepkg` will accept — a tree has no version
/// of its own, so it takes the kernel's, the way `[[kernel.module]]` does.
#[test]
fn a_tree_is_versioned_by_the_kernel_it_was_built_against() {
    let dir = materialize_tree("dkms-tree-pkgver", DKMS_CONF).unwrap();
    assert!(
        pkgbuild(&dir).contains("\npkgver=6.19.2_1\n"),
        "{}",
        pkgbuild(&dir)
    );
    // The `.SRCINFO` beside it has to agree, or Kiln would be lying to itself.
    let srcinfo = std::fs::read_to_string(dir.join(".SRCINFO")).unwrap();
    assert!(srcinfo.contains("pkgver = 6.19.2_1"), "{srcinfo}");
}

/// The recipe is bash, and a syntax error in it is a build that dies after the
/// build root has been assembled — several hundred megabytes and a minute in,
/// for a mistake a parse would have caught here.
#[test]
fn the_synthesized_recipe_is_valid_bash() {
    for dir in [
        materialize("dkms-syntax"),
        materialize_tree("dkms-syntax-tree", DKMS_CONF).unwrap(),
    ] {
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
}
