//! Unit state. step 7.
//!
//! Enablement is preset files plus one offline `systemctl preset-all`, so the
//! testable surface is the preset's bytes, what gets refused, and what the
//! host tools were asked to do.

mod scratch;

use kiln_image::overlay::{NoOwners, Owners};
use kiln_image::units::{self, Applied, Systemctl};
use kiln_manifest::UnitFile;
use kiln_resolve::EnableState;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn unit(name: &str, body: &str) -> (String, UnitFile) {
    (
        name.to_string(),
        UnitFile {
            name: name.to_string(),
            source: None,
            content: Some(body.to_string()),
            enable: false,
        },
    )
}

fn states(pairs: &[(&str, EnableState)]) -> BTreeMap<String, EnableState> {
    pairs.iter().map(|(n, s)| (n.to_string(), *s)).collect()
}

fn go(
    root: &Path,
    states: &BTreeMap<String, EnableState>,
    files: &BTreeMap<String, UnitFile>,
    spy: &Spy,
) -> kiln_image::Result<Applied> {
    units::apply(
        root,
        Path::new("/nonexistent"),
        states,
        files,
        &NoOwners,
        spy,
    )
}

/// A package ships the unit; Kiln only decides whether it runs.
fn package_ships(root: &Path, name: &str) {
    scratch::file(
        root,
        &format!("{}/{name}", units::UNIT_DIR),
        "[Unit]\nDescription=from a package\n",
        0o644,
    );
}

#[test]
fn the_preset_file_is_what_decides_enablement() {
    let root = scratch::root("units-preset");
    package_ships(&root, "sshd.socket");
    package_ships(&root, "bluetooth.service");
    let spy = Spy::default();

    go(
        &root,
        &states(&[
            ("sshd.socket", EnableState::Enabled),
            ("bluetooth.service", EnableState::Disabled),
        ]),
        &BTreeMap::new(),
        &spy,
    )
    .unwrap();

    insta::assert_snapshot!(std::fs::read_to_string(root.join(units::PRESET_PATH)).unwrap());
}

/// `20-` is the whole mechanism. Preset files are read in lexicographic order
/// and the first matching line wins, so Kiln's file has to sort ahead of the
/// `90-systemd.preset` and `disable *` default that Arch ships. A rename to
/// `kiln.preset` would silently stop working.
#[test]
fn the_preset_sorts_ahead_of_everything_arch_ships() {
    let name = Path::new(units::PRESET_PATH)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap();
    assert!(name.starts_with("20-"));
    assert!(name < "90-systemd.preset");
    assert!(name < "99-default.preset");
}

/// Masking is a symlink to `/dev/null` in the administrator's directory, which
/// is where `systemctl mask` puts one — so `systemctl unmask` on the booted
/// machine works the way it does everywhere else.
#[test]
fn a_mask_is_a_symlink_the_administrator_can_undo() {
    let root = scratch::root("units-mask");
    package_ships(&root, "systemd-resolved.service");
    let spy = Spy::default();

    go(
        &root,
        &states(&[("systemd-resolved.service", EnableState::Masked)]),
        &BTreeMap::new(),
        &spy,
    )
    .unwrap();

    let link = root.join(units::ADMIN_DIR).join("systemd-resolved.service");
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("/dev/null"));
    let preset = std::fs::read_to_string(root.join(units::PRESET_PATH)).unwrap();
    assert!(
        !preset.contains("systemd-resolved"),
        "a mask says it once, in the place the administrator can reach:\n{preset}"
    );
}

/// The mask has to exist before `preset-all` runs, or some *other* unit's
/// preset can hang a `.wants` symlink on a unit that is supposed to be masked.
#[test]
fn masks_are_written_before_preset_all() {
    let root = scratch::root("units-order");
    package_ships(&root, "avahi-daemon.service");
    let spy = Spy::default();

    go(
        &root,
        &states(&[("avahi-daemon.service", EnableState::Masked)]),
        &BTreeMap::new(),
        &spy,
    )
    .unwrap();

    assert_eq!(spy.saw_mask_at_preset_time(), Some(true));
}

/// Enabling a unit no package provides is a hard error — otherwise the
/// image boots without it and nothing ever says why.
#[test]
fn enabling_a_unit_nothing_provides_is_refused_with_a_near_miss() {
    let root = scratch::root("units-missing");
    package_ships(&root, "sshd.socket");
    let spy = Spy::default();

    let err = go(
        &root,
        &states(&[("sshd.sockte", EnableState::Enabled)]),
        &BTreeMap::new(),
        &spy,
    )
    .unwrap_err();
    insta::assert_snapshot!(format!("{err}"));
    assert_eq!(spy.presets(), Vec::<PathBuf>::new(), "nothing ran");
}

/// A unit Kiln ships counts as provided. The existence check runs after
/// shipping for exactly this reason.
#[test]
fn a_kiln_shipped_unit_can_be_enabled() {
    let root = scratch::root("units-shipped");
    let spy = Spy::default();
    let files = BTreeMap::from([unit(
        "myapp.service",
        "[Service]\nExecStart=/usr/bin/myapp\n",
    )]);

    let applied = go(
        &root,
        &states(&[("myapp.service", EnableState::Enabled)]),
        &files,
        &spy,
    )
    .unwrap();

    assert_eq!(applied.shipped, ["myapp.service"]);
    assert!(root.join(units::UNIT_DIR).join("myapp.service").is_file());
    assert!(std::fs::read_to_string(root.join(units::PRESET_PATH))
        .unwrap()
        .contains("enable myapp.service\n"));
}

/// `foo@bar.service` is an instance of the template `foo@.service`. A naive
/// filename check refuses every templated unit in existence.
#[test]
fn an_instance_is_provided_by_its_template() {
    let root = scratch::root("units-template");
    package_ships(&root, "getty@.service");
    let spy = Spy::default();

    go(
        &root,
        &states(&[("getty@tty1.service", EnableState::Enabled)]),
        &BTreeMap::new(),
        &spy,
    )
    .unwrap();
}

/// A unit can exist under a name that is not any file's name. `Alias=` is the
/// second way, and refusing it would reject `enable = ["dbus.service"]` on an
/// image where the file is `dbus-broker.service`.
#[test]
fn an_alias_counts_as_provided() {
    let root = scratch::root("units-alias");
    scratch::file(
        &root,
        &format!("{}/dbus-broker.service", units::UNIT_DIR),
        "[Unit]\nDescription=D-Bus\n\n[Install]\nAlias=dbus.service\n",
        0o644,
    );
    let spy = Spy::default();

    go(
        &root,
        &states(&[("dbus.service", EnableState::Enabled)]),
        &BTreeMap::new(),
        &spy,
    )
    .unwrap();
}

/// Same rule as the overlay, reached through a different key: shipping a unit
/// on top of a package's file is refused, and the diagnostic points at the
/// drop-in, because that is what the user almost always wanted.
#[test]
fn shipping_a_unit_over_a_packages_own_is_refused_and_points_at_the_drop_in() {
    let root = scratch::root("units-conflict");
    let spy = Spy::default();
    let files = BTreeMap::from([unit(
        "sshd.service",
        "[Service]\nExecStart=/usr/bin/sshd -D\n",
    )]);

    let err = units::apply(
        &root,
        Path::new("/nonexistent"),
        &BTreeMap::new(),
        &files,
        &Owned("openssh"),
        &spy,
    )
    .unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("owned by the package `openssh`"), "{text}");
    assert!(text.contains("sshd.service.d/10-kiln.conf"), "{text}");
}

/// The tools are pointed at the staging root. Running `preset-all` against `/`
/// would enable units on the *builder*, which is the worst thing in this
/// module and the reason the calls are behind a trait.
#[test]
fn preset_all_is_pointed_at_the_staging_root() {
    let root = scratch::root("units-root");
    let spy = Spy::default();
    go(&root, &BTreeMap::new(), &BTreeMap::new(), &spy).unwrap();
    assert_eq!(spy.presets(), vec![root]);
}

/// `systemd-analyze verify` complains about units whose `Requires=` it cannot
/// resolve in a tree being assembled. Those are warnings, and failing the build
/// on them would make the feature unusable.
#[test]
fn verify_output_is_warnings_not_errors() {
    let root = scratch::root("units-verify");
    let spy = Spy::noisy();
    let files = BTreeMap::from([unit(
        "myapp.service",
        "[Service]\nExecStart=/usr/bin/myapp\n",
    )]);

    let applied = go(&root, &BTreeMap::new(), &files, &spy).unwrap();
    assert_eq!(
        applied.warnings,
        ["myapp.service: Requires= references unit"]
    );
}

#[derive(Default)]
struct Spy {
    presets: RefCell<Vec<PathBuf>>,
    /// Whether the mask symlink was on disk when `preset_all` was called.
    mask_at_preset: RefCell<Option<bool>>,
    complaints: Vec<String>,
}

impl Spy {
    fn noisy() -> Spy {
        Spy {
            complaints: vec!["myapp.service: Requires= references unit".into()],
            ..Spy::default()
        }
    }

    fn presets(&self) -> Vec<PathBuf> {
        self.presets.borrow().clone()
    }

    fn saw_mask_at_preset_time(&self) -> Option<bool> {
        *self.mask_at_preset.borrow()
    }
}

impl Systemctl for Spy {
    fn preset_all(&self, root: &Path) -> kiln_image::Result<()> {
        let masks = kiln_image::tree::entries(&root.join(units::ADMIN_DIR)).unwrap_or_default();
        *self.mask_at_preset.borrow_mut() = Some(!masks.is_empty());
        self.presets.borrow_mut().push(root.to_path_buf());
        Ok(())
    }

    fn verify(&self, _: &Path, _: &[String]) -> kiln_image::Result<Vec<String>> {
        Ok(self.complaints.clone())
    }
}

struct Owned(&'static str);

impl Owners for Owned {
    fn owner_of(&self, _: &str) -> Option<String> {
        Some(self.0.to_string())
    }
}
