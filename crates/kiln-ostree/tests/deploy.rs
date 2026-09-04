//! Deployment, generations and rollback.
//!
//! **Privileged**, and against a real sysroot under `target/` rather than `/`.
//! It is not a boot test — nothing here proves the machine comes up — but it is
//! the whole of what `kiln list`, `kiln deploy` and `kiln rollback` do to the
//! deployment list, and that is what this file exercises.

mod harness;

use harness::manifest;
use kiln_ostree::commit::{self, CommitOptions};
use kiln_ostree::{deploy, Sysroot};
use kiln_record::Record;
use std::path::Path;

/// Commit `count` generations into a fresh sysroot and deploy each one.
fn sysroot_with(name: &str, count: u64) -> (Sysroot, Vec<String>) {
    let base = harness::scratch(name);
    let sysroot = Sysroot::init(&base).unwrap();
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();
    let opts = CommitOptions {
        repo: base.join("ostree/repo"),
        generation: 1,
        built_by: "forge".into(),
        subject: None,
    };

    let mut checksums = Vec::new();
    for generation in 1..=count {
        let opts = CommitOptions {
            generation,
            ..opts.clone()
        };
        std::fs::write(
            tree.join("usr/lib/os-release"),
            format!("NAME=Kiln\nID=kiln\nPRETTY_NAME=\"Kiln fixture\"\nVERSION={generation}\n"),
        )
        .unwrap();
        let record = Record::of(&plan, generation, kiln_resolve::UidMap::new());
        let committed = commit::commit(&tree, &plan, &record, &harness::manifest(), &opts).unwrap();
        sysroot
            .deploy_now(&committed.checksum, &manifest(), "fixture", 0)
            .unwrap();
        checksums.push(committed.checksum);
    }
    (sysroot, checksums)
}

#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn deploying_makes_the_new_generation_the_default() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, _) = sysroot_with("deploy-default", 2);

    let generations = sysroot.generations().unwrap();
    assert_eq!(generations.len(), 2);
    // The deployment list is in boot order, so the newest is first.
    assert_eq!(generations[0].number, 2);
    assert_eq!(generations[1].number, 1);
    assert!(generations[1].rollback_target);
    assert!(!generations[0].rollback_target);
}

/// `ostree admin rollback` does not exist: libostree has
/// `set-default`, `undeploy` and `pin`, and `set-default` is itself a
/// reordering of the deployment list rather than a call. `kiln rollback` is
/// Kiln's own operation, and this is what says so.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn rollback_makes_the_previous_generation_the_default() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, _) = sysroot_with("deploy-rollback", 3);
    assert_eq!(sysroot.generations().unwrap()[0].number, 3);

    let target = deploy::rollback(&sysroot).unwrap();
    assert_eq!(target.number, 2);

    let after = sysroot.generations().unwrap();
    assert_eq!(after[0].number, 2, "generation 2 now boots");
    // And rolling back again goes to 3, not to 1: "the previous one" is a
    // position in the boot order, not a step backwards through history.
    assert_eq!(deploy::rollback(&sysroot).unwrap().number, 3);
}

///, on a real sysroot rather than on synthetic entry files.
///
/// The entries libostree writes make the trap concrete. Both deployments share
/// a kernel, so both `ostree=` paths carry the same bootcsum and differ only in
/// the trailing deployment index — and the entry that boots, `ostree:0`, is
/// `ostree-2.conf`. Reading the directory and taking the first filename gets
/// `ostree-1.conf`, which is `ostree:1`: the rollback.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn the_bls_entries_agree_with_the_deployment_list() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, _) = sysroot_with("deploy-bls", 2);
    let boot = sysroot.boot();

    let entries = kiln_ostree::entries::read(&boot);
    assert_eq!(entries.len(), 2, "one entry per deployment");

    // In boot order the versions descend, and the filenames ascend. That is the
    // whole point, in two assertions.
    assert_eq!(
        entries.iter().map(|e| e.version).collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(
        entries
            .iter()
            .map(|e| e.filename.as_str())
            .collect::<Vec<_>>(),
        ["ostree-2.conf", "ostree-1.conf"]
    );

    // And the default entry names deployment index 0, which is the first
    // element of the deployment list.
    let default = kiln_ostree::entries::default(&boot).unwrap();
    let first = &sysroot.generations().unwrap()[0];
    assert_eq!(first.index, 0);
    assert!(
        default.options.ends_with("/0"),
        "the highest-version entry must name deployment 0: {}",
        default.options
    );
    assert!(entries[1].options.ends_with("/1"), "{}", entries[1].options);
}

/// The same thing after a rollback, which is where it would silently stop being
/// true: the deployments swap places, libostree rewrites the entries, and
/// anything caching the filename-to-generation mapping is now wrong.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn the_entries_still_agree_after_a_rollback() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, _) = sysroot_with("deploy-bls-rollback", 2);
    let before = default_title(&sysroot.boot());

    deploy::rollback(&sysroot).unwrap();

    let generations = sysroot.generations().unwrap();
    assert_eq!(generations[0].number, 1, "generation 1 now boots");
    assert_eq!(generations[0].index, 0);

    let default = kiln_ostree::entries::default(&sysroot.boot()).unwrap();
    assert!(default.options.ends_with("/0"), "{}", default.options);
    // The default *entry* is still `ostree:0` — what changed is which
    // deployment that is. An assertion on the title alone would have passed
    // whether or not the rollback did anything.
    assert_eq!(default_title(&sysroot.boot()), before);
    assert_ne!(generations[0].checksum, generations[1].checksum);
}

/// kargs are fully declarative. Every deploy passes the complete set from
/// `kernel.cmdline`, so removing a line from the TOML removes the karg —
/// unlike an append/delete model, where the live set drifts away from any
/// written source of truth and nothing can say what the machine boots with.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn removing_a_karg_from_the_manifest_removes_it_from_the_next_deployment() {
    if !harness::require_root("deploying") {
        return;
    }
    let base = harness::scratch("deploy-kargs");
    let sysroot = Sysroot::init(&base).unwrap();
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();
    let opts = CommitOptions {
        repo: base.join("ostree/repo"),
        generation: 1,
        built_by: "forge".into(),
        subject: None,
    };
    let record = Record::of(&plan, 1, kiln_resolve::UidMap::new());

    let first = commit::commit(&tree, &plan, &record, &harness::manifest(), &opts).unwrap();
    sysroot
        .deploy_now(&first.checksum, &manifest(), "fixture", 0)
        .unwrap();
    assert!(default_options(&sysroot.boot()).contains("quiet"));

    std::fs::write(
        tree.join("usr/lib/os-release"),
        "NAME=Kiln\nID=kiln\nPRETTY_NAME=\"Kiln fixture\"\nVERSION=2\n",
    )
    .unwrap();
    let second = commit::commit(&tree, &plan, &record, &harness::manifest(), &opts).unwrap();
    let mut without_quiet = manifest();
    without_quiet.kernel.cmdline.remove("quiet");
    sysroot
        .deploy_now(&second.checksum, &without_quiet, "fixture", 0)
        .unwrap();

    let options = default_options(&sysroot.boot());
    assert!(!options.contains("quiet"), "{options}");
    assert!(options.contains("rw"), "{options}");
}

/// generations are the only IDs the CLI accepts, and asking for one that
/// is not there should say what is.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn an_unknown_generation_lists_the_ones_that_exist() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, _) = sysroot_with("deploy-unknown", 2);
    let err = sysroot.set_default(99).unwrap_err();
    assert_eq!(
        format!("{err}"),
        "there is no generation 99; this machine has 2, 1"
    );
}

/// `kiln sysroot init` has to be safe to run twice, because an installer
/// that crashed halfway is exactly when someone runs it again.
#[test]
#[ignore = "privileged: initializing a sysroot needs root"]
fn initializing_a_sysroot_twice_is_not_an_error() {
    if !harness::require_root("initializing a sysroot") {
        return;
    }
    let base = harness::scratch("deploy-init-twice");
    Sysroot::init(&base).unwrap();
    Sysroot::init(&base).unwrap();
    assert!(base.join("ostree/repo/config").is_file());
}

fn default_title(boot: &Path) -> String {
    kiln_ostree::entries::default(boot)
        .expect("a default BLS entry")
        .title
}

fn default_options(boot: &Path) -> String {
    kiln_ostree::entries::default(boot)
        .expect("a default BLS entry")
        .options
}

// ── phase 4: removal, the baseline, and the boot counter ────────────────────

/// `kiln rm` removes exactly the generation named and leaves the
/// rest where they were.
///
/// Two things this pins down at once, and the second was a surprise:
///
/// **libostree prunes as it deploys.** `simple_write_deployment` with no RETAIN
/// flag keeps the new deployment and the previous default, and drops the rest.
/// Four deploys therefore leave three generations, not four — `[4, 3, 1]` — and
/// generation 1 is in that list *only because the initial deploy pinned it as
/// the baseline*.
/// Without the pin the floor would have been pruned away by the ordinary act of
/// building three more images, which is precisely the situation automatic
/// rollback needs to not be in.
///
/// **Removal is by generation, not by index.** `remove` pairs the `Generation`
/// list with libostree's `Deployment` list by position, and libostree renumbers
/// deployment indices on every write — so taking the middle one of three and
/// then asking which two survived is the shape of the bug that silently removes
/// the wrong image.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn removing_a_generation_takes_that_one_and_no_other() {
    if !harness::require_root("removing a deployment") {
        return;
    }
    let (sysroot, _) = sysroot_with("deploy-rm", 4);

    let before: Vec<u64> = sysroot
        .generations()
        .unwrap()
        .iter()
        .map(|g| g.number)
        .collect();
    assert_eq!(
        before,
        vec![4, 3, 1],
        "libostree retained a different set than expected; generation 1 is here because \
the baseline pin protects it, and the rest is `simple_write_deployment`'s own pruning"
    );

    // The middle one: neither the newest nor the baseline, and the position
    // where an off-by-one against the deployment list would be visible.
    sysroot.remove(&[3]).unwrap();

    let left: Vec<u64> = sysroot
        .generations()
        .unwrap()
        .iter()
        .map(|g| g.number)
        .collect();
    assert_eq!(left, vec![4, 1], "`kiln rm 3` removed the wrong deployment");
}

/// Generation 1 is pinned as the baseline by the deploy that creates it,
/// not by a later command noticing it is the oldest — so the protection is
/// there from the first moment there is something to protect.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn the_first_generation_is_pinned_as_the_baseline() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, _) = sysroot_with("deploy-baseline", 2);
    let generations = sysroot.generations().unwrap();

    let first = generations.iter().find(|g| g.number == 1).unwrap();
    assert!(first.baseline, "generation 1 is not marked as the baseline");
    assert!(
        first.pinned,
        "the baseline was not pinned when it was deployed"
    );

    let second = generations.iter().find(|g| g.number == 2).unwrap();
    assert!(!second.baseline);
    assert!(!second.pinned, "only the baseline is pinned automatically");
}

/// The counter is armed only when the image can clear it. The fixture
/// tree ships neither `grub-editenv` nor Kiln's boot-success script, so this is
/// the "cannot bless" case — and arming it anyway would put a working image on
/// probation it can never leave.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn an_image_that_cannot_clear_a_counter_is_never_given_one() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, checksums) = sysroot_with("deploy-counter", 1);
    assert!(!sysroot.can_bless(&checksums[0]));

    let deployed = sysroot
        .deploy_now(&checksums[0], &manifest(), "fixture", 3)
        .unwrap();
    assert_eq!(deployed.counted, kiln_ostree::Counter::ImageCannotBless);
    assert_eq!(
        kiln_ostree::grubenv::counting(sysroot.path(), 3),
        kiln_ostree::Counting::Off,
        "a counter was armed for an image with nothing to clear it"
    );
}

/// libostree's grub2 backend runs `grub-mkconfig` chrooted into
/// the deployment with a host-absolute output path, so it cannot work against a
/// sysroot that is not `/`. Getting this wrong is not a degradation — it is a
/// deploy that dies after the tree is already checked out, which is exactly how
/// it was found.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn a_sysroot_that_is_not_the_root_never_selects_the_grub2_backend() {
    if !harness::require_root("deploying") {
        return;
    }
    let (sysroot, checksums) = sysroot_with("deploy-backend", 1);
    assert_ne!(sysroot.path(), Path::new("/"), "the fixture is not /");
    assert_eq!(
        sysroot.backend_for(&checksums[0]),
        kiln_ostree::Backend::None
    );

    // And it is written down, rather than left to libostree's `auto` to guess
    // from whether a grub.cfg happens to exist yet.
    let config = std::fs::read_to_string(sysroot.path().join("ostree/repo/config")).unwrap();
    assert!(
        config.contains("bootloader=none"),
        "the sysroot's repo config does not pin the bootloader:\n{config}"
    );
}

/// `kiln sysroot init` on a target that was already **built into** must
/// work, and must leave the commits that are there deployable.
///
/// This is the recovery path for following the installer's table out of order, which is
/// an easy thing to do: `kiln build --sysroot /mnt` creates `ostree/repo` by
/// itself and succeeds, so the mistake is invisible until `kiln deploy` fails
/// on a missing `ostree/deploy`. Nothing about that is unrecoverable — the
/// commits are in the repository with their generation numbers — but "nothing
/// is lost, run one command" is only true if this test says so.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn initializing_a_sysroot_that_was_already_built_into_keeps_its_commits() {
    if !harness::require_root("initializing a sysroot") {
        return;
    }
    let base = harness::scratch("deploy-init-after-build");
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();

    // Commit straight into `<base>/ostree/repo` with no `sysroot init` first,
    // exactly as `kiln build --sysroot` does.
    let committed = commit::commit(
        &tree,
        &plan,
        &Record::of(&plan, 1, kiln_resolve::UidMap::new()),
        &harness::manifest(),
        &CommitOptions {
            repo: base.join("ostree/repo"),
            generation: 1,
            built_by: "installer".into(),
            subject: None,
        },
    )
    .unwrap();
    assert!(base.join("ostree/repo").exists());
    assert!(
        !base.join("ostree/deploy").exists(),
        "a plain commit must not create the sysroot layout — that is the bug this covers"
    );

    // The recovery, and it takes no argument saying "there is already a repo
    // here".
    let sysroot = Sysroot::init(&base).unwrap();
    sysroot
        .deploy_now(&committed.checksum, &manifest(), "fixture", 0)
        .unwrap();

    let generations = sysroot.generations().unwrap();
    assert_eq!(generations.len(), 1);
    assert_eq!(generations[0].number, 1, "the commit kept its generation");
    assert_eq!(generations[0].checksum, committed.checksum);
}

/// The installer's sequence: `sysroot init`, `build`, then
/// `deploy <gen>` — where the generation being named has only ever been
/// **committed**, because building is not deploying.
///
/// This is the path `kiln deploy` could not walk. `set_default` reorders the
/// deployment *list*, so on a target where nothing is deployed yet it answered
/// `there is no generation 1; this machine has no Kiln deployments yet` about a
/// generation sitting in the repository. Everything the deploy needs is in the
/// commit — puts the manifest in its metadata precisely so that kargs come
/// from the generation being deployed rather than from whatever `/etc/kiln`
/// says today.
#[test]
#[ignore = "privileged: deploying writes a real OSTree sysroot"]
fn a_generation_that_was_only_ever_committed_can_still_be_deployed() {
    if !harness::require_root("deploying") {
        return;
    }
    let base = harness::scratch("deploy-committed-only");
    let sysroot = Sysroot::init(&base).unwrap();
    let tree = harness::image_tree(&base.join("tree"));
    let plan = harness::plan();

    let committed = commit::commit(
        &tree,
        &plan,
        &Record::of(&plan, 1, kiln_resolve::UidMap::new()),
        &manifest(),
        &CommitOptions {
            repo: base.join("ostree/repo"),
            generation: 1,
            built_by: "installer".into(),
            subject: None,
        },
    )
    .unwrap();

    // What `kiln build --sysroot /mnt` leaves: a commit, and nothing deployed.
    assert!(sysroot.generations().unwrap().is_empty());

    // The commit is findable by generation even though no deployment is, which
    // is what makes the fallback possible at all.
    let (checksum, metadata) =
        commit::find_generation(&sysroot.repo(), 1).expect("the commit is there by generation");
    assert_eq!(checksum, committed.checksum);

    // And the kargs come with it, from the commit rather than from disk.
    let recorded = metadata
        .manifest
        .expect("every commit carries its manifest");
    assert_eq!(deploy::kargs(&recorded), deploy::kargs(&manifest()));

    sysroot
        .deploy_now(&checksum, &recorded, &metadata.image, 0)
        .unwrap();

    let generations = sysroot.generations().unwrap();
    assert_eq!(generations.len(), 1);
    assert_eq!(generations[0].number, 1);
    assert!(
        generations[0].baseline,
        "the first generation an installer deploys is the baseline"
    );
}
