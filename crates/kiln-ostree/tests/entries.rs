//! BLS entry ordering.
//!
//! This whole file exists because of one bug that already happened. libostree
//! writes `ostree-1.conf`, `ostree-2.conf`, …, and the entry that *boots* is
//! the one with the highest BLS `version` — the highest-numbered file. Sorting
//! by filename and taking the first selects the **rollback** deployment. That
//! cost one wrong boot in the phase 0 spike, and it would cost more in a
//! boot-acceptance test that silently asserted against the wrong image.

use kiln_ostree::entries;
use std::path::PathBuf;

fn boot_with(name: &str, entries: &[(&str, i64, &str)]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/test-roots")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("loader/entries")).unwrap();
    for (name, version, title) in entries {
        std::fs::write(
            dir.join("loader/entries").join(name),
            format!(
                "title {title}\nversion {version}\nlinux /ostree/kiln/vmlinuz\n\
                 options root=UUID=abc ostree=/ostree/boot.1/kiln/0\n"
            ),
        )
        .unwrap();
    }
    dir
}

/// The one that matters. `ostree-1.conf` sorts first by filename and boots
/// *last*; picking it is picking the rollback deployment.
#[test]
fn the_highest_version_boots_not_the_lowest_filename() {
    let boot = boot_with(
        "bls-order",
        &[
            ("ostree-1.conf", 1, "generation 41"),
            ("ostree-2.conf", 2, "generation 42"),
        ],
    );
    let order: Vec<String> = entries::read(&boot)
        .into_iter()
        .map(|e| e.filename)
        .collect();
    assert_eq!(order, ["ostree-2.conf", "ostree-1.conf"]);
    assert_eq!(entries::default(&boot).unwrap().title, "generation 42");
}

/// The filename and the version are allowed to disagree, and when they do the
/// version wins. A test built on entries that happen to agree proves nothing.
#[test]
fn the_filename_is_not_the_sort_key() {
    let boot = boot_with(
        "bls-disagree",
        &[("ostree-9.conf", 1, "old"), ("ostree-1.conf", 9, "new")],
    );
    assert_eq!(entries::default(&boot).unwrap().title, "new");
}

#[test]
fn an_entry_parses_into_its_fields() {
    let entry = entries::parse(
        "ostree-2.conf",
        "title Kiln 42\n\
         version 2\n\
         linux /ostree/kiln-abc/vmlinuz-6.19.2\n\
         initrd /ostree/kiln-abc/initramfs.img\n\
         options root=UUID=1234 rw quiet ostree=/ostree/boot.1/kiln/abc/0\n",
    );
    assert_eq!(entry.version, 2);
    assert_eq!(entry.title, "Kiln 42");
    assert!(entry.options.contains("ostree=/ostree/boot.1"));
    assert_eq!(entry.linux, "/ostree/kiln-abc/vmlinuz-6.19.2");
}

/// An entry with no `version` sorts last rather than crashing. A malformed
/// entry in `/boot` is somebody else's bug, and `kiln status` should still say
/// something useful.
#[test]
fn a_malformed_entry_does_not_take_the_listing_down() {
    let boot = boot_with("bls-malformed", &[("ostree-1.conf", 3, "good")]);
    std::fs::write(
        boot.join("loader/entries/broken.conf"),
        "this is not a bls entry\n",
    )
    .unwrap();
    let read = entries::read(&boot);
    assert_eq!(read.len(), 2);
    assert_eq!(read[0].title, "good");
    assert_eq!(read[1].version, 0);
}

#[test]
fn a_boot_directory_with_no_entries_is_not_an_error() {
    let dir = PathBuf::from("/nonexistent/boot");
    assert!(entries::read(&dir).is_empty());
    assert!(entries::default(&dir).is_none());
}
