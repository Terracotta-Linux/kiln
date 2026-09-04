//! The build record.
//!
//! There is no lockfile, so this file is the whole persistence story: what a
//! commit remembers about itself, and what a later Kiln can do with it.

use kiln_manifest::Hash;
use kiln_record::{Record, RecordedIds, FORMAT};
use kiln_resolve::{
    BuildPlan, ContentRef, EnableState, IdEntry, ImageRef, Provenance, ResolvedInput, SourcePin,
    UidMap,
};

fn uid_map() -> UidMap {
    let mut map = UidMap::new();
    map.groups.insert("systemd-journal".into(), 972);
    map.groups.insert("http".into(), 33);
    map.users.insert(
        "http".into(),
        IdEntry {
            uid: 33,
            gid: 33,
            home: "/srv/http".into(),
            shell: "/usr/bin/nologin".into(),
        },
    );
    map
}

fn plan() -> BuildPlan {
    let mut plan = BuildPlan {
        config_id: Hash("b3:11c4de".into()),
        image: ImageRef {
            name: "workstation".into(),
            arch: "x86_64".into(),
        },
        inputs: vec![
            ResolvedInput::RepoPackage {
                name: "linux".into(),
                evr: "6.19.2-1".into(),
                filename: "linux-6.19.2-1-x86_64.pkg.tar.zst".into(),
                sha256: "3c9f".into(),
                repo: "core".into(),
                explicit: true,
            },
            ResolvedInput::AurPackage {
                name: "zen-browser-bin".into(),
                pkgbase: "zen-browser-bin".into(),
                evr: "1.16.3-1".into(),
                aur_commit: "3f1a9c8e".into(),
                srcinfo_hash: Hash("b3:aa11".into()),
                pulled_in_by: None,
            },
            ResolvedInput::KernelModule {
                name: "v4l2loopback".into(),
                source: "modules/v4l2loopback".into(),
                build_key: Hash("b3:cc41".into()),
                recipe: Hash("b3:dd52".into()),
                kernel_evr: "6.19.2-1".into(),
            },
            ResolvedInput::BuiltPackage {
                name: "myapp".into(),
                path: "pkgbuilds/myapp".into(),
                build_key: Hash("b3:ee55".into()),
                recipe: Hash("b3:ff66".into()),
                sources: vec![SourcePin {
                    url: "https://example.invalid/myapp-1.0.tar.gz".into(),
                    sha256: "1a2b".into(),
                }],
            },
            ResolvedInput::FilePackage {
                path: "packages/myapp-1.0-1-x86_64.pkg.tar.zst".into(),
                sha256: "9f2c1ab4".into(),
            },
            ResolvedInput::File {
                target: "/etc/motd".into(),
                content: ContentRef::Local {
                    path: "files/motd".into(),
                    digest: Hash("b3:6d2e".into()),
                },
                mode: None,
            },
            ResolvedInput::Unit {
                name: "sshd.socket".into(),
                content: ContentRef::Inline {
                    digest: Hash::of(b""),
                },
                enable: EnableState::Enabled,
            },
        ],
        volatile: Vec::new(),
        uid_map: UidMap::new(),
        provenance: Provenance {
            resolved_at: "2026-08-30T19:04:11Z".into(),
            snapshot: "2026-08-30".into(),
            repos: vec![(
                "core".into(),
                vec!["https://geo.mirror.pkgbuild.com/core/os/x86_64".into()],
            )],
            libalpm: "16.0.1".into(),
        },
    };
    plan.canonicalize();
    plan
}

#[test]
fn the_record_of_a_plan() {
    insta::assert_snapshot!(Record::of(&plan(), 42, uid_map()).to_json());
}

#[test]
fn a_record_round_trips() {
    let record = Record::of(&plan(), 42, uid_map());
    assert_eq!(Record::parse(&record.to_json()).unwrap(), record);
}

/// The plan carries the *seed* — what the previous generation asked for
/// — and the record has to carry what this generation actually has. Recording
/// the seed instead would mean a service account allocated during the
/// transaction was never pinned and moved again next time, which is precisely
/// the drift the mechanism exists to stop.
#[test]
fn the_record_carries_the_captured_ids_not_the_seeded_ones() {
    let mut plan = plan();
    plan.uid_map.groups.insert("stale".into(), 900);

    let record = Record::of(&plan, 42, uid_map());
    assert!(!record.uid_map.groups.contains_key("stale"));
    assert_eq!(record.next_seed(), uid_map());

    // And the seed is recorded too, separately. The two answer different
    // questions: what the next generation replays, and what this one did.
    assert_eq!(record.uid_seed.groups.get("stale"), Some(&900));
    assert_eq!(record.seeded_with(), plan.uid_map);
}

/// the snapshot date is recorded even in rolling mode. That single field
/// is what makes a past image reconstructible without anyone having pinned
/// anything in advance — `kiln rebuild` points the Archive mirrors at it.
#[test]
fn the_snapshot_date_is_recorded_even_when_tracking_live_mirrors() {
    let record = Record::of(&plan(), 42, uid_map());
    assert_eq!(record.repos.snapshot, "2026-08-30");
}

/// change detection compares the booted deployment's recorded `plan_id`
/// against a freshly computed one. If the record did not carry it verbatim,
/// `kiln build` could not refuse a no-op.
#[test]
fn the_plan_id_survives_the_round_trip_exactly() {
    let plan = plan();
    let record = Record::parse(&Record::of(&plan, 1, UidMap::new()).to_json()).unwrap();
    assert_eq!(record.plan_id(), plan.plan_id());
}

/// A record written by a newer Kiln is refused, not half-read. It drives UID
/// replay and `kiln rebuild`; half-understanding one produces a wrong image
/// rather than an error, and a wrong image is discovered at boot.
#[test]
fn a_record_from_a_newer_kiln_is_refused_with_advice() {
    let mut json = Record::of(&plan(), 42, uid_map()).to_json();
    json = json.replace(
        &format!("\"kiln\": {FORMAT}"),
        &format!("\"kiln\": {}", FORMAT + 1),
    );
    let err = Record::parse(&json).unwrap_err();
    insta::assert_snapshot!(format!("{err}"));
}

/// Adding a field is not a format change: serde ignores what it does not know
/// and fills what is missing. Bumping `FORMAT` for every new field would make
/// every older Kiln refuse every newer image for no reason.
#[test]
fn an_unknown_field_is_ignored_rather_than_refused() {
    let json = Record::of(&plan(), 42, uid_map()).to_json().replacen(
        '{',
        "{\n  \"something_from_the_future\": [1, 2, 3],",
        1,
    );
    assert_eq!(Record::parse(&json).unwrap().generation, 42);
}

/// A local `.pkg.tar.zst` is pinned by sha256, a config-tree file by blake3.
/// One list with a field named `blake3` holding a sha256 is the kind of thing
/// that is read wrong once and then trusted.
#[test]
fn a_local_package_and_a_local_file_are_not_the_same_list() {
    let record = Record::of(&plan(), 42, uid_map());
    assert_eq!(record.local_packages.len(), 1);
    assert_eq!(record.local_packages[0].sha256, "9f2c1ab4");
    assert_eq!(record.local_files.len(), 1);
    assert_eq!(record.local_files[0].blake3, "b3:6d2e");
}

/// The two shapes of `UidMap` are allowed to disagree, and this is where that
/// gets noticed. Deriving `Serialize` on the in-memory type would mean a
/// refactor there silently changed the on-disk format of every image ever
/// built.
#[test]
fn the_recorded_ids_are_a_separate_shape_that_converts_both_ways() {
    let ids = RecordedIds::from(&uid_map());
    assert_eq!(UidMap::from(&ids), uid_map());
}
