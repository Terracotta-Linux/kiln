//! `build_key`.
//!
//! A hit skips the build entirely, so what goes into this hash is the
//! difference between a cache that is fast and a cache that is *wrong*.

use kiln_build::SourcePin;
use kiln_build::{Ingredients, BUILDER_VERSION};
use kiln_manifest::Hash;

fn pin(url: &str, sha: &str) -> SourcePin {
    SourcePin {
        url: url.into(),
        sha256: sha.into(),
    }
}

fn base() -> Ingredients {
    Ingredients::new(Hash("b3:dd12".into()), "x86_64")
        .with_sources(vec![
            pin("https://example.invalid/mytool-1.2.0.tar.gz", "aaaa"),
            pin("mytool.patch", "bbbb"),
        ])
        .with_makedeps(vec!["gcc-15.1.1-1".into(), "cmake-4.1.0-2".into()])
}

#[test]
fn the_same_ingredients_give_the_same_key() {
    assert_eq!(base().build_key(), base().build_key());
}

/// The order a dependency closure happened to be walked in is not a property of
/// the artifact.
#[test]
fn order_within_the_ingredients_does_not_matter() {
    let mut reversed = base();
    reversed.sources.reverse();
    reversed.makedeps.reverse();
    assert_eq!(reversed.build_key(), base().build_key());

    // A name repeated in the closure is the same closure.
    let mut duplicated = base();
    duplicated.makedeps.push("gcc-15.1.1-1".into());
    assert_eq!(duplicated.build_key(), base().build_key());
}

/// The sentence this test exists for:
///
/// > Including `makedep_evrs` in the key is what makes the cache *correct*
/// > rather than merely fast — a package built against `gcc 15.1` is not the
/// > same artifact as one built against `gcc 15.2`, and pretending otherwise
/// > produces the worst class of bug this project can have.
#[test]
fn a_build_time_dependency_moving_invalidates_the_artifact() {
    let newer = Ingredients {
        makedeps: vec!["gcc-15.2.0-1".into(), "cmake-4.1.0-2".into()],
        ..base()
    };
    assert_ne!(newer.build_key(), base().build_key());
}

/// *"bump `linux` from 6.19.2 to 6.19.3 and every out-of-tree module's
/// build key changes, its cache entry misses, and it rebuilds. No DKMS-style
/// runtime rebuild, no hooks, no half-updated module tree."*
#[test]
fn a_kernel_bump_rebuilds_every_out_of_tree_module() {
    let against_old = base().against_kernel("6.19.2-1");
    let against_new = base().against_kernel("6.19.3-1");
    assert_ne!(against_old.build_key(), against_new.build_key());

    // An ordinary package is not a kernel module, and must not be dragged into
    // rebuilding every time the kernel moves.
    assert_ne!(against_old.build_key(), base().build_key());
    assert_eq!(base().build_key(), base().build_key());
}

#[test]
fn every_ingredient_reaches_the_key() {
    let baseline = base().build_key();

    let other_recipe = Ingredients {
        recipe: Hash("b3:0000".into()),
        ..base()
    };
    assert_ne!(other_recipe.build_key(), baseline, "the recipe");

    let other_arch = Ingredients {
        arch: "aarch64".into(),
        ..base()
    };
    assert_ne!(other_arch.build_key(), baseline, "the architecture");

    let mut moved_source = base();
    moved_source.sources[0].sha256 = "dead".into();
    assert_ne!(moved_source.build_key(), baseline, "a source's contents");

    let mut renamed_source = base();
    renamed_source.sources[0].url = "https://elsewhere.invalid/x.tar.gz".into();
    assert_ne!(
        renamed_source.build_key(),
        baseline,
        "where a source came from"
    );

    let fewer = Ingredients {
        sources: vec![pin("mytool.patch", "bbbb")],
        ..base()
    };
    assert_ne!(fewer.build_key(), baseline, "a dropped source");
}

/// The key is frozen so that a refactor cannot silently invalidate every
/// cached artifact in existence. `BUILDER_VERSION` is the deliberate lever;
/// changing it and this constant together is the supported way to do it.
#[test]
fn the_key_is_frozen() {
    assert_eq!(
        BUILDER_VERSION, 1,
        "bump the expectation below alongside it"
    );
    assert_eq!(
        base().build_key().to_string(),
        "b3:89284aba8cf626af16a8d7197880803f989db4eda78412511c567d7fea9c238f",
        "\n\
         `build_key` changed. Every cached .pkg.tar.zst just became unreachable —\n\
         hours of compilation on every machine running Kiln.\n\
         \n\
         If that was the intent, bump BUILDER_VERSION and this value together. If it\n\
         was not, something reordered or dropped an ingredient: find that.\n"
    );
}

/// `BUILDER_VERSION` and `HASH_EPOCH` invalidate different things on purpose:
/// changing how a build *runs* should not force every image to rebuild, and
/// changing what a plan *means* should not throw away hours of compilation.
#[test]
fn the_builder_version_is_not_the_hash_epoch() {
    assert_ne!(
        BUILDER_VERSION,
        kiln_manifest::HASH_EPOCH,
        "these have moved independently at least once; if they ever coincide by \
         accident, this test is the reminder that they are not the same knob"
    );
}
