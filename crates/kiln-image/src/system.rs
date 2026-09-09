//! `[system]`: hostname, timezone, keymap, locale.
//!
//! `system.timezone`, `system.keymap` and `system.locale.lang` always carry a
//! concrete value in the `Manifest` — `"UTC"`, `"us"` and `"C.UTF-8"` are
//! defaults, not an unset state — so `/etc/localtime`, `/etc/vconsole.conf`
//! and `/etc/locale.conf` are written for every image, whether or not the
//! configuration has a `[system]` table at all. `system.hostname` is the one
//! key with a real unset (`None` means systemd's own default applies), and
//! `system.locale.generate` is only ever locales beyond the ones glibc's
//! stock archive already carries — so those two write nothing when the
//! configuration leaves them empty, the same rule `kernel::install_module_config`
//! follows for `kernel.modules.load`/`blacklist`.
//!
//! Because the first three are unconditional, `kiln-config` refuses a
//! `[[file]]` entry at any of these five targets outright (`FORBIDDEN_TARGETS`
//! for the three always-written ones, a dedicated conflict check for the two
//! conditional ones) — so nothing here can silently clobber content the user
//! thought they were writing through `[[file]]`.
//!
//! Written before normalization moves `/etc` to `/usr/etc` (step 10), and
//! before step 9's kernel placement — `locale-gen` needs nothing from the
//! kernel step, and there is no reason to make it wait.

use crate::tree::{self, Result};
use kiln_manifest::SystemDefaults;
use kiln_sandbox::SandboxSpec;
use std::collections::BTreeSet;
use std::path::Path;

pub const HOSTNAME_PATH: &str = "etc/hostname";
pub const LOCALTIME_PATH: &str = "etc/localtime";
pub const VCONSOLE_PATH: &str = "etc/vconsole.conf";
pub const LOCALE_CONF_PATH: &str = "etc/locale.conf";
pub const LOCALE_GEN_PATH: &str = "etc/locale.gen";

/// Where `tzdata` puts zoneinfo data, staging-root-relative. Also the prefix
/// of the `/etc/localtime` symlink target, which is a path *in the image*,
/// not in the staging root.
pub const ZONEINFO_DIR: &str = "usr/share/zoneinfo";

pub fn locale_gen_conf(generate: &BTreeSet<String>) -> String {
    generate.iter().map(|l| format!("{l}\n")).collect()
}

/// Write everything `[system]` owns into the staging root.
pub fn install(root: &Path, system: &SystemDefaults) -> Result<()> {
    if let Some(hostname) = &system.hostname {
        write(root, HOSTNAME_PATH, &format!("{hostname}\n"))?;
    }

    // dracut logs and moves on for a missing driver (see `kernel.rs`); glibc's
    // `ln` equivalent here is worse, since a dangling `/etc/localtime` is not
    // even logged — the machine just silently reads as UTC. Checked against
    // the staging root's own `tzdata`, not the builder's, because that is the
    // zoneinfo the booted image will actually have.
    let zoneinfo = root.join(ZONEINFO_DIR).join(&system.timezone);
    if !zoneinfo.is_file() {
        return Err(tree::shape(format!(
            "`system.timezone = \"{tz}\"` has no zoneinfo data at /{ZONEINFO_DIR}/{tz} in the \
             image — check the spelling against the zone names `tzdata` ships",
            tz = system.timezone,
        )));
    }
    tree::symlink(
        &format!("/{ZONEINFO_DIR}/{}", system.timezone),
        &root.join(LOCALTIME_PATH),
    )?;

    write(root, VCONSOLE_PATH, &format!("KEYMAP={}\n", system.keymap))?;
    write(
        root,
        LOCALE_CONF_PATH,
        &format!("LANG={}\n", system.locale.lang),
    )?;

    if !system.locale.generate.is_empty() {
        write(
            root,
            LOCALE_GEN_PATH,
            &locale_gen_conf(&system.locale.generate),
        )?;
    }

    Ok(())
}

/// Whether `install` left `/etc/locale.gen` with anything in it. Running
/// `locale-gen` unconditionally would be harmless — an empty `locale.gen`
/// compiles nothing — but it is one more sandboxed process for every image,
/// and most images never ask for a generated locale at all.
pub fn needs_locale_gen(system: &SystemDefaults) -> bool {
    !system.locale.generate.is_empty()
}

/// Compiles `/etc/locale.gen` into `/usr/lib/locale/locale-archive`. Run
/// chrooted into the staging root, against the glibc the main transaction
/// already installed there — never the builder's.
pub fn locale_gen_spec(root: &Path) -> SandboxSpec {
    SandboxSpec::in_root(root, ["locale-gen".to_string()])
}

fn write(root: &Path, at: &str, body: &str) -> Result<()> {
    let dest = root.join(at);
    tree::write(&dest, body)?;
    tree::set_mode(&dest, 0o644)
}
