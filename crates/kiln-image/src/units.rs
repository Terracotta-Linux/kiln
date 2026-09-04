//! Step 7: unit state. step 7.
//!
//! Enablement is realized as **preset files**, not by running `systemctl enable`
//! in a chroot. `systemctl preset-all --root=<staging>` materializes the
//! `.wants` symlinks offline, with no running systemd and no pid 1 in the
//! staging root to talk to.
//!
//! Two concerns stay separate, as the schema keeps them: shipping a unit file
//! and deciding whether a unit runs. A package's unit can be enabled without
//! Kiln shipping anything, and Kiln can ship a unit without enabling it.

use crate::overlay::{Owners, Refusal};
use crate::tree::{self, Error, Result};
use kiln_manifest::UnitFile;
use kiln_resolve::EnableState;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Where shipped units go. `/usr/lib`, not `/etc`: they are image content, and
/// `/etc/systemd/system` is where an *administrator* overrides them.
pub const UNIT_DIR: &str = "usr/lib/systemd/system";

/// Where a mask goes — the same place `systemctl mask` puts one, so that
/// `systemctl unmask` on the booted machine works the way it does everywhere
/// else. It lands in `/usr/etc` after normalization and is 3-way-merged at
/// deploy, which means the administrator can take it back.
pub const ADMIN_DIR: &str = "etc/systemd/system";

/// `20-` puts Kiln ahead of every preset Arch ships (`90-systemd.preset`, and
/// the `disable *` default). Preset files are read in lexicographic order and
/// the first matching line wins, so the number is the whole mechanism.
pub const PRESET_PATH: &str = "usr/lib/systemd/system-preset/20-kiln.preset";

#[derive(Debug, Default)]
pub struct Applied {
    /// Units whose file Kiln wrote, in the order written.
    pub shipped: Vec<String>,
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    pub masked: Vec<String>,
    /// `systemd-analyze verify` output. Warnings, not errors: it complains
    /// about things that are legitimate in an image being assembled (units
    /// whose `Requires=` target lives in a package the verifier cannot see
    /// resolved), and failing the build on them would make it unusable.
    pub warnings: Vec<String>,
}

/// The host tools this step drives, behind a trait for the same reason
/// `Sysusers` is: they are pointed at the staging root, and a test should be
/// able to assert that rather than take it on faith.
pub trait Systemctl {
    fn preset_all(&self, root: &Path) -> Result<()>;
    /// `systemd-analyze verify --root`. Returns its complaints; an inability to
    /// run it at all is not fatal.
    fn verify(&self, root: &Path, units: &[String]) -> Result<Vec<String>>;
}

pub struct HostSystemctl;

impl Systemctl for HostSystemctl {
    fn preset_all(&self, root: &Path) -> Result<()> {
        let out = std::process::Command::new("systemctl")
            .arg("preset-all")
            .arg(format!("--root={}", root.display()))
            .output()
            .map_err(tree::io("running systemctl preset-all against", root))?;
        if !out.status.success() {
            return Err(tree::shape(format!(
                "systemctl preset-all --root={} failed: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    fn verify(&self, root: &Path, units: &[String]) -> Result<Vec<String>> {
        if units.is_empty() {
            return Ok(Vec::new());
        }
        let out = std::process::Command::new("systemd-analyze")
            .arg("verify")
            .arg(format!("--root={}", root.display()))
            .args(units)
            .output();
        let Ok(out) = out else {
            // The builder has no systemd-analyze. Say nothing rather than fail:
            // this is a lint over shipped units, not a contract.
            return Ok(Vec::new());
        };
        Ok(String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect())
    }
}

/// Realize unit files and unit state into the staging root.
///
/// `states` is the plan's answer, not the manifest's three lists: it leaves
/// `enable`/`disable`/`mask` independent, and resolution has already settled
/// which wins. `files` is what Kiln ships.
pub fn apply(
    root: &Path,
    config_root: &Path,
    states: &BTreeMap<String, EnableState>,
    files: &BTreeMap<String, UnitFile>,
    owners: &dyn Owners,
    systemctl: &dyn Systemctl,
) -> Result<Applied> {
    let mut applied = Applied::default();
    let mut refusals = Vec::new();

    for (name, unit) in files {
        let at = format!("/{UNIT_DIR}/{name}");
        if let Some(owner) = owners.owner_of(&at) {
            refusals.push(Refusal {
                target: name.clone(),
                why: format!("{at} is owned by the package `{owner}`"),
                hint: Some(format!(
                    "systemd's own override mechanism is a drop-in: ship \
                     `/etc/systemd/system/{name}.d/10-kiln.conf` with just the settings to \
                     change, and the rest of {owner}'s unit keeps working"
                )),
            });
            continue;
        }
        let body = match (&unit.content, &unit.source) {
            (Some(content), _) => content.clone(),
            (None, Some(source)) => {
                let from = config_root.join(source);
                std::fs::read_to_string(&from)
                    .map_err(tree::io("reading the unit source", &from))?
            }
            (None, None) => {
                refusals.push(Refusal {
                    target: name.clone(),
                    why: "neither `source` nor `content` is set".into(),
                    hint: None,
                });
                continue;
            }
        };
        tree::write(&root.join(UNIT_DIR).join(name), &body)?;
        tree::set_mode(&root.join(UNIT_DIR).join(name), 0o644)?;
        applied.shipped.push(name.clone());
    }

    // enabling a unit that no package provides is a hard error. The
    // check runs after shipping so Kiln's own units count as provided, and it
    // is the only check here that needs the assembled tree rather than the
    // configuration.
    let available = available_units(root);
    for (name, state) in states {
        if *state == EnableState::Unset || files.contains_key(name) {
            continue;
        }
        if provides(&available, root, name) {
            continue;
        }
        let verb = match state {
            EnableState::Masked => "masked",
            EnableState::Disabled => "disabled",
            _ => "enabled",
        };
        refusals.push(Refusal {
            target: name.clone(),
            why: format!("nothing in this image provides it, and it is being {verb}"),
            hint: kiln_diag::did_you_mean(name, available.iter().map(String::as_str)).or(Some(
                "add the package that ships it, or ship the unit with `[[systemd.unit]]`".into(),
            )),
        });
    }

    if !refusals.is_empty() {
        return Err(Error::Refused {
            noun: ("unit", "units"),
            problems: refusals,
        });
    }

    // Masks are written before `preset-all`, so that a masked unit cannot pick
    // up a `.wants` symlink from some *other* unit's preset on the way past.
    for (name, state) in states {
        match state {
            EnableState::Masked => {
                tree::symlink("/dev/null", &root.join(ADMIN_DIR).join(name))?;
                applied.masked.push(name.clone());
            }
            EnableState::Enabled => applied.enabled.push(name.clone()),
            EnableState::Disabled => applied.disabled.push(name.clone()),
            EnableState::Unset => {}
        }
    }

    tree::write(
        &root.join(PRESET_PATH),
        &render_preset(&applied.enabled, &applied.disabled),
    )?;
    systemctl.preset_all(root)?;
    applied.warnings = systemctl.verify(root, &applied.shipped)?;
    Ok(applied)
}

/// The body of `20-kiln.preset`.
///
/// Masked units get no line. A mask is a symlink to `/dev/null`, which is a
/// stronger statement than a preset and one that `systemctl unmask` can undo;
/// writing `disable` for it too would say the same thing twice, in a place the
/// administrator cannot reach.
pub fn render_preset(enabled: &[String], disabled: &[String]) -> String {
    let mut out = String::from(
        "# Generated by Kiln from [systemd] enable/disable.\n\
         # Read before every preset Arch ships, so these lines win.\n",
    );
    for name in enabled {
        out.push_str(&format!("enable {name}\n"));
    }
    for name in disabled {
        out.push_str(&format!("disable {name}\n"));
    }
    out
}

/// Unit names the assembled tree provides, by filename.
fn available_units(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for dir in [UNIT_DIR, ADMIN_DIR] {
        for path in tree::entries(&root.join(dir)).unwrap_or_default() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                out.insert(name.to_string());
            }
        }
    }
    out
}

/// Is `name` provided, allowing for the two ways a unit exists under a name
/// that is not a filename?
fn provides(available: &BTreeSet<String>, root: &Path, name: &str) -> bool {
    if available.contains(name) {
        return true;
    }
    // `foo@bar.service` is an instance of the template `foo@.service`.
    if let Some((stem, suffix)) = name.rsplit_once('.') {
        if let Some((base, instance)) = stem.split_once('@') {
            if !instance.is_empty() && available.contains(&format!("{base}@.{suffix}")) {
                return true;
            }
        }
    }
    // `Alias=` in some other unit's [Install] section. Read only when the
    // cheap answers have failed — this opens every unit file in the image.
    available.iter().any(|other| {
        let path = root.join(UNIT_DIR).join(other);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return false;
        };
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("Alias="))
            .any(|aliases| aliases.split_whitespace().any(|a| a == name))
    })
}
