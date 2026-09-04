//! The manifest as a *persisted* format.
//!
//! Every commit carries the manifest it was built from, in its metadata and at
//! `/usr/lib/kiln/manifest.json`, so that `kiln show <gen>` and `kiln rebuild
//! <gen>` work on a machine whose configuration has since been edited or
//! deleted — which is the normal case.
//!
//! That makes the JSON encoding an interface, and these are its two rules: a
//! manifest that goes in comes back out unchanged, and what comes back has the
//! same `config_id`. The second is the one that matters. `config_id` is what
//! `kiln rebuild` reconstructs a plan from, so a field that quietly failed to
//! round-trip would produce a rebuild that resolves to something else and says
//! nothing about why.

use kiln_manifest::{
    BootLoader, ExtraRepo, FileEntry, Initramfs, LocalPackage, Manifest, OutOfTreeModule, Script,
    ScriptPhase, Snapshot, UnitFile,
};

/// A manifest that uses every part of the schema, so the round-trip has
/// something to lose. A specimen with defaults everywhere would pass whatever
/// the encoding did.
fn specimen() -> Manifest {
    let mut m = Manifest::default();
    m.image.name = "workstation".into();
    m.image.arch = "x86_64".into();

    m.repos.snapshot = Snapshot::Date("2026-08-30".into());
    m.repos
        .mirrors
        .insert("https://mirror.example/$repo/os/$arch".into());
    m.repos.extra.insert(
        "local".into(),
        ExtraRepo {
            name: "local".into(),
            server: "file:///srv/local".into(),
            key: Some("ABCD1234".into()),
        },
    );

    m.packages.repo.insert("neovim".into());
    m.packages.repo.insert("firefox".into());
    m.packages.exclude.insert("nano".into());
    m.packages.file.insert(
        "pkgs/thing-1.0-1-x86_64.pkg.tar.zst".into(),
        LocalPackage {
            path: "pkgs/thing-1.0-1-x86_64.pkg.tar.zst".into(),
            sha256: "abc123".into(),
        },
    );

    m.kernel.package = "linux-lts".into();
    m.kernel.cmdline.insert("quiet".into());
    m.kernel.cmdline.insert("rw".into());
    m.kernel.modules.load.insert("kvm_intel".into());
    m.kernel.modules.blacklist.insert("nouveau".into());
    m.kernel.out_of_tree.insert(
        "v4l2loopback".into(),
        OutOfTreeModule {
            name: "v4l2loopback".into(),
            source: "modules/v4l2loopback".into(),
        },
    );

    m.boot.loader = BootLoader::Grub2;
    m.boot.timeout = 3;
    m.boot.initramfs = Initramfs::Dracut;

    m.systemd.enable.insert("sshd.service".into());
    m.systemd.mask.insert("bluetooth.service".into());
    m.systemd.units.insert(
        "backup.timer".into(),
        UnitFile {
            name: "backup.timer".into(),
            source: Some("units/backup.timer".into()),
            content: None,
            enable: true,
        },
    );

    m.files.insert(
        "/etc/motd".into(),
        FileEntry {
            target: "/etc/motd".into(),
            source: None,
            content: Some("welcome\n".into()),
            mode: Some(0o644),
        },
    );
    m.scripts.insert(
        "20-locale".into(),
        Script {
            name: "20-locale".into(),
            source: Some("scripts/20-locale.sh".into()),
            content: None,
            after: ScriptPhase::Packages,
        },
    );

    m.system.hostname = Some("forge".into());
    m.system.timezone = "Europe/Berlin".into();
    m.system.locale.lang = "en_US.UTF-8".into();
    m.system.locale.generate.insert("en_US.UTF-8 UTF-8".into());

    m.local_digests.insert(
        "scripts/20-locale.sh".into(),
        kiln_manifest::Hash::of(b"locale-gen\n"),
    );
    m
}

#[test]
fn a_manifest_survives_a_trip_through_json_with_its_identity_intact() {
    let original = specimen();
    let json = serde_json::to_string(&original).expect("a manifest serializes");
    let back: Manifest = serde_json::from_str(&json).expect("and comes back");

    assert_eq!(
        back.config_id(),
        original.config_id(),
        "a field that does not round-trip changes the identity a rebuild resolves from"
    );
}

/// Not just the identity: the fields a rebuild actually assembles from have to
/// arrive individually, because `config_id` is a hash and a hash that matches
/// tells you nothing about *which* field you are about to read.
#[test]
fn the_parts_assembly_reads_arrive_intact() {
    let json = serde_json::to_string(&specimen()).unwrap();
    let back: Manifest = serde_json::from_str(&json).unwrap();

    assert_eq!(back.repos.snapshot, Snapshot::Date("2026-08-30".into()));
    assert_eq!(
        back.files["/etc/motd"].content.as_deref(),
        Some("welcome\n")
    );
    assert_eq!(back.files["/etc/motd"].mode, Some(0o644));
    assert_eq!(back.scripts["20-locale"].after, ScriptPhase::Packages);
    assert_eq!(
        back.scripts["20-locale"].source.as_deref(),
        Some("scripts/20-locale.sh")
    );
    assert!(back.systemd.units["backup.timer"].enable);
    assert!(back.systemd.mask.contains("bluetooth.service"));
    assert_eq!(
        back.kernel.out_of_tree["v4l2loopback"].source,
        "modules/v4l2loopback"
    );
    assert_eq!(back.boot.loader, BootLoader::Grub2);
    assert_eq!(back.system.hostname.as_deref(), Some("forge"));
}

/// Spans point into files on the machine that built the image. A generation
/// read back out of a commit has no such files, and an origin naming
/// `desktop.toml:14` of a configuration that has since been rewritten is worse
/// than no origin at all — so they are deliberately not persisted, and this
/// says so rather than leaving it to be read out of a `#[serde(skip)]`.
#[test]
fn diagnostics_are_not_persisted_and_do_not_affect_the_identity() {
    let mut original = specimen();
    let json = serde_json::to_string(&original).unwrap();
    assert!(
        !json.contains("origins"),
        "provenance must not reach the persisted form:\n{json}"
    );

    let back: Manifest = serde_json::from_str(&json).unwrap();
    assert!(back.origins.is_empty());
    assert!(back.item_origins.is_empty());

    // And their absence is not a difference: `config_id` excludes them,
    // so a manifest read back out of a commit is the same manifest.
    original.origins = Default::default();
    original.item_origins = Default::default();
    assert_eq!(back.config_id(), original.config_id());
}

/// Two manifests are the same when their canonical encodings agree. Written out
/// rather than derived, so it gets a test: a `PartialEq` that compared
/// `origins` would call a rebuild's manifest different from the original's on
/// every machine.
#[test]
fn manifests_are_equal_when_their_identities_are() {
    let json = serde_json::to_string(&specimen()).unwrap();
    let back: Manifest = serde_json::from_str(&json).unwrap();
    assert_eq!(back, specimen());

    let mut different = specimen();
    different.boot.timeout = 10;
    assert_ne!(different, specimen());
}
