//! What Kiln has to canonicalize by hand.

mod scratch;

use kiln_image::determinism::*;
use scratch::*;

/// `%INSTALLDATE%` is the wall clock at transaction time, so two
/// builds of the same plan differ in one `desc` file per package — about 150 in
/// a base image.
#[test]
fn install_dates_are_pinned_to_the_epoch() {
    let root = root("determinism-installdate");
    file(
        &root,
        "usr/lib/sysimage/pacman/local/glibc-2.42-3/desc",
        "%NAME%\nglibc\n\n%INSTALLDATE%\n1756666000\n\n%SIZE%\n52000000\n",
        0o644,
    );
    file(
        &root,
        "usr/lib/sysimage/pacman/local/linux-6.19.2-1/desc",
        "%NAME%\nlinux\n\n%INSTALLDATE%\n1756666001\n",
        0o644,
    );

    assert_eq!(pin_install_dates(&root).unwrap(), 2);
    let text =
        std::fs::read_to_string(root.join("usr/lib/sysimage/pacman/local/glibc-2.42-3/desc"))
            .unwrap();
    assert_eq!(
        text,
        "%NAME%\nglibc\n\n%INSTALLDATE%\n0\n\n%SIZE%\n52000000\n"
    );

    // Idempotent: a second normalization of the same tree changes nothing.
    assert_eq!(pin_install_dates(&root).unwrap(), 0);
}

/// The `desc` format is a marker line followed by a value line. Both edges are
/// easy to mishandle in place, which is why the parsing is a pure function with
/// a test rather than a loop with a flag.
#[test]
fn pinning_a_field_handles_the_awkward_records() {
    // The field is the last thing in the file.
    assert_eq!(
        pin_field("%INSTALLDATE%\n1756\n", "%INSTALLDATE%", "0"),
        "%INSTALLDATE%\n0\n"
    );
    // The marker with no value at all — an empty field, not a missing one.
    assert_eq!(
        pin_field("%INSTALLDATE%\n\n%SIZE%\n1\n", "%INSTALLDATE%", "0"),
        "%INSTALLDATE%\n\n%SIZE%\n1\n"
    );
    // A record ending immediately after the marker.
    assert_eq!(
        pin_field("%NAME%\nx\n%INSTALLDATE%\n", "%INSTALLDATE%", "0"),
        "%NAME%\nx\n%INSTALLDATE%\n"
    );
    // Only the named field moves.
    assert_eq!(
        pin_field("%SIZE%\n99\n", "%INSTALLDATE%", "0"),
        "%SIZE%\n99\n"
    );
    // Every occurrence, not just the first.
    assert_eq!(
        pin_field("%A%\n1\n\n%A%\n2\n", "%A%", "0"),
        "%A%\n0\n\n%A%\n0\n"
    );
}

/// Setting `DBPath` for the transaction is only half the
/// relocation: the image ships a `pacman.conf` that still points at
/// `/var/lib/pacman`, which is now empty, so `pacman -Q` on the booted system
/// reports nothing and `kiln why` / `kiln owns` are dead on arrival.
#[test]
fn the_images_own_pacman_conf_points_at_the_relocated_database() {
    // Arch ships it commented out under [options].
    let stock = "[options]\n#RootDir     = /\n#DBPath      = /var/lib/pacman/\nHoldPkg = pacman\n\n[core]\n";
    let out = rewrite_dbpath(stock, "/usr/lib/sysimage/pacman").unwrap();
    assert!(out.contains("DBPath     = /usr/lib/sysimage/pacman\n"));
    assert!(!out.contains("/var/lib/pacman"));
    // Everything else survives, in order.
    assert!(out.contains("HoldPkg = pacman"));
    assert!(out.contains("[core]"));

    // An uncommented setting is replaced, not duplicated.
    let set = "[options]\nDBPath = /var/lib/pacman/\n";
    let out = rewrite_dbpath(set, "/usr/lib/sysimage/pacman").unwrap();
    assert_eq!(out.matches("DBPath").count(), 1);

    // No `[options]` at all is a broken config, and papering over it would
    // produce exactly the silent failure this exists to prevent.
    assert!(rewrite_dbpath("[core]\nServer = x\n", "/usr/lib/sysimage/pacman").is_none());
}

#[test]
fn a_missing_options_section_is_an_error_that_says_what_breaks() {
    let root = root("determinism-pacman-conf");
    file(&root, "etc/pacman.conf", "[core]\nServer = x\n", 0o644);
    let err = point_pacman_conf_at_the_image_database(&root)
        .unwrap_err()
        .to_string();
    assert!(err.contains("database would appear empty"), "got: {err}");
}

/// The half that is not about determinism at all: a populated
/// machine-id makes every machine deployed from the image share an identity.
#[test]
fn machine_id_is_truncated_rather_than_deleted() {
    let root = root("determinism-machine-id");
    file(
        &root,
        "etc/machine-id",
        "5f8a1c2e3b4d5e6f7a8b9c0d1e2f3a4b\n",
        0o444,
    );
    reset_machine_id(&root).unwrap();

    let at = root.join("etc/machine-id");
    assert!(
        at.is_file(),
        "systemd's first-boot marker is an *empty* file, not a missing one"
    );
    assert_eq!(std::fs::metadata(&at).unwrap().len(), 0);
}

/// The synced repository databases are resolution state, not image content, and
/// they are large. `local/` must survive: that is what `pacman -Q` reads on the
/// booted system.
#[test]
fn sync_databases_are_dropped_and_the_local_one_is_not() {
    let root = root("determinism-syncdb");
    file(&root, "usr/lib/sysimage/pacman/sync/core.db", "aaaa", 0o644);
    file(
        &root,
        "usr/lib/sysimage/pacman/local/glibc-2.42-3/desc",
        "%NAME%\nglibc\n",
        0o644,
    );

    assert_eq!(drop_sync_databases(&root).unwrap(), 4);
    assert!(!root.join("usr/lib/sysimage/pacman/sync").exists());
    assert!(root
        .join("usr/lib/sysimage/pacman/local/glibc-2.42-3/desc")
        .is_file());

    // Idempotent — normalization may run over a tree that has already had this
    // done to it.
    assert_eq!(drop_sync_databases(&root).unwrap(), 0);
}
