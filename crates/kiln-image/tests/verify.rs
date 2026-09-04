//! The OSTree contract, checked.
//!
//! Every assertion here corresponds to something that went wrong at least once
//! in the Phase 0 spike. Several of them fail only *after* a successful boot,
//! which is the most expensive place to find a problem — so they are checked
//! before the commit instead.

mod scratch;

use kiln_image::verify;
use scratch::*;
use std::path::Path;

/// A tree that satisfies the contract, for the tests below to break one thing
/// at a time.
fn good(name: &str) -> std::path::PathBuf {
    let root = root(name);
    for (link_name, target) in verify::TOP_LEVEL_LINKS {
        link(&root, link_name, target);
    }
    for d in verify::TOP_LEVEL_DIRS {
        dir(&root, d, 0o755);
    }
    dir(&root, "usr/etc", 0o755);
    file(&root, "usr/etc/machine-id", "", 0o444);
    dir(&root, "usr/lib/sysimage/pacman/local", 0o755);
    // libostree titles the boot entry from `ID` or `PRETTY_NAME` and refuses to
    // deploy a tree with neither. Arch's `filesystem` package ships this.
    file(&root, "usr/lib/os-release", "NAME=Kiln\nID=kiln\n", 0o644);
    root
}

fn problems(root: &Path) -> Vec<String> {
    verify::check(root).into_iter().map(|p| p.what).collect()
}

#[test]
fn a_normalized_tree_passes() {
    let root = good("verify-good");
    assert_eq!(problems(&root), Vec::<String>::new());
}

/// Nothing in Arch creates this symlink, and it went unmentioned until it broke.
/// Without it `ostree admin status` fails from inside the booted system, which
/// would take `kiln list`, `kiln status` and `kiln rollback` with it.
#[test]
fn a_missing_ostree_symlink_is_caught_and_explained() {
    let root = good("verify-ostree-link");
    std::fs::remove_file(root.join("ostree")).unwrap();

    let found = verify::check(&root);
    assert_eq!(found.len(), 1);
    assert!(found[0].what.contains("/ostree"));
    assert!(
        found[0].consequence.contains("kiln rollback"),
        "the message must say what it breaks: {}",
        found[0].consequence
    );
}

/// the `pacman` package owns `/var/lib/pacman`, so relocating the
/// database does not remove it, and the drain would recreate it on every boot.
#[test]
fn a_surviving_pacman_directory_is_caught() {
    let root = good("verify-var-pacman");
    dir(&root, "var/lib/pacman", 0o755);
    let found = problems(&root);
    assert!(
        found.iter().any(|p| p.contains("/var/lib/pacman")),
        "{found:?}"
    );
    // …and it is *also* caught as a non-empty /var, which is the point of
    // checking both: either alone would let one shape of the bug through.
    assert!(
        found.iter().any(|p| p.contains("/var is not empty")),
        "{found:?}"
    );
}

#[test]
fn an_unmoved_etc_is_caught() {
    let root = good("verify-etc");
    file(&root, "etc/motd", "hello\n", 0o644);
    // A real `/etc` mountpoint is fine; shipped content in it is not — the
    // check is that /etc has been *moved*, which a stray file proves it was not.
    std::fs::remove_dir_all(root.join("usr/etc")).unwrap();
    let found = problems(&root);
    assert!(
        found.iter().any(|p| p.contains("/usr/etc is missing")),
        "{found:?}"
    );
}

/// Again as a contract check rather than a determinism one: this is
/// the assertion that catches an image which would give every machine deployed
/// from it the same identity.
#[test]
fn a_populated_machine_id_is_caught() {
    let root = good("verify-machine-id");
    file(
        &root,
        "usr/etc/machine-id",
        "5f8a1c2e3b4d5e6f7a8b9c0d1e2f3a4b\n",
        0o444,
    );
    let found = verify::check(&root);
    assert_eq!(found.len(), 1);
    assert!(found[0].consequence.contains("share one identity"));
}

#[test]
fn boot_must_be_empty_because_ostree_owns_it() {
    let root = good("verify-boot");
    file(&root, "boot/vmlinuz-linux", "kernel\n", 0o644);
    assert!(problems(&root)
        .iter()
        .any(|p| p.contains("/boot is not empty")));
}

/// report everything wrong in one pass. Fixing contract violations one
/// build at a time, at ten minutes a build, is the difference between an
/// afternoon and a week.
#[test]
fn every_problem_is_reported_at_once_and_described() {
    let root = root("verify-empty");
    let found = verify::check(&root);
    assert!(
        found.len() > 5,
        "an empty directory violates nearly everything"
    );

    let text = verify::describe(&found);
    assert!(text.contains("OSTree filesystem contract"));
    for problem in &found {
        assert!(text.contains(&problem.what));
        assert!(!problem.consequence.is_empty(), "{}", problem.what);
    }
}

/// libostree titles the boot entry from `ID` or `PRETTY_NAME` and refuses to
/// deploy a tree that has neither. Such a tree commits perfectly happily, so
/// without this check the failure lands at deploy time — after a build that
/// looked like it worked, and with a message about `/etc/os-release` in a tree
/// whose `/etc` is now `/usr/etc`.
#[test]
fn a_tree_with_no_os_identity_is_a_problem() {
    let root = good("verify-os-release");
    file(&root, "usr/lib/os-release", "NAME=Nameless\n", 0o644);

    let problems = verify::check(&root);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].what.contains("ID or PRETTY_NAME"));

    file(&root, "usr/lib/os-release", "NAME=Kiln\nID=kiln\n", 0o644);
    assert!(verify::check(&root).is_empty());
}
