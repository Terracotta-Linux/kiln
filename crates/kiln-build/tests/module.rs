//! Out-of-tree modules as synthesized recipes.
//!
//! Nothing here compiles a module — that is `makepkg`'s job, in the same
//! sandbox every other package goes through, which is the whole claim of
//! What is worth asserting is that the recipe Kiln writes is a recipe
//! Kiln can then *read*, because the alternative is discovering it is not
//! twenty minutes into a build.

use kiln_build::{module, Recipe};
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

/// A module source tree: a Makefile and a C file, which is what
/// `[[kernel.module]]` points at.
fn source(at: &Path) -> PathBuf {
    let dir = at.join("src/v4l2loopback");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "obj-m := v4l2loopback.o\n").unwrap();
    std::fs::write(dir.join("v4l2loopback.c"), "/* a module */\n").unwrap();
    dir
}

/// The sandbox must never be reached: a synthesized recipe ships its own
/// `.SRCINFO`, and spending a `makepkg --printsrcinfo` round trip to re-read a
/// file Kiln wrote thirty lines earlier would be asking bash what Kiln knows.
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

#[test]
fn a_synthesized_recipe_reads_back_as_a_recipe() {
    let base = scratch("module-recipe");
    let dir = module::materialize(
        "v4l2loopback",
        &source(&base),
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
    .expect("materializing the recipe");

    let recipe = Recipe::read(
        &dir,
        "v4l2loopback",
        Hash("b3:aa".into()),
        "x86_64",
        &NeverRuns,
    )
    .expect("reading the synthesized recipe");

    assert_eq!(recipe.meta.pkgbase, "v4l2loopback");
    assert_eq!(recipe.meta.pkgnames, ["v4l2loopback"]);
    // the headers are what a module is compiled against, and naming them
    // in `makedepends` is what puts them in the build root and their resolved
    // EVR in the build key.
    assert_eq!(recipe.meta.makedepends, ["linux-headers"]);
    // Nothing to fetch. A module's source is a directory in the configuration
    // tree, so phase 1 — the only phase with a network — has no work to do.
    assert!(recipe.remote_sources().is_empty());
    assert!(!recipe.meta.is_volatile());
}

/// The module's own source is carried over, and the recipe Kiln wrote does not
/// leak into it.
#[test]
fn the_module_source_is_copied_and_the_configuration_tree_is_not_touched() {
    let base = scratch("module-copy");
    let from = source(&base);
    let dir = module::materialize(
        "v4l2loopback",
        &from,
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
    .unwrap();

    assert!(dir.join("Makefile").is_file());
    assert!(dir.join("v4l2loopback.c").is_file());
    assert!(dir.join("PKGBUILD").is_file());
    // the config root is somewhere Kiln reads and never writes, and
    // `makepkg` writes to the directory it runs in.
    assert!(!from.join("PKGBUILD").exists());
    assert!(!from.join(".SRCINFO").exists());

    let pkgbuild = std::fs::read_to_string(dir.join("PKGBUILD")).unwrap();
    assert!(
        pkgbuild.contains("rm -f \"$srcdir/v4l2loopback/PKGBUILD\""),
        "the synthesized recipe must not become part of the module's source"
    );
}

/// Kiln finds the kernel by looking for `/usr/lib/modules/*/pkgbase` and
/// refuses to guess when it finds two directories. A module that added a
/// top-level one would break the step that places the kernel, so it goes
/// *inside* the kernel's own directory.
#[test]
fn a_module_installs_inside_the_kernels_own_module_directory() {
    let base = scratch("module-dest");
    let dir = module::materialize(
        "v4l2loopback",
        &source(&base),
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
    .unwrap();
    let pkgbuild = std::fs::read_to_string(dir.join("PKGBUILD")).unwrap();
    assert!(
        pkgbuild.contains("$pkgdir/usr/lib/modules/$kver/extramodules"),
        "{pkgbuild}"
    );
    // Every path the package writes to is under the kernel's own directory.
    // A sibling — the `extramodules-6.19` shape Arch's own module packages use
    // — would be the second `/usr/lib/modules/*` entry refuses to choose
    // between.
    for line in pkgbuild.lines().filter(|l| l.contains("$pkgdir")) {
        assert!(
            line.contains("$pkgdir/usr/lib/modules/$kver/"),
            "writes outside the kernel's module directory: {line}"
        );
    }
}

/// `pkgver` may contain neither `-` nor `:`, and a kernel EVR contains both.
/// `makepkg` rejects the recipe outright rather than warning, so this is a
/// build that never starts.
#[test]
fn the_kernel_version_is_sanitized_into_something_makepkg_accepts() {
    assert_eq!(module::version_of("6.19.2-1"), "6.19.2_1");
    assert_eq!(module::version_of("2:6.19.2-1"), "2_6.19.2_1");
    for evr in ["6.19.2-1", "2:6.19.2-1"] {
        let version = module::version_of(evr);
        assert!(
            !version.contains('-') && !version.contains(':'),
            "{version}"
        );
    }
}

/// A `make` that compiles nothing exits 0. Shipping an image whose driver is
/// silently absent is the failure is trying to make pleasant, and it is
/// only pleasant if it happens at build time.
#[test]
fn a_module_that_produces_no_ko_file_fails_the_build() {
    let base = scratch("module-empty");
    let dir = module::materialize(
        "v4l2loopback",
        &source(&base),
        &base.join("recipe"),
        "x86_64",
        "linux",
        "6.19.2-1",
    )
    .unwrap();
    let pkgbuild = std::fs::read_to_string(dir.join("PKGBUILD")).unwrap();
    assert!(pkgbuild.contains("==> ERROR:"), "{pkgbuild}");
}
