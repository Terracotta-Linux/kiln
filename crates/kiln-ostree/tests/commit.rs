//! Committing and reading back.
//!
//! **Privileged.** A `Bare` repository stores real ownership, so writing one
//! needs root — and a `BareUser` repository would test something else: a
//! deployment checked out from one has every file owned by whoever built it.
//!
//! What is not privileged, and lives in `entries.rs`, is the decision about
//! which deployment boots. That is the one that has already been got wrong.

mod harness;

use kiln_ostree::commit::{self, CommitOptions};
use kiln_ostree::generation::{self, Metadata};
use kiln_record::Record;
use ostree::gio;
use std::path::Path;

#[test]
#[ignore = "privileged: a bare OSTree repository stores real ownership"]
fn a_commit_carries_its_generation_and_its_record() {
    if !harness::require_root("committing to a bare repository") {
        return;
    }
    let base = harness::scratch("ostree-commit");
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();
    let record = Record::of(&plan, 1, kiln_resolve::UidMap::new());

    let committed =
        commit::commit(&tree, &plan, &record, &harness::manifest(), &options(&base)).unwrap();
    assert_eq!(committed.generation, 1);
    assert_eq!(committed.reference, "kiln/fixture/x86_64");
    assert_eq!(committed.parent, None);

    let repo = commit::open_or_create(&base.join("repo")).unwrap();
    let metadata = commit::read_metadata(&repo, &committed.checksum).unwrap();
    assert_eq!(metadata.plan_id, plan.plan_id().to_string());
    assert_eq!(metadata.image, "fixture");
    // the record is in the metadata so `kiln list` and `kiln check` do not
    // have to check out a tree to answer.
    assert_eq!(metadata.record.unwrap().plan_id(), plan.plan_id());
}

/// monotonic, `parent.generation + 1`, read from the parent commit rather
/// than from a counter anywhere else — the number has to survive a machine that
/// was rolled back, reimaged, or had `/var/lib/kiln` deleted.
#[test]
#[ignore = "privileged: a bare OSTree repository stores real ownership"]
fn generations_increment_from_the_parent_commit() {
    if !harness::require_root("committing to a bare repository") {
        return;
    }
    let base = harness::scratch("ostree-generations");
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();
    let record = Record::of(&plan, 1, kiln_resolve::UidMap::new());
    let opts = options(&base);

    let first = commit::commit(&tree, &plan, &record, &harness::manifest(), &opts).unwrap();
    std::fs::write(
        tree.join("usr/lib/os-release"),
        "NAME=Kiln\nID=kiln\nPRETTY_NAME=\"Kiln fixture\"\nVERSION=2\n",
    )
    .unwrap();

    // The number is the caller's to supply, because the tree being committed
    // already contains a record that names it — so the increment lives in
    // `next_generation`, and this is the test that it reads the parent.
    let repo = commit::open_or_create(&base.join("repo")).unwrap();
    let next = commit::next_generation(&repo, &plan.image.ostree_ref()).unwrap();
    drop(repo);
    assert_eq!(next, 2);
    let second = commit::commit(
        &tree,
        &plan,
        &record,
        &harness::manifest(),
        &CommitOptions {
            generation: next,
            ..options(&base)
        },
    )
    .unwrap();

    assert_eq!(second.generation, 2);
    assert_eq!(second.parent.as_deref(), Some(first.checksum.as_str()));

    let repo = commit::open_or_create(&base.join("repo")).unwrap();
    let history = commit::history(&repo, &first.reference).unwrap();
    let numbers: Vec<u64> = history.iter().map(|(_, m)| m.generation).collect();
    assert_eq!(numbers, [2, 1], "newest first");
}

/// Regression test: a generation removed from the *middle* must not take the
/// rest of the history with it.
///
/// `kiln clean` keeps the newest few and the baseline, so gen 1 stays while
/// 2..n-1 go, and the prune deletes their commit objects — leaving the
/// surviving newer commit naming a parent that is no longer stored. Walking
/// the parent chain from the tip then died at that hole, and `kiln show`,
/// `kiln diff` and `kiln rebuild` failed *for every generation*, including
/// ones sitting right there in the repository, with a `No such metadata
/// object` naming a commit the user never asked about.
#[test]
#[ignore = "privileged: a bare OSTree repository stores real ownership"]
fn a_pruned_generation_does_not_hide_the_ones_that_are_left() {
    if !harness::require_root("committing to a bare repository") {
        return;
    }
    let base = harness::scratch("ostree-pruned-middle");
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();
    let record = Record::of(&plan, 1, kiln_resolve::UidMap::new());
    let reference = plan.image.ostree_ref();

    let mut checksums = Vec::new();
    for generation in 1..=3 {
        std::fs::write(
            tree.join("usr/lib/os-release"),
            format!("NAME=Kiln\nID=kiln\nVERSION={generation}\n"),
        )
        .unwrap();
        let committed = commit::commit(
            &tree,
            &plan,
            &record,
            &harness::manifest(),
            &CommitOptions {
                generation,
                ..options(&base)
            },
        )
        .unwrap();
        checksums.push(committed.checksum);
    }

    let repo = commit::open_or_create(&base.join("repo")).unwrap();
    // What libostree does for a deployment it is keeping: a ref of its own, so
    // the commit survives the prune even once nothing else points at it. This
    // is why gen 1 is still readable on a machine whose gen 2 is long gone.
    repo.set_ref_immediate(
        None,
        "ostree/1/1/2",
        Some(&checksums[0]),
        gio::Cancellable::NONE,
    )
    .unwrap();
    drop(repo);

    // Prune generation 2 out of the middle, exactly as `kiln rm 2` would.
    let object = base
        .join("repo/objects")
        .join(&checksums[1][..2])
        .join(format!("{}.commit", &checksums[1][2..]));
    assert!(
        object.is_file(),
        "{} should exist to be pruned",
        object.display()
    );
    std::fs::remove_file(&object).unwrap();

    let repo = commit::open_or_create(&base.join("repo")).unwrap();
    let numbers: Vec<u64> = commit::history(&repo, &reference)
        .unwrap()
        .iter()
        .map(|(_, m)| m.generation)
        .collect();
    assert_eq!(
        numbers,
        [3, 1],
        "the hole ends a chain, it does not end the history"
    );
    let (checksum, _) = commit::find_generation(&repo, 1).unwrap();
    assert_eq!(
        checksum, checksums[0],
        "a generation a ref still holds open is findable with its chain broken"
    );
}

/// the commit filter rejects `/var` and `/boot`. Normalization already
/// emptied them and the contract verifier already said so — this is the third
/// check, and the only one that runs on the bytes actually being committed.
#[test]
#[ignore = "privileged: a bare OSTree repository stores real ownership"]
fn nothing_under_var_or_boot_reaches_the_commit() {
    if !harness::require_root("committing to a bare repository") {
        return;
    }
    let base = harness::scratch("ostree-filter");
    let tree = harness::image_tree(&base.join("tree"));
    // A tree that got past normalization with content it should not have.
    std::fs::write(tree.join("var/leftover"), "should not ship\n").unwrap();
    std::fs::write(tree.join("boot/vmlinuz-stale"), "should not ship\n").unwrap();

    let plan = harness::plan();
    let record = Record::of(&plan, 1, kiln_resolve::UidMap::new());
    let committed =
        commit::commit(&tree, &plan, &record, &harness::manifest(), &options(&base)).unwrap();

    let repo = commit::open_or_create(&base.join("repo")).unwrap();
    let listing = harness::list_commit(&repo, &committed.checksum);
    assert!(
        !listing
            .iter()
            .any(|p| p.starts_with("/var/") || p.starts_with("/boot/")),
        "{listing:#?}"
    );
    assert!(listing.iter().any(|p| p == "/usr/lib/os-release"));
}

/// Two builds of the same plan must produce the same commit. The commit
/// timestamp is wall clock, so `write_commit` would make them differ in it —
/// The same point, one level up from the tree.
#[test]
#[ignore = "privileged: a bare OSTree repository stores real ownership"]
fn the_same_tree_and_plan_produce_the_same_checksum() {
    if !harness::require_root("committing to a bare repository") {
        return;
    }
    let base = harness::scratch("ostree-determinism");
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();
    let record = Record::of(&plan, 1, kiln_resolve::UidMap::new());

    let first =
        commit::commit(&tree, &plan, &record, &harness::manifest(), &options(&base)).unwrap();
    // A second repository, so the parent is `None` both times: the point is the
    // *content*, and a parented commit legitimately differs.
    let other = harness::scratch("ostree-determinism-2");
    let mut opts = options(&other);
    opts.repo = other.join("repo");
    let second = commit::commit(&tree, &plan, &record, &harness::manifest(), &opts).unwrap();

    assert_eq!(first.checksum, second.checksum);
}

/// A commit that is not Kiln's should say so rather than produce a `Metadata`
/// full of empty strings. `kiln list` on a machine with an rpm-ostree
/// deployment beside a Kiln one is a real situation.
#[test]
fn a_foreign_commits_metadata_is_refused_with_the_checksum() {
    let empty = glib::VariantDict::new(None).end();
    let err = Metadata::from_variant(&empty, "a81fc2e1b4d9").unwrap_err();
    insta::assert_snapshot!(format!("{err}"));
}

/// The record is compressed because commit metadata is read on every
/// `kiln list`, and a few hundred kilobytes of JSON per commit is not that.
#[test]
fn the_record_round_trips_through_compression() {
    let plan = harness::plan();
    let record = Record::of(&plan, 7, kiln_resolve::UidMap::new());
    let json = record.to_json();
    let packed = generation::compress("kiln.record", json.as_bytes()).unwrap();
    assert!(packed.len() < json.len(), "compression should compress");
    assert_eq!(generation::decompress(&packed).unwrap(), json);
}

/// A record that will not decompress is a broken commit, not a reason for
/// `kiln list` to fail: the generation, the ids and the date are all still
/// there, and those are what the listing shows.
#[test]
fn a_corrupt_record_leaves_the_rest_of_the_metadata_readable() {
    let plan = harness::plan();
    let record = Record::of(&plan, 7, kiln_resolve::UidMap::new());
    let metadata = Metadata::of(&plan, 7, &record, &harness::manifest(), "forge");

    let dict = glib::VariantDict::new(Some(&metadata.to_variant().unwrap()));
    dict.insert("kiln.record", [0u8, 1, 2, 3].as_slice());
    let read = Metadata::from_variant(&dict.end(), "abc").unwrap();

    assert_eq!(read.generation, 7);
    assert_eq!(read.built_by, "forge");
    assert!(read.record.is_none());
}

fn options(base: &Path) -> CommitOptions {
    CommitOptions {
        repo: base.join("repo"),
        generation: 1,
        built_by: "forge".into(),
        subject: Some("kiln fixture".into()),
    }
}
