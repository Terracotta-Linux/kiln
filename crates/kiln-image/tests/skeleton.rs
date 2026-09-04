//! The empty staging root. step 1.

mod scratch;

use kiln_image::skeleton;

#[test]
fn creates_the_database_directory_and_the_mountpoints_and_nothing_else() {
    let root = scratch::root("skeleton");
    skeleton::create(&root).unwrap();

    let mut top: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    top.sort();
    insta::assert_debug_snapshot!(top);
    assert!(root.join(kiln_alpm::session::DB_PATH).is_dir());
}

/// The obvious first move — lay down the usr-merge symlinks, then
/// install into them — aborts the transaction, because Arch's `filesystem`
/// package owns `/home`, `/opt`, `/srv` and `/root` as real directories. They
/// are created in step 10 instead. If someone "fixes" the skeleton by adding
/// them, this fails before the transaction does.
#[test]
fn the_top_level_symlinks_are_not_here() {
    let root = scratch::root("skeleton-no-links");
    skeleton::create(&root).unwrap();
    for owned_by_filesystem in ["home", "opt", "srv", "root", "usr/bin", "bin", "lib"] {
        assert!(
            root.join(owned_by_filesystem).symlink_metadata().is_err(),
            "{owned_by_filesystem} must not exist yet"
        );
    }
    assert!(!root.join("var").exists(), "/var comes from `filesystem`");
}

#[test]
fn tmp_is_sticky() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch::root("skeleton-tmp");
    skeleton::create(&root).unwrap();
    let mode = root.join("tmp").metadata().unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o1777);
}

/// Assembly builds from nothing. Reusing a directory that already has content
/// would mean a stale file from a failed build silently becoming image content.
#[test]
fn refuses_a_root_that_already_has_content() {
    let root = scratch::root("skeleton-dirty");
    scratch::file(&root, "usr/lib/leftover", "from a build that failed", 0o644);
    let err = skeleton::create(&root).unwrap_err();
    assert!(format!("{err}").contains("already has content"), "{err}");
}
