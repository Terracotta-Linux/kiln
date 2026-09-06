//! `plan_id` must not move by accident.
//!
//! The companion to `kiln-config`'s hash freeze, and the same contract: a
//! refactor must not change these values, and a deliberate change requires
//! bumping `HASH_EPOCH` **and** the expectation below in the same commit.
//! Pasting in the new value is never the fix — see the failure message.
//!
//! The plan here is built in code rather than resolved from the fixture, on
//! purpose. What is frozen is the *canonical encoding* of a `BuildPlan`; making
//! that depend on whether two runs of `makepkg` produce byte-identical archives
//! would turn a real regression and a zstd upgrade into the same red test.

use kiln_manifest::{Hash, HASH_EPOCH};
use kiln_resolve::{
    BuildPlan, ContentRef, EnableState, IdEntry, ImageRef, Provenance, ResolvedInput, SourcePin,
    UidMap, VolatileInput,
};

/// Frozen at hash epoch 4. `plan_id` embeds `HASH_EPOCH` directly, so an epoch
/// bump moves it whether or not anything about a *plan* changed — which is the
/// intent: the epoch exists to invalidate every cached identity at once. This
/// bump was `kernel.dracut_modules` joining `Kernel`'s canonical encoding; see
/// `kiln-config`'s `hash_freeze.rs` for why.
const FROZEN_AT_EPOCH: u32 = 4;

/// The plan as phase 2 could express it: repository packages, a file and a
/// unit.
///
/// This one is frozen **and must never grow**. Phase 3 added four input kinds,
/// and the claim in `ResolvedInput`'s documentation — that a plan containing
/// none of a new kind encodes exactly as it did before that kind existed — is
/// only worth anything if something checks it. Extending this specimen instead
/// of adding a second one would have quietly destroyed the evidence.
fn specimen() -> BuildPlan {
    let mut uid_map = UidMap::new();
    // A group with no user of its own name, and a user that owns one. Both
    // shapes are in the specimen because the seed renders them differently and
    // the encoding has to keep them apart.
    uid_map.groups.insert("systemd-journal".into(), 972);
    uid_map.groups.insert("systemd-network".into(), 977);
    uid_map.users.insert(
        "systemd-network".into(),
        IdEntry {
            uid: 977,
            gid: 977,
            home: "/".into(),
            shell: "/usr/bin/nologin".into(),
        },
    );

    let mut plan = BuildPlan {
        config_id: Hash("b3:11c4de".into()),
        image: ImageRef {
            name: "workstation".into(),
            arch: "x86_64".into(),
        },
        inputs: vec![
            ResolvedInput::Unit {
                name: "backup.timer".into(),
                content: ContentRef::Local {
                    path: "units/backup.timer".into(),
                    digest: Hash("b3:41ff".into()),
                },
                enable: EnableState::Enabled,
            },
            ResolvedInput::RepoPackage {
                name: "linux".into(),
                evr: "6.19.2-1".into(),
                filename: "linux-6.19.2-1-x86_64.pkg.tar.zst".into(),
                sha256: "3c9f".into(),
                repo: "core".into(),
                explicit: true,
            },
            ResolvedInput::File {
                target: "/etc/motd".into(),
                content: ContentRef::Inline {
                    digest: Hash("b3:6d2e".into()),
                },
                mode: Some(0o644),
            },
            ResolvedInput::RepoPackage {
                name: "glibc".into(),
                evr: "2.42-3".into(),
                filename: "glibc-2.42-3-x86_64.pkg.tar.zst".into(),
                sha256: "a10b".into(),
                repo: "core".into(),
                explicit: false,
            },
        ],
        volatile: vec![VolatileInput {
            input: "foo-git".into(),
            reason: "pkgver() runs upstream code".into(),
            what: kiln_resolve::Volatile::AurPackage {
                name: "foo-git".into(),
            },
        }],
        uid_map,
        provenance: Provenance {
            resolved_at: "2026-08-30T19:04:11Z".into(),
            snapshot: "2026-08-30".into(),
            repos: vec![("core".into(), vec!["https://example/core".into()])],
            libalpm: "16.0.1".into(),
        },
    };
    plan.canonicalize();
    plan
}

/// The phase-3 input kinds: AUR packages, built packages, a local package file
/// and an out-of-tree kernel module. Frozen separately so the encoding of each
/// is pinned without touching the specimen above.
fn phase_three_specimen() -> BuildPlan {
    let mut plan = specimen();
    plan.inputs.extend([
        ResolvedInput::AurPackage {
            name: "zen-browser-bin".into(),
            pkgbase: "zen-browser-bin".into(),
            evr: "1.16.3-1".into(),
            aur_commit: "3f1a9c8e".into(),
            srcinfo_hash: Hash("b3:aa01".into()),
            pulled_in_by: None,
        },
        ResolvedInput::AurPackage {
            name: "some-dependency".into(),
            pkgbase: "some-dependency".into(),
            evr: "0.4-2".into(),
            aur_commit: "9d02bb71".into(),
            srcinfo_hash: Hash("b3:aa02".into()),
            pulled_in_by: Some("zen-browser-bin".into()),
        },
        ResolvedInput::BuiltPackage {
            name: "my-tool".into(),
            path: "pkgbuilds/my-tool".into(),
            build_key: Hash("b3:cc41".into()),
            recipe: Hash("b3:dd12".into()),
            sources: vec![
                SourcePin {
                    url: "https://example.invalid/my-tool-1.0.tar.gz".into(),
                    sha256: "9f2c".into(),
                },
                SourcePin {
                    url: "my-tool.patch".into(),
                    sha256: "1b8e".into(),
                },
            ],
        },
        ResolvedInput::FilePackage {
            path: "packages/myapp-1.0-1-x86_64.pkg.tar.zst".into(),
            sha256: "9f2c1ab4".into(),
        },
        ResolvedInput::KernelModule {
            name: "v4l2loopback".into(),
            source: "modules/v4l2loopback".into(),
            build_key: Hash("b3:ee55".into()),
            recipe: Hash("b3:ff66".into()),
            kernel_evr: "6.19.2-1".into(),
        },
    ]);
    plan.canonicalize();
    plan
}

#[test]
fn plan_id_is_frozen() {
    assert_eq!(
        HASH_EPOCH, FROZEN_AT_EPOCH,
        "HASH_EPOCH moved without this test moving with it"
    );
    let got = specimen().plan_id();
    assert_eq!(
        got.to_string(),
        "b3:c6b275aad00fcb923c1b3fe96f74eba2b0809b676b85e141e0208d24962278b4",
        "\n\
         `plan_id` changed. There are exactly two legitimate causes:\n\
         \n\
           1. A bug — a refactor altered the canonical encoding without meaning to.\n\
              Find it. The encoding is the hash input; it is not free to drift.\n\
           2. A deliberate change to what a plan *is*. Then bump HASH_EPOCH and\n\
              FROZEN_AT_EPOCH together, in this commit, and say why in the message.\n\
         \n\
         Pasting the new value in is never the fix.\n"
    );
}

/// Adding an input kind must not disturb a plan that contains none of it.
///
/// This is the property that lets the taxonomy grow without invalidating every
/// build cache in existence. It holds because each variant's encoding is tagged
/// by name, so nothing about `RepoPackage` shifts when `AurPackage` appears
/// beside it in the enum — but "holds because" is an argument, and this is the
/// check.
///
/// The evidence is historical: phase 3 added four variants and this value, then
/// frozen at epoch 2, did not move. Epoch 3 moved it for an unrelated reason
/// (the UID seed), so what the assertion does *now* is catch the next kind
/// somebody adds. That is the same job, aimed forward.
#[test]
fn the_phase_three_input_kinds_did_not_move_a_phase_two_plan() {
    assert_eq!(
        specimen().plan_id().to_string(),
        "b3:c6b275aad00fcb923c1b3fe96f74eba2b0809b676b85e141e0208d24962278b4",
        "\nthis is the value frozen at epoch 3 with the phase-3 kinds already present: adding an input kind must not \
         invalidate a plan that uses none of it\n"
    );
}

#[test]
fn the_phase_three_input_kinds_are_frozen_too() {
    assert_eq!(HASH_EPOCH, FROZEN_AT_EPOCH);
    assert_eq!(
        phase_three_specimen().plan_id().to_string(),
        "b3:2eb39da992b2ccadeaf3c3d970862ac1334b07b77cc5358ac12fe27f1e39a761",
        "\nsee `plan_id_is_frozen` for the two legitimate reasons this can change\n"
    );
}

/// Each phase-3 kind must actually reach the identity — a variant that encodes
/// to nothing would silently stop tracking whatever it describes.
#[test]
fn every_phase_three_field_moves_the_identity() {
    let baseline = phase_three_specimen().plan_id();
    let mutate = |f: &dyn Fn(&mut ResolvedInput)| {
        let mut plan = phase_three_specimen();
        for input in plan.inputs.iter_mut() {
            f(input);
        }
        plan.canonicalize();
        plan.plan_id()
    };

    // the AUR commit *is* the identity. A force-push with the same pkgver
    // has to be visible, or the whole reason for tracking commits is gone.
    assert_ne!(
        mutate(&|i| {
            if let ResolvedInput::AurPackage { aur_commit, .. } = i {
                *aur_commit = "0000000".into();
            }
        }),
        baseline,
        "an AUR commit change must move plan_id even at the same version"
    );
    // bump the kernel and every out-of-tree module's key changes.
    assert_ne!(
        mutate(&|i| {
            if let ResolvedInput::KernelModule { kernel_evr, .. } = i {
                *kernel_evr = "6.19.3-1".into();
            }
        }),
        baseline
    );
    // a source pin is what makes a build reproducible.
    assert_ne!(
        mutate(&|i| {
            if let ResolvedInput::BuiltPackage { sources, .. } = i {
                sources[0].sha256 = "dead".into();
            }
        }),
        baseline
    );
    // an optional integrity guarantee is not a guarantee.
    assert_ne!(
        mutate(&|i| {
            if let ResolvedInput::FilePackage { sha256, .. } = i {
                *sha256 = "beef".into();
            }
        }),
        baseline
    );
    // which package dragged a dependency in is part of what the image is.
    assert_ne!(
        mutate(&|i| {
            if let ResolvedInput::AurPackage { pulled_in_by, .. } = i {
                if pulled_in_by.is_some() {
                    *pulled_in_by = Some("something-else".into());
                }
            }
        }),
        baseline
    );
}

/// The identity must not depend on the order the resolver happened to emit
/// inputs in, nor on facts about the resolution rather than its result.
#[test]
fn plan_id_ignores_input_order_and_resolution_facts() {
    let baseline = specimen().plan_id();

    let mut shuffled = specimen();
    shuffled.inputs.reverse();
    shuffled.canonicalize();
    assert_eq!(shuffled.plan_id(), baseline, "input order must not matter");

    let mut later = specimen();
    later.provenance = Provenance {
        resolved_at: "2031-01-01T00:00:00Z".into(),
        snapshot: "2031-01-01".into(),
        repos: vec![("core".into(), vec!["https://elsewhere/core".into()])],
        libalpm: "17.0.0".into(),
    };
    assert_eq!(
        later.plan_id(),
        baseline,
        "when and where a plan was resolved is not part of what it is"
    );

    // a volatile input is excluded from the identity precisely because
    // it could not be resolved. Including it would make `plan_id` a guess.
    let mut volatile = specimen();
    volatile.volatile.clear();
    assert_eq!(volatile.plan_id(), baseline);
}

/// …but everything the image is actually made of must move it.
#[test]
fn plan_id_tracks_every_part_of_the_image() {
    let baseline = specimen().plan_id();

    let mut one_less = specimen();
    one_less.inputs.pop();
    assert_ne!(one_less.plan_id(), baseline);

    let mut different_config = specimen();
    different_config.config_id = Hash("b3:000000".into());
    assert_ne!(different_config.plan_id(), baseline);

    let mut different_image = specimen();
    different_image.image.name = "laptop".into();
    assert_ne!(different_image.plan_id(), baseline);

    // a different UID seed is a different image, and honestly so — two
    // machines that pinned different IDs really do produce different trees.
    let mut different_uids = specimen();
    different_uids.uid_map.groups.insert("kvm".into(), 963);
    assert_ne!(different_uids.plan_id(), baseline);
}

/// A build script's *text* is the whole of its identity, so both halves
/// of that have to hold: adding one to a plan must move `plan_id`, and editing
/// one must move it again. Neither is automatic — a variant that encoded to
/// nothing would compile, resolve, and silently stop tracking the one input
/// whose effect is arbitrary.
///
/// The phase is in the identity too, because a script moved from `packages` to
/// `files` runs against a different tree (steps 5 and 8) and can produce a
/// different image from byte-identical text.
#[test]
fn a_build_script_reaches_the_identity_by_text_and_by_phase() {
    let baseline = specimen().plan_id();
    assert_eq!(
        baseline.to_string(),
        "b3:c6b275aad00fcb923c1b3fe96f74eba2b0809b676b85e141e0208d24962278b4",
        "the specimen has no scripts, so adding the kind must not have moved it"
    );

    let with_script = |text: &str, phase| {
        let mut plan = specimen();
        plan.inputs.push(ResolvedInput::BuildScript {
            name: "20-locale".into(),
            phase,
            content: ContentRef::Local {
                path: "scripts/20-locale.sh".into(),
                digest: Hash::of(text.as_bytes()),
            },
        });
        plan.canonicalize();
        plan.plan_id()
    };

    let original = with_script("locale-gen\n", kiln_manifest::ScriptPhase::Files);
    assert_ne!(original, baseline, "adding a script must move the identity");
    assert_ne!(
        with_script("locale-gen --purge\n", kiln_manifest::ScriptPhase::Files),
        original,
        "editing a script must move it"
    );
    assert_ne!(
        with_script("locale-gen\n", kiln_manifest::ScriptPhase::Packages),
        original,
        "moving a script between phases must move it"
    );
}
