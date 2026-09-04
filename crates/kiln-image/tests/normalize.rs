//! Step 10: the OSTree contract, end to end over a synthetic tree.
//!
//!
//! The individual pieces have their own tests. What is tested here is the
//! *order*, which is where the findings live: every step depends on the one
//! before it, and getting any of them backwards produces a tree that commits
//! cleanly and fails after a successful boot.

mod scratch;

use kiln_image::{normalize, verify};
use std::path::{Path, PathBuf};

/// A staging root shaped like one a real transaction leaves behind.
fn assembled(name: &str) -> PathBuf {
    let root = scratch::root(name);
    scratch::account_files(&root);
    scratch::file(
        &root,
        "etc/machine-id",
        "6f2a1e7c3b594d8e9a0f1b2c3d4e5f60\n",
        0o444,
    );
    scratch::file(
        &root,
        "etc/pacman.conf",
        "[options]\nDBPath = /var/lib/pacman\nHoldPkg = pacman glibc\n",
        0o644,
    );
    scratch::file(&root, "usr/bin/sh", "#!/bin/sh\n", 0o755);
    // libostree refuses to deploy a tree with no `ID` or `PRETTY_NAME`, so the
    // contract verifier checks for one. Arch's `filesystem` package ships this.
    scratch::file(
        &root,
        "usr/lib/os-release",
        "NAME=\"Arch Linux\"\nID=arch\nPRETTY_NAME=\"Arch Linux\"\n",
        0o644,
    );

    // What a base image actually leaves in the directories that become
    // symlinks. Deleting these with the directory would silently
    // lose the web root.
    scratch::file(&root, "srv/http/index.html", "<h1>it works</h1>\n", 0o644);
    scratch::file(&root, "root/.bash_profile", "# root's profile\n", 0o644);

    scratch::dir(&root, "var/lib/pacman/local", 0o755);
    // The relocated database, where a real transaction with `DBPath` set puts
    // it. The contract verifier checks for it, and rightly: without it the
    // booted image answers `pacman -Q` with nothing.
    scratch::file(
        &root,
        "usr/lib/sysimage/pacman/local/fixture-base-1.0-1/desc",
        "%NAME%\nfixture-base\n\n%INSTALLDATE%\n1756684800\n\n",
        0o644,
    );
    scratch::file(&root, "var/lib/myservice/state.db", "state\n", 0o644);
    scratch::file(&root, "var/log/pacman.log", "wall clock\n", 0o644);
    scratch::link(&root, "var/run", "../run");
    scratch::dir(&root, "boot", 0o755);
    scratch::dir(&root, "tmp", 0o1777);
    root
}

#[test]
fn a_normalized_tree_satisfies_the_contract_verifier() {
    let root = assembled("normalize-contract");
    normalize::run(&root).unwrap();

    let problems = verify::check(&root);
    assert!(problems.is_empty(), "{}", verify::describe(&problems));
}

///, and the reason the relocation runs *before* the drain. `/srv`
/// becomes a symlink into `/var`, so what a package left there is `/var`
/// content. Draining first and relocating after would leave `/home` pointing at
/// nothing and `/srv/http` deleted.
#[test]
fn what_a_package_left_in_srv_survives_as_a_var_default() {
    let root = assembled("normalize-n2");
    let report = normalize::run(&root).unwrap();

    assert!(report
        .relocated
        .contains(&("/srv/http".into(), "/var/srv/http".into())));
    assert!(root
        .join("usr/share/factory/var/srv/http/index.html")
        .is_file());
    let conf = std::fs::read_to_string(root.join("usr/lib/tmpfiles.d/kiln-var.conf")).unwrap();
    assert!(conf.contains("C /var/srv/http/index.html"), "{conf}");
}

/// `/root` becomes `/var/roothome`, not `/var/root`.
#[test]
fn roots_home_is_relocated_under_its_ostree_name() {
    let root = assembled("normalize-roothome");
    normalize::run(&root).unwrap();
    assert!(root
        .join("usr/share/factory/var/roothome/.bash_profile")
        .is_file());
    assert_eq!(
        std::fs::read_link(root.join("root")).unwrap(),
        Path::new("var/roothome")
    );
}

/// Findings N6 and D2, and the reason they run first: both files are in `/etc`,
/// and after step 4 there is no `/etc` to fix.
#[test]
fn pacman_conf_and_machine_id_are_fixed_before_etc_moves() {
    let root = assembled("normalize-etc-first");
    normalize::run(&root).unwrap();

    let conf = std::fs::read_to_string(root.join("usr/etc/pacman.conf")).unwrap();
    assert!(conf.contains("/usr/lib/sysimage/pacman"), "{conf}");
    assert!(!conf.contains("/var/lib/pacman"), "{conf}");
    assert_eq!(
        std::fs::metadata(root.join("usr/etc/machine-id"))
            .unwrap()
            .len(),
        0
    );
}

/// the whole directory moves, and `/etc` must not exist in the commit.
/// libostree creates the deployment's `/etc` by 3-way-merging `/usr/etc`
/// against the machine's own.
#[test]
fn etc_becomes_usr_etc_and_leaves_nothing_behind() {
    let root = assembled("normalize-etc");
    normalize::run(&root).unwrap();
    assert!(!root.join("etc").exists());
    assert!(root.join("usr/etc/passwd").is_file());
}

/// Nothing in Arch creates this link, and without it
/// `ostree admin status` fails from inside the booted system — taking
/// `kiln list`, `kiln status` and `kiln rollback` down with it. The failure
/// appears only after a *successful* boot, which is the worst place to find it.
#[test]
fn the_ostree_symlink_is_there() {
    let root = assembled("normalize-n4");
    normalize::run(&root).unwrap();
    assert_eq!(
        std::fs::read_link(root.join("ostree")).unwrap(),
        Path::new("sysroot/ostree")
    );
}

/// The usr-merge links are *relative*. An absolute `/usr/bin` resolves against
/// the builder's filesystem whenever the tree is read from outside the
/// deployment — which is how the installer and libostree read it.
#[test]
fn the_usr_merge_links_are_relative() {
    let root = assembled("normalize-relative");
    normalize::run(&root).unwrap();
    for (link, target) in verify::TOP_LEVEL_LINKS {
        let got = std::fs::read_link(root.join(link)).unwrap();
        assert_eq!(got, Path::new(target), "/{link}");
        assert!(!got.is_absolute(), "/{link} must be relative");
    }
}

/// The `pacman` package owns `/var/lib/pacman` regardless of
/// `DBPath`, so a faithful drain would emit a line recreating it on every boot
/// — making the design's own assertion true at build time and false at runtime.
#[test]
fn var_lib_pacman_is_dropped_rather_than_drained() {
    let root = assembled("normalize-n5");
    normalize::run(&root).unwrap();
    let conf = std::fs::read_to_string(root.join("usr/lib/tmpfiles.d/kiln-var.conf")).unwrap();
    assert!(!conf.contains("/var/lib/pacman"), "{conf}");
}

/// `/var` is empty, not absent: libostree creates the real `/var` in the
/// stateroot, and the commit carries the mountpoint.
#[test]
fn var_is_present_and_empty() {
    let root = assembled("normalize-var");
    normalize::run(&root).unwrap();
    assert!(root.join("var").is_dir());
    assert_eq!(
        kiln_image::tree::entries(&root.join("var")).unwrap().len(),
        0
    );
}

/// an empty configuration produces an empty image. Such an image has no
/// `pacman` package and therefore no `/etc/pacman.conf`, and normalization must
/// not fail on it — "No such file or directory" for a file that is *correctly*
/// absent is the kind of error that sends someone looking for a bug that is not
/// there.
#[test]
fn an_image_with_no_packages_normalizes() {
    let root = scratch::root("normalize-empty");
    scratch::dir(&root, "usr/lib", 0o755);
    scratch::dir(&root, "var", 0o755);
    normalize::run(&root).unwrap();
    assert!(root.join("ostree").symlink_metadata().is_ok());
}
