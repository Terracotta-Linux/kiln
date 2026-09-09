//! `[system]`: hostname, timezone, keymap, locale.

mod scratch;

use kiln_manifest::{Locale, SystemDefaults};
use kiln_sandbox::Network;
use std::collections::BTreeSet;

fn zoneinfo(root: &std::path::Path, tz: &str) {
    scratch::file(root, &format!("usr/share/zoneinfo/{tz}"), "TZif", 0o644);
}

#[test]
fn locale_gen_is_one_line_per_entry() {
    let generate: BTreeSet<String> =
        ["en_US.UTF-8 UTF-8".into(), "de_DE.UTF-8 UTF-8".into()].into();
    assert_eq!(
        kiln_image::system::locale_gen_conf(&generate),
        "de_DE.UTF-8 UTF-8\nen_US.UTF-8 UTF-8\n"
    );
}

/// The three keys with a concrete default are written even when the
/// configuration never touched `[system]` at all — there is no unset state
/// for them to fall back to.
#[test]
fn defaults_are_materialized_with_no_system_table_at_all() {
    let root = scratch::root("system-defaults");
    zoneinfo(&root, "UTC");

    kiln_image::system::install(&root, &SystemDefaults::default()).unwrap();

    assert!(!root.join(kiln_image::system::HOSTNAME_PATH).exists());
    assert!(!root.join(kiln_image::system::LOCALE_GEN_PATH).exists());
    assert_eq!(
        std::fs::read_link(root.join(kiln_image::system::LOCALTIME_PATH)).unwrap(),
        std::path::Path::new("/usr/share/zoneinfo/UTC")
    );
    assert_eq!(
        std::fs::read_to_string(root.join(kiln_image::system::VCONSOLE_PATH)).unwrap(),
        "KEYMAP=us\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(kiln_image::system::LOCALE_CONF_PATH)).unwrap(),
        "LANG=C.UTF-8\n"
    );
}

#[test]
fn hostname_is_written_only_when_set() {
    let root = scratch::root("system-hostname");
    zoneinfo(&root, "UTC");
    let system = SystemDefaults {
        hostname: Some("forge".to_string()),
        ..SystemDefaults::default()
    };

    kiln_image::system::install(&root, &system).unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join(kiln_image::system::HOSTNAME_PATH)).unwrap(),
        "forge\n"
    );
}

#[test]
fn locale_gen_is_written_only_when_generate_is_non_empty() {
    let root = scratch::root("system-locale-gen");
    zoneinfo(&root, "UTC");
    let system = SystemDefaults {
        locale: Locale {
            lang: "en_US.UTF-8".to_string(),
            generate: ["en_US.UTF-8 UTF-8".to_string()].into(),
        },
        ..SystemDefaults::default()
    };

    assert!(kiln_image::system::needs_locale_gen(&system));
    kiln_image::system::install(&root, &system).unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join(kiln_image::system::LOCALE_GEN_PATH)).unwrap(),
        "en_US.UTF-8 UTF-8\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(kiln_image::system::LOCALE_CONF_PATH)).unwrap(),
        "LANG=en_US.UTF-8\n"
    );
}

/// The symlink target is a path in the *image*, resolved against
/// `/usr/share/zoneinfo` at boot — never the staging root it was written
/// into.
#[test]
fn timezone_becomes_an_absolute_symlink_into_the_image() {
    let root = scratch::root("system-timezone-symlink");
    zoneinfo(&root, "Asia/Riyadh");
    let system = SystemDefaults {
        timezone: "Asia/Riyadh".to_string(),
        ..SystemDefaults::default()
    };

    kiln_image::system::install(&root, &system).unwrap();

    assert_eq!(
        std::fs::read_link(root.join(kiln_image::system::LOCALTIME_PATH)).unwrap(),
        std::path::Path::new("/usr/share/zoneinfo/Asia/Riyadh")
    );
}

/// `ln -s` would happily create a symlink pointing nowhere; a mistyped
/// `system.timezone` deserves a build failure that names the problem, not a
/// dangling `/etc/localtime` that silently reads as UTC on the booted
/// machine.
#[test]
fn an_unknown_timezone_is_rejected_rather_than_a_dangling_symlink() {
    let root = scratch::root("system-timezone-missing");
    let system = SystemDefaults {
        timezone: "Nowhere/City".to_string(),
        ..SystemDefaults::default()
    };

    let err = kiln_image::system::install(&root, &system).unwrap_err();
    assert!(err.to_string().contains("Nowhere/City"), "{err}");
    assert!(!root.join(kiln_image::system::LOCALTIME_PATH).exists());
}

#[test]
fn locale_gen_spec_is_sandboxed_with_no_network() {
    let root = scratch::root("system-locale-gen-spec");
    let spec = kiln_image::system::locale_gen_spec(&root);
    assert_eq!(spec.network, Network::Disabled);
    assert_eq!(spec.command, vec!["locale-gen".to_string()]);
    assert_eq!(spec.root, root);
}
