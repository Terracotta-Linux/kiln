//! Resolution: manifest → BuildPlan, against the fixture repository.

mod harness;

use harness::*;
use kiln_resolve::{ContentRef, EnableState, ResolvedInput};

#[test]
fn resolves_the_smallest_bootable_image() {
    let plan = plan("resolve-bootable", BOOTABLE);
    let names: Vec<&str> = plan.packages().filter_map(|i| i.package_name()).collect();
    assert_eq!(names, ["fixture-init", "fixture-linux"]);
    assert_eq!(plan.image.ostree_ref(), "kiln/fixture/x86_64");
}

/// `plan_id` is the build identity. It must be a function of the
/// configuration's content and what that content resolved to — nothing else.
#[test]
fn plan_id_ignores_the_order_packages_were_written_in() {
    let forward = format!("{BOOTABLE}\n[systemd]\nenable = [\"a.service\", \"b.service\"]\n");
    let backward = format!("{BOOTABLE}\n[systemd]\nenable = [\"b.service\", \"a.service\"]\n");
    assert_eq!(
        plan("resolve-order-a", &forward).plan_id(),
        plan("resolve-order-b", &backward).plan_id()
    );
}

/// The date a rolling build resolved on moves every day. If it were part of the
/// identity, `kiln build` could never say "nothing to do".
#[test]
fn plan_id_excludes_the_resolution_date() {
    let mut a = plan("resolve-date-a", BOOTABLE);
    let before = a.plan_id();
    a.provenance.resolved_at = "1999-12-31T23:59:59Z".into();
    a.provenance.snapshot = "1999-12-31".into();
    assert_eq!(a.plan_id(), before);
}

/// …but it is still recorded, because that single field is what makes a past
/// image reconstructible without anyone having pinned anything.
#[test]
fn a_rolling_build_records_the_date_it_resolved_on() {
    let plan = plan("resolve-snapshot", BOOTABLE);
    assert_eq!(plan.provenance.snapshot.len(), "2026-08-30".len());
    assert!(plan.provenance.resolved_at.ends_with('Z'));
    assert!(plan
        .provenance
        .resolved_at
        .starts_with(&plan.provenance.snapshot));
}

/// Editing a file the configuration ships must change the build identity even
/// though no TOML changed.
#[test]
fn changing_a_shipped_file_changes_plan_id() {
    let toml = format!("{BOOTABLE}\n[[file]]\nsource = \"files/motd\"\ntarget = \"/etc/motd\"\n");
    let write = |name: &str, body: &str| plan_with(name, &toml, &[("files/motd", body)]);

    let one = write("resolve-file-a", "hello\n");
    let two = write("resolve-file-b", "goodbye\n");
    assert_ne!(one.config_id, two.config_id);
    assert_ne!(one.plan_id(), two.plan_id());
}

/// A file and a unit reach the plan as content identities, not bytes: the plan
/// is metadata, and assembly is what reads the disk.
#[test]
fn files_and_units_enter_the_plan_as_content_identities() {
    let toml = format!(
        r#"{BOOTABLE}
[systemd]
enable = ["fixture.service"]

[[systemd.unit]]
name = "myapp.service"
enable = true
content = """
[Unit]
Description=My app
"""

[[file]]
target = "/usr/lib/tmpfiles.d/scratch.conf"
content = "d /var/scratch 0755 root root 30d\n"
"#
    );
    let plan = plan("resolve-content", &toml);

    let file = plan
        .inputs
        .iter()
        .find(|i| matches!(i, ResolvedInput::File { .. }))
        .expect("the file entry reached the plan");
    match file {
        ResolvedInput::File {
            target, content, ..
        } => {
            assert_eq!(target, "/usr/lib/tmpfiles.d/scratch.conf");
            assert!(matches!(content, ContentRef::Inline { .. }));
        }
        _ => unreachable!(),
    }

    let unit = |name: &str| {
        plan.inputs.iter().find_map(|i| match i {
            ResolvedInput::Unit {
                name: n, enable, ..
            } if n == name => Some(*enable),
            _ => None,
        })
    };
    assert_eq!(unit("myapp.service"), Some(EnableState::Enabled));
    // A unit a *package* ships, enabled by name. It has no content of its own,
    // but the image differs, so it is an input.
    assert_eq!(unit("fixture.service"), Some(EnableState::Enabled));
}

/// Kiln leaves enable/disable/mask as three independent lists, so a unit can be
/// in more than one. The plan must say what the image will do, not restate the
/// argument.
#[test]
fn masking_beats_disabling_beats_enabling() {
    let toml = format!(
        "{BOOTABLE}\n[systemd]\nenable = [\"a.service\", \"b.service\"]\n\
         disable = [\"b.service\", \"c.service\"]\nmask = [\"c.service\"]\n"
    );
    let plan = plan("resolve-unit-state", &toml);
    let state = |name: &str| {
        plan.inputs.iter().find_map(|i| match i {
            ResolvedInput::Unit {
                name: n, enable, ..
            } if n == name => Some(*enable),
            _ => None,
        })
    };
    assert_eq!(state("a.service"), Some(EnableState::Enabled));
    assert_eq!(state("b.service"), Some(EnableState::Disabled));
    assert_eq!(state("c.service"), Some(EnableState::Masked));
}

/// every package carries a checksum so `kiln rebuild` can be satisfied
/// after mirrors have moved on.
#[test]
fn every_resolved_package_carries_its_filename_and_checksum() {
    let plan = plan("resolve-checksums", BOOTABLE);
    for input in plan.packages() {
        match input {
            ResolvedInput::RepoPackage {
                name,
                filename,
                sha256,
                evr,
                ..
            } => {
                assert!(!filename.is_empty(), "{name} has no filename");
                assert!(!sha256.is_empty(), "{name} has no checksum");
                assert!(!evr.is_empty(), "{name} has no version");
            }
            _ => unreachable!(),
        }
    }
}

/// the same configuration against moved mirrors must produce a different
/// build identity. This is the whole mechanism behind `kiln check`.
#[test]
fn a_moved_mirror_changes_plan_id_without_changing_config_id() {
    // `fixture-libfoo` is the package that differs between the two fixture
    // repositories; everything else is identical.
    let toml = BOOTABLE.replace(
        r#"repo = ["fixture-linux", "fixture-init"]"#,
        r#"repo = ["fixture-linux", "fixture-init", "fixture-libfoo"]"#,
    );
    let (m, dir) = manifest("resolve-moved", &toml);

    let cfg = dir.join("config");
    let aur = no_aur();
    let inputs = kiln_resolve::Inputs::new(&aur);
    let now = kiln_resolve::resolve(&m, &cfg, &options(&dir, "."), &inputs).unwrap();
    let next = kiln_resolve::resolve(&m, &cfg, &options(&dir, "next"), &inputs).unwrap();

    assert_eq!(
        now.config_id, next.config_id,
        "the configuration did not change"
    );
    assert_ne!(now.plan_id(), next.plan_id(), "but the packages did");
}
