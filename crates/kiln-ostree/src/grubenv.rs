//! The GRUB environment block.
//!
//! Automatic rollback needs one bit of state that survives a kernel that never
//! reaches userspace, and GRUB has exactly one: `grubenv`, a fixed-size file at
//! `$prefix/grubenv` that the bootloader can both read *and* rewrite in place.
//! Kiln arms a counter there when it stages a new generation; the generated
//! `/etc/grub.d/09_kiln_boot_counter` snippet decrements it on each attempt and
//! selects the rollback entry when it runs out; `kiln-boot-success.service`
//! clears it once `boot-complete.target` is reached.
//!
//! **Why a file and not BLS boot counting.** The Boot Loader Specification's
//! `ostree-42+3.conf` naming — which libostree will happily write, and which
//! originally specified — is only ever *decremented by the bootloader*.
//! systemd-boot implements it; Arch's GRUB 2.14 does not (its `blsuki` module
//! parses BLS entries and contains no counting logic, and Fedora's counting
//! support is a downstream patch). Under the GRUB2 backend settled on,
//! a `+3` in a filename is a claim nothing honours, so Kiln does not write one.
//!
//! The format is GRUB's, and it is rigid on purpose: exactly `SIZE` bytes, a
//! header line, `key=value` lines, then `#` padding. GRUB rewrites it from the
//! bootloader with no filesystem allocator, so the length must never change.
//! Kiln writes it with ordinary file I/O — `grub-editenv` is not assumed on the
//! build host, only inside the image, where the `grub` package puts it.

use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What `grub-editenv create` makes, and the only size GRUB will rewrite in
/// place. A block of a different length is one the bootloader can read and
/// never update, which is a counter that never decrements.
pub const SIZE: usize = 1024;

const HEADER: &str = "# GRUB Environment Block\n";

/// The variable holding the attempts remaining, including the one about to
/// happen. Absent means "not being counted": a generation that has already
/// booted successfully, or one Kiln did not stage.
pub const COUNTER: &str = "boot_counter";

/// Set to `1` by `kiln-boot-success.service`. The snippet checks it as well as
/// the counter so that a machine which reaches `boot-complete.target` and then
/// reboots before the service manages to clear the counter is not counted
/// against the generation.
pub const SUCCESS: &str = "boot_success";

/// The script inside a deployment that clears the counter, relative to the
/// deployment root. `kiln_image::bootcount::SCRIPT`, named again here because
/// this crate does not depend on that one and the two must not drift — there is
/// a test in `kiln-cli` that they are the same string.
pub const BLESS: &str = "usr/lib/kiln/boot-success";

/// `<sysroot>/boot/grub/grubenv`.
///
/// One path, not a search. libostree hands GRUB an ext4 `/boot` that it owns
/// and `grub-install` puts its prefix at `/boot/grub` there; Fedora's
/// `/boot/grub2` is a Red Hat rename that an Arch-derived image does not have.
pub fn path(sysroot: &Path) -> PathBuf {
    sysroot.join("boot/grub/grubenv")
}

/// Parse a block. Unknown variables are preserved — `grubenv` is shared with
/// GRUB's own `saved_entry`/`next_entry` bookkeeping, and Kiln rewriting the
/// file must not be how a machine loses its default entry.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .collect()
}

/// Render a block: header, sorted `key=value` lines, `#` padding to `SIZE`.
///
/// Sorted because two runs that set the same variables must produce the same
/// bytes — this file is compared in tests and read by eye during a bad boot,
/// and a map's iteration order is not a thing to spend either on.
pub fn render(vars: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    let mut out = String::from(HEADER);
    for (key, value) in vars {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    if out.len() > SIZE {
        return Err(Error::Ostree {
            doing: "writing the GRUB environment block",
            message: format!(
                "the variables do not fit in GRUB's {SIZE}-byte block ({} bytes). GRUB rewrites \
                 this file in place from the bootloader, so it cannot grow",
                out.len()
            ),
        });
    }
    let mut bytes = out.into_bytes();
    bytes.resize(SIZE, b'#');
    Ok(bytes)
}

/// Read the block, or an empty map if there is none. A sysroot with no GRUB
/// installed on it is the normal case for `--sysroot` in a test, and it is not
/// an error to ask what a file that does not exist says.
pub fn read(sysroot: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path(sysroot))
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// Apply `changes` to the block, creating it if it is not there. `None` removes
/// a variable.
///
/// Read-modify-write rather than truncate-and-write, for the reason `parse`
/// gives: GRUB's own variables live in this file too.
pub fn update(sysroot: &Path, changes: &[(&str, Option<&str>)]) -> Result<()> {
    let at = path(sysroot);
    let mut vars = read(sysroot);
    for (key, value) in changes {
        match value {
            Some(v) => vars.insert((*key).to_string(), (*v).to_string()),
            None => vars.remove(*key),
        };
    }
    let bytes = render(&vars)?;
    if let Some(parent) = at.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            doing: "creating the GRUB directory at",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&at, bytes).map_err(|source| Error::Io {
        doing: "writing the GRUB environment block at",
        path: at,
        source,
    })
}

/// Give the deployment that is about to boot `tries` attempts.
///
/// `boot_success` is cleared in the same write. Leaving a stale `1` there from
/// the previous generation's successful boot would tell the snippet the machine
/// is already known good, and the counter would never be looked at.
pub fn arm(sysroot: &Path, tries: u32) -> Result<()> {
    let tries = tries.to_string();
    update(sysroot, &[(COUNTER, Some(&tries)), (SUCCESS, Some("0"))])
}

/// Stop counting: the user chose this generation deliberately (`kiln deploy`,
/// `kiln rollback`), so it gets no probation and any demotion `kiln status` was
/// reporting is resolved.
pub fn disarm(sysroot: &Path) -> Result<()> {
    update(sysroot, &[(COUNTER, None), (SUCCESS, Some("1"))])
}

/// What the counter says, for `kiln status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Counting {
    /// No counter armed: nothing was staged, or the last boot succeeded and
    /// `kiln-boot-success.service` cleared it.
    Off,
    /// A generation is on probation with this many attempts left.
    Armed { left: u32, tries: u32 },
    /// The counter ran out. The bootloader is selecting the rollback entry.
    Exhausted { tries: u32 },
}

/// Read the counting state.
///
/// `tries` is passed in rather than stored. The block holds only what is left,
/// and the total belongs to the generated `/etc/grub.d` ladder — whose length
/// *is* the number of tries — so `kiln_image::bootcount::TRIES` is the one
/// place it is decided.
pub fn counting(sysroot: &Path, tries: u32) -> Counting {
    let vars = read(sysroot);
    if vars.get(SUCCESS).map(String::as_str) == Some("1") {
        return Counting::Off;
    }
    match vars.get(COUNTER).and_then(|v| v.parse::<u32>().ok()) {
        None => Counting::Off,
        Some(0) => Counting::Exhausted { tries },
        Some(left) => Counting::Armed { left, tries },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rendered_block_is_exactly_grubs_size() {
        let mut vars = BTreeMap::new();
        vars.insert(COUNTER.to_string(), "3".to_string());
        let bytes = render(&vars).unwrap();
        assert_eq!(bytes.len(), SIZE);
        assert!(bytes.starts_with(HEADER.as_bytes()));
        assert!(bytes.ends_with(b"#"));
    }

    /// The round trip has to be exact, because Kiln is not the only writer:
    /// GRUB's `savedefault` puts `saved_entry` in the same file, and a rewrite
    /// that dropped it would lose the machine's default entry.
    #[test]
    fn variables_kiln_does_not_know_survive_a_rewrite() {
        let mut vars = parse("# GRUB Environment Block\nsaved_entry=2\nboot_counter=3\n");
        vars.insert(SUCCESS.to_string(), "0".to_string());
        let rendered = render(&vars).unwrap();
        let back = parse(&String::from_utf8(rendered).unwrap());
        assert_eq!(back.get("saved_entry").map(String::as_str), Some("2"));
        assert_eq!(back.get(COUNTER).map(String::as_str), Some("3"));
    }

    /// The padding is `#`, which is a comment in GRUB's parser — so a block
    /// read back must not gain a variable from its own padding.
    #[test]
    fn padding_does_not_parse_as_a_variable() {
        let mut vars = BTreeMap::new();
        vars.insert(COUNTER.to_string(), "3".to_string());
        let text = String::from_utf8(render(&vars).unwrap()).unwrap();
        assert_eq!(parse(&text).len(), 1);
    }

    #[test]
    fn a_block_that_would_not_fit_is_refused_rather_than_truncated() {
        let mut vars = BTreeMap::new();
        vars.insert("huge".to_string(), "x".repeat(SIZE));
        assert!(render(&vars).is_err());
    }
}
