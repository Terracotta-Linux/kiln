//! The whole input taxonomy reaching a plan.
//!
//! The unit tests in `kiln-build` and `kiln-aur` check those crates in
//! isolation. This checks that resolution actually *asks* them, with the right
//! arguments, and puts the answers in the plan — the wiring, which is where an
//! integration usually goes wrong.

mod harness;

use harness::*;
use kiln_resolve::{Inputs, ResolvedInput};

/// A recipe in the configuration tree. `.SRCINFO` is required by resolution and
/// is written here for the same reason a user would commit one.
fn recipe(makedepends: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "pkgbuilds/mytool/PKGBUILD",
            "pkgname=mytool\npkgver=1.0\npkgrel=1\n".to_string(),
        ),
        (
            "pkgbuilds/mytool/.SRCINFO",
            format!(
                "pkgbase = mytool\n\tpkgver = 1.0\n\tpkgrel = 1\n\
                 \tmakedepends = {makedepends}\n\
                 \tsource = https://example.invalid/mytool-1.0.tar.gz\n\
                 \tsha256sums = aaaa\n\npkgname = mytool\n"
            ),
        ),
    ]
}

fn owned(pairs: Vec<(&'static str, String)>) -> Vec<(&'static str, String)> {
    pairs
}

fn as_refs<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    pairs.iter().map(|(a, b)| (*a, b.as_str())).collect()
}

const WITH_RECIPE: &str = "\nbuild = [\"pkgbuilds/mytool\"]\n";

#[test]
fn a_recipe_in_the_configuration_tree_becomes_a_built_package() {
    let files = owned(recipe("fixture-libfoo"));
    let plan = plan_with(
        "inputs-built",
        &format!("{BOOTABLE}{WITH_RECIPE}"),
        &as_refs(&files),
    );

    let built = plan
        .inputs
        .iter()
        .find_map(|i| match i {
            ResolvedInput::BuiltPackage { name, sources, .. } => Some((name, sources)),
            _ => None,
        })
        .expect("the recipe must reach the plan");
    assert_eq!(built.0, "mytool");
    assert_eq!(built.1.len(), 1, "its one pinned source");
    assert_eq!(built.1[0].sha256, "aaaa");
}

/// The cache identity, end to end: *"a package built against `gcc 15.1` is not the same
/// artifact as one built against `gcc 15.2`."* The unit test proves the key
/// function does this; this proves resolution actually feeds it the resolved
/// closure rather than the declared names.
#[test]
fn the_build_key_moves_when_a_build_time_dependency_does() {
    let key_for = |name: &str, makedepends: &str| {
        let files = owned(recipe(makedepends));
        plan_with(name, &format!("{BOOTABLE}{WITH_RECIPE}"), &as_refs(&files))
            .inputs
            .iter()
            .find_map(|i| i.build_key().cloned())
            .expect("a built package has a build key")
    };

    // `fixture-libfoo` is 1.2 in the fixture repository; `fixture-app` pulls it
    // in *and* adds itself, so the closures genuinely differ.
    assert_ne!(
        key_for("inputs-key-a", "fixture-libfoo"),
        key_for("inputs-key-b", "fixture-app"),
    );
    // The same recipe twice is the same key — otherwise nothing would ever hit.
    assert_eq!(
        key_for("inputs-key-c", "fixture-libfoo"),
        key_for("inputs-key-d", "fixture-libfoo"),
    );
}

/// *"bump `linux` from 6.19.2 to 6.19.3 and every out-of-tree module's
/// build key changes, its cache entry misses, and it rebuilds."*
#[test]
fn an_out_of_tree_module_is_keyed_to_the_kernel_in_the_image() {
    let toml = format!(
        "{BOOTABLE}\n\n[[kernel.module]]\nname = \"my-module\"\nsource = \"kernel/my-module\"\n"
    );
    let plan = plan_with(
        "inputs-module",
        &toml,
        &[("kernel/my-module/Makefile", "obj-m := my-module.o\n")],
    );

    let module = plan
        .inputs
        .iter()
        .find_map(|i| match i {
            ResolvedInput::KernelModule {
                name,
                kernel_evr,
                build_key,
                ..
            } => Some((name.clone(), kernel_evr.clone(), build_key.clone())),
            _ => None,
        })
        .expect("the module must reach the plan");

    assert_eq!(module.0, "my-module");
    assert_eq!(
        module.1, "6.19-1",
        "the resolved EVR of the kernel this image actually contains"
    );
    assert!(!module.2 .0.is_empty());
}

/// a `SKIP` checksum means the contents are only known after fetching,
/// so it is reported rather than guessed at — and excluded from `plan_id`,
/// because including something unresolved would make the identity a guess.
#[test]
fn a_recipe_with_an_unverifiable_source_is_reported_as_volatile() {
    let files: Vec<(&'static str, String)> = vec![
        ("pkgbuilds/mytool/PKGBUILD", "pkgname=mytool\n".to_string()),
        (
            "pkgbuilds/mytool/.SRCINFO",
            "pkgbase = mytool\n\tpkgver = r1\n\tpkgrel = 1\n\
             \tsource = mytool::git+https://example.invalid/mytool.git\n\
             \tsha256sums = SKIP\n"
                .to_string(),
        ),
    ];
    let plan = plan_with(
        "inputs-volatile",
        &format!("{BOOTABLE}{WITH_RECIPE}"),
        &as_refs(&files),
    );

    assert_eq!(plan.volatile.len(), 1, "{:?}", plan.volatile);
    assert!(plan.volatile[0].input.contains("pkgbuilds/mytool"));
    assert!(plan.volatile[0]
        .reason
        .contains("only known after fetching"));

    // The volatile input must not have quietly become part of the identity.
    let mut without = plan.clone();
    without.volatile.clear();
    assert_eq!(without.plan_id(), plan.plan_id());
}

/// *"Nothing is downloaded, nothing is built, nothing is unpacked."*
/// Sourcing a PKGBUILD to learn what it declares would be running a shell
/// script during what the user was told is a cheap metadata query, so a recipe
/// without a `.SRCINFO` is a diagnostic with the command to produce one.
#[test]
fn a_recipe_without_a_srcinfo_is_told_how_to_make_one() {
    let errs = try_plan_with(
        "inputs-no-srcinfo",
        &format!("{BOOTABLE}{WITH_RECIPE}"),
        &[("pkgbuilds/mytool/PKGBUILD", "pkgname=mytool\n")],
    )
    .expect_err("resolution must not run the PKGBUILD");

    insta::assert_snapshot!(kiln_diag::render_all(&errs));
}

/// AUR packages reach the plan with their commit as identity, and a
/// transitively pulled one records what required it.
#[test]
fn aur_packages_reach_the_plan_with_their_commit_and_their_reason() {
    let toml = format!("{BOOTABLE}\naur = [\"top-thing\"]\n");
    let (manifest, dir) = manifest("inputs-aur", &toml);

    let transport = kiln_aur::Recorded::new()
        .with_rpc(
            r#"{"type":"multiinfo","resultcount":2,"results":[
                {"Name":"top-thing","PackageBase":"top-thing","Version":"2.1-1",
                 "Maintainer":"someone","Depends":["helper-thing","fixture-libfoo"]},
                {"Name":"helper-thing","PackageBase":"helper-thing","Version":"0.4-2",
                 "Maintainer":"someone-else","Depends":[]}]}"#,
        )
        .with_head("top-thing", &"a".repeat(40))
        .with_head("helper-thing", &"b".repeat(40));

    let plan = kiln_resolve::resolve(
        &manifest,
        &dir.join("config"),
        &options(&dir, "."),
        &Inputs::new(&transport),
    )
    .unwrap_or_else(|e| panic!("{}", kiln_diag::render_all(&e)));

    let aur: Vec<(String, Option<String>)> = plan
        .inputs
        .iter()
        .filter_map(|i| match i {
            ResolvedInput::AurPackage {
                name,
                pulled_in_by,
                aur_commit,
                ..
            } => {
                assert_eq!(aur_commit.len(), 40, "the commit is the identity");
                Some((name.clone(), pulled_in_by.clone()))
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        aur,
        [
            ("helper-thing".to_string(), Some("top-thing".to_string())),
            ("top-thing".to_string(), None),
        ]
    );
    // `fixture-libfoo` is in the configured repositories, so the AUR closure
    // must stop there rather than looking for it on aur.archlinux.org.
    assert!(!plan
        .inputs
        .iter()
        .any(|i| matches!(i, ResolvedInput::AurPackage { name, .. } if name == "fixture-libfoo")));
}
