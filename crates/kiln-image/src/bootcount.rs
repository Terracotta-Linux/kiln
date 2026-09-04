//! Automatic rollback on boot failure, image side.
//!
//! A user who edits `system.toml`, runs `kiln apply`, and reboots into a system
//! that does not come up must not need a rescue USB. That converts "you can
//! roll back" into "it rolled back for you", which is the difference between an
//! immutable system that is safe for a normal user and one that is safe for
//! someone who owns a second computer.
//!
//! Three pieces, all of them image content, all written by assembly rather than
//! asked for in TOML — a boot counter is Kiln's own machinery, not a decision
//! anybody makes per image:
//!
//! ```text
//! /etc/grub.d/09_kiln_boot_counter   decrement, and pick the rollback entry at zero
//! /usr/lib/kiln/boot-success         clear the counter once the boot is good
//! /usr/lib/systemd/system/kiln-boot-success.service   run it after boot-complete.target
//! ```
//!
//! **The counter lives in `grubenv`, not in BLS entry filenames.** An earlier
//! design specified the Boot Loader Specification's `ostree-42+3.conf` counting,
//! which libostree can write through its `boot-counting-tries` repo option.
//! Nothing on this path decrements it: BLS counting is the *bootloader's* job,
//! and the GRUB2 backend Kiln settled on does not implement it (Arch's GRUB 2.14 ships
//! `blsuki.mod`, which parses BLS entries and contains no counting logic;
//! Fedora's counting support is a downstream patch, and systemd-boot — which
//! does implement it — has no libostree backend at all). GRUB's own mechanism
//! is `grubenv`, and that is the one Kiln uses.
//!
//! **Why the snippet is a ladder rather than arithmetic.** GRUB's script
//! language has no arithmetic, and Arch's GRUB has no `increment`/`decrement`
//! module — that is another Fedora patch. So the decrement is written out as
//! one comparison per remaining attempt. `tries` is small and Kiln generates
//! the file, so the ladder costs a few lines and buys a mechanism that works on
//! a stock Arch bootloader.

use crate::tree::{self, Result};
use std::path::Path;

/// How many attempts a newly staged generation gets before it is demoted.
/// The chosen default.
///
/// Decided here because the generated ladder's *length* is the number of tries:
/// a snippet built for 3 and a counter armed at 5 would spend two attempts in a
/// branch that does not exist and fall straight through to the rollback entry.
/// `kiln-ostree` takes it as an argument rather than keeping a second copy.
pub const TRIES: u32 = 3;

/// Where Kiln's own boot-success script goes, beside the manifest and the
/// record it already writes there (step 11).
pub const SCRIPT: &str = "usr/lib/kiln/boot-success";

/// `09` puts this after `00_header` — which is what emits the `load_env` that
/// makes `${boot_counter}` readable at all — and before `10_linux` and
/// libostree's own `15_ostree`, so `default` is already set to something when
/// this overrides it.
pub const SNIPPET: &str = "etc/grub.d/09_kiln_boot_counter";

pub const UNIT: &str = "kiln-boot-success.service";

/// Write all three. Unconditional: `boot.loader` takes one value, so
/// there is no image this does not apply to.
///
/// Every piece is inert where GRUB is absent — the snippet is a file nothing
/// reads, and the unit's `ConditionPathExists` on `grub-editenv` keeps it from
/// failing in an image that does not ship `grub`. An image that cannot count is
/// one that boots the way it did before this existed, rather than one with a
/// failed unit in `systemctl --failed`.
pub fn install(root: &Path, tries: u32) -> Result<()> {
    tree::write(&root.join(SNIPPET), &snippet(tries))?;
    tree::set_mode(&root.join(SNIPPET), 0o755)?;
    tree::write(&root.join(SCRIPT), BOOT_SUCCESS)?;
    tree::set_mode(&root.join(SCRIPT), 0o755)?;
    Ok(())
}

/// The unit Kiln ships, handed to `units::apply` like any `[[systemd.unit]]` so
/// that it is shipped, preset and verified by the same code as everything else.
pub fn unit() -> kiln_manifest::UnitFile {
    kiln_manifest::UnitFile {
        name: UNIT.to_string(),
        source: None,
        content: Some(SERVICE.to_string()),
        enable: true,
    }
}

/// `/etc/grub.d/09_kiln_boot_counter`.
///
/// A `grub-mkconfig` fragment generator: it runs as a shell script and prints
/// GRUB script to stdout. The heredoc is quoted, so `${boot_counter}` reaches
/// GRUB rather than being expanded by the shell that generates the file.
pub fn snippet(tries: u32) -> String {
    // With no attempts left the rollback deployment is entry 1: libostree
    // writes menu entries in deployment order, and the deployment list is in
    // boot order, so index 1 is the same deployment `kiln rollback` would pick.
    let mut ladder = String::new();
    for left in (1..=tries).rev() {
        let keyword = if left == tries { "if" } else { "elif" };
        ladder.push_str(&format!(
            "  {keyword} [ \"${{boot_counter}}\" = \"{left}\" ]; then\n"
        ));
        ladder.push_str(&format!("    set boot_counter=\"{}\"\n", left - 1));
    }
    if tries == 0 {
        ladder.push_str("  set default=\"1\"\n");
    } else {
        ladder.push_str("  else\n    set default=\"1\"\n  fi\n");
    }

    format!("{PREAMBLE}{ladder}{EPILOGUE}")
}

/// Everything above the generated ladder. Split out so the ladder is the only
/// part of this file that is computed, and the rest can be read as the shell it
/// is rather than as an escaped format string.
const PREAMBLE: &str = r#"#!/bin/sh
# Generated by Kiln — automatic rollback on boot failure.
#
# `kiln apply` arms `boot_counter` in grubenv when it stages a generation. Each
# boot attempt spends one; when they run out GRUB selects entry 1, the rollback
# deployment. `kiln-boot-success.service` clears the counter once
# boot-complete.target is reached, so a generation that works is never counted
# against.
#
# A ladder rather than arithmetic because GRUB script has neither, and Arch's
# GRUB has no `decrement` module (both are Fedora patches).
set -e

cat <<'EOF'
# Kiln boot counting
if [ -n "${boot_counter}" -a "${boot_success}" != "1" ]; then
"#;

const EPILOGUE: &str = r#"  save_env boot_counter
fi
EOF
"#;

/// `/usr/lib/kiln/boot-success`.
///
/// A script rather than two `ExecStart=` lines because of the remount: on an
/// OSTree system `/boot` is very often mounted read-only, and the unit has to
/// take it read-write, write, and put it back — which is three commands with a
/// conditional in the middle, and that is a script.
const BOOT_SUCCESS: &str = r#"#!/bin/sh
# Generated by Kiln.
#
# Marks the running generation good, so GRUB stops counting its attempts.
# Runs from kiln-boot-success.service, after boot-complete.target.
set -eu

env=/boot/grub/grubenv
[ -e "$env" ] || exit 0

# The unit runs with MountFlags=slave, so a read-write /boot here is private to
# this service and the rest of the system keeps the read-only one it booted
# with.
remounted=no
if mountpoint -q /boot; then
    if mount -o remount,rw /boot 2>/dev/null; then
        remounted=yes
    fi
fi

grub-editenv "$env" set boot_success=1
grub-editenv "$env" unset boot_counter

if [ "$remounted" = yes ]; then
    mount -o remount,ro /boot 2>/dev/null || true
fi
"#;

/// `kiln-boot-success.service`.
///
/// The shape is `systemd-bless-boot.service`'s, because it is the same job:
/// `Requires=` pulls `boot-complete.target` into the transaction, `After=`
/// waits for it, and any health check a distribution wants to gate blessing on
/// orders itself `Before=boot-complete.target`. It is not
/// `systemd-bless-boot.service` itself, which is conditioned on the
/// `LoaderBootCountPath` EFI variable that only systemd-boot sets.
const SERVICE: &str = "\
[Unit]
Description=Mark this Kiln generation as having booted successfully
DefaultDependencies=no
Requires=boot-complete.target
After=local-fs.target boot-complete.target
RequiresMountsFor=/boot
Conflicts=shutdown.target
Before=shutdown.target
ConditionPathExists=/usr/bin/grub-editenv
ConditionPathExists=/boot/grub/grubenv

[Service]
Type=oneshot
RemainAfterExit=yes
# Private mount namespace: the script takes /boot read-write to update grubenv,
# and the rest of the system keeps the read-only /boot it booted with.
MountFlags=slave
ExecStart=/usr/lib/kiln/boot-success

[Install]
WantedBy=multi-user.target
";

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder has to spend exactly one attempt per boot and select the
    /// rollback entry on the attempt after the last one. Three tries means
    /// three boots of the new generation, then entry 1.
    #[test]
    fn the_ladder_spends_one_attempt_per_step_and_then_rolls_back() {
        let text = snippet(3);
        for (from, to) in [("3", "2"), ("2", "1"), ("1", "0")] {
            assert!(
                text.contains(&format!("[ \"${{boot_counter}}\" = \"{from}\" ]")),
                "no branch for {from}:\n{text}"
            );
            assert!(
                text.contains(&format!("set boot_counter=\"{to}\"")),
                "{text}"
            );
        }
        assert!(text.contains("set default=\"1\""), "{text}");
    }

    /// The heredoc is quoted so GRUB's variables survive the shell that
    /// generates the fragment. An unquoted `EOF` here would expand
    /// `${boot_counter}` to nothing at grub-mkconfig time, and the emitted
    /// config would test the empty string forever.
    #[test]
    fn grub_variables_are_not_expanded_by_the_generating_shell() {
        let text = snippet(3);
        assert!(text.contains("<<'EOF'"), "{text}");
        assert!(text.contains("${boot_counter}"), "{text}");
        assert!(text.contains("${boot_success}"), "{text}");
    }

    /// a machine that reached boot-complete.target and rebooted before
    /// the service cleared the counter must not be counted against. That is the
    /// `boot_success` half of the condition, and losing it would demote a
    /// working generation after three fast reboots.
    #[test]
    fn a_blessed_boot_short_circuits_the_counter() {
        assert!(snippet(3).contains("\"${boot_success}\" != \"1\""));
    }

    /// The three files, where the rest of the system looks for them. Cheap, and
    /// the failure it catches is silent: a snippet at the wrong path is a file
    /// grub-mkconfig never reads, and a script that is not executable is a unit
    /// that fails on a machine that otherwise booted fine.
    #[test]
    fn install_places_all_three_pieces_executably() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("kiln-bootcount-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        install(&root, TRIES).unwrap();

        for path in [SNIPPET, SCRIPT] {
            let at = root.join(path);
            let mode = std::fs::metadata(&at).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "{path} is not executable");
        }
        // written to /etc in the staging root, because normalization
        // moves the whole directory to /usr/etc afterwards. Writing it straight
        // to /usr/etc would put it where the *live* /etc is never merged from.
        assert!(root.join("etc/grub.d/09_kiln_boot_counter").exists());
        assert!(unit().content.unwrap().contains("boot-complete.target"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// The unit has to be inert in an image with no `grub`, rather than a
    /// failed unit on every boot of one. This allows an empty configuration to
    /// produce an empty image, and Kiln writing this unconditionally must not
    /// make such an image report a failure.
    #[test]
    fn the_unit_does_nothing_where_grub_is_absent() {
        let unit = unit().content.unwrap();
        assert!(
            unit.contains("ConditionPathExists=/usr/bin/grub-editenv"),
            "{unit}"
        );
    }

    #[test]
    fn the_snippet_sorts_before_the_ostree_generator() {
        let name = Path::new(SNIPPET).file_name().unwrap().to_str().unwrap();
        assert!(name < "15_ostree", "{name} would run after libostree's own");
        assert!(name > "00_header", "{name} would run before load_env");
    }
}
