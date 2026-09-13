//! `[boot]`: the one field of it that has anything left to write.
//!
//! `boot.loader` and `boot.initramfs` are each one value today (`BootLoader`
//! and `Initramfs` are single-variant enums) — validated so a typo gets a
//! diagnostic, but with nothing else for assembly to *do*, since there is
//! only ever the one thing to place. `boot.timeout` is different: it is a
//! real number that has to reach GRUB, and nothing upstream of this module
//! ever writes it anywhere. `grub-mkconfig` reads it from `GRUB_TIMEOUT=` in
//! `/etc/default/grub`, a file the `grub` package ships pre-populated with
//! its own defaults (`GRUB_TIMEOUT=5` among them) — so without this, every
//! image boots with the package's stock timeout regardless of what
//! `boot.timeout` says, `config_id` moves and a rebuild happens, and nothing
//! about the running system is actually different.
//!
//! Patched in place rather than written wholesale, unlike `system.rs`'s
//! locale/timezone files: `/etc/default/grub` also carries `GRUB_DEFAULT`,
//! `GRUB_DISTRIBUTOR`, `GRUB_CMDLINE_LINUX`, and whatever else the package or
//! a `[[file]]` override put there, none of which `boot.timeout` has any
//! business touching. Absent entirely — no `grub` package installed — is not
//! an error: nothing will ever run `grub-mkconfig` against this image either,
//! so there is nothing to patch and nothing reads the value anyway.
//!
//! Run after step 6 (overlay), so a `[[file]]` override of
//! `/etc/default/grub` is the text this patches rather than something
//! overwritten by it — the same reason `bootcount::install` sits there.

use crate::tree::{self, Result};
use std::path::Path;

pub const GRUB_DEFAULTS_PATH: &str = "etc/default/grub";

/// Set `GRUB_TIMEOUT` to `boot.timeout`, leaving every other line alone.
pub fn install(root: &Path, boot: &kiln_manifest::Boot) -> Result<()> {
    let path = root.join(GRUB_DEFAULTS_PATH);
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(tree::io("reading", &path)(source)),
    };

    let line = format!("GRUB_TIMEOUT={}", boot.timeout);
    let mut found = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("GRUB_TIMEOUT=") {
                found = true;
                line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(line);
    }
    lines.push(String::new()); // trailing newline

    tree::write(&path, &lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_manifest::{Boot, BootLoader, Initramfs};

    fn boot(timeout: i64) -> Boot {
        Boot {
            loader: BootLoader::Grub2,
            timeout,
            initramfs: Initramfs::Dracut,
        }
    }

    fn root(name: &str) -> std::path::PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-roots")
            .join(format!("boot-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("etc/default")).unwrap();
        root
    }

    /// The stock file a fresh `grub` package extracts, with the timeout the
    /// package picked rather than the one the configuration asked for.
    #[test]
    fn the_stock_timeout_is_replaced_and_nothing_else_moves() {
        let root = root("stock");
        std::fs::write(
            root.join(GRUB_DEFAULTS_PATH),
            "GRUB_DEFAULT=0\nGRUB_TIMEOUT=5\nGRUB_DISTRIBUTOR=\"Arch\"\n",
        )
        .unwrap();

        install(&root, &boot(0)).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join(GRUB_DEFAULTS_PATH)).unwrap(),
            "GRUB_DEFAULT=0\nGRUB_TIMEOUT=0\nGRUB_DISTRIBUTOR=\"Arch\"\n"
        );
    }

    /// A file with no `GRUB_TIMEOUT=` line at all — a `[[file]]` override
    /// that left it out — gains one rather than being left to boot at
    /// whatever GRUB itself defaults to.
    #[test]
    fn a_missing_timeout_line_is_appended() {
        let root = root("missing");
        std::fs::write(root.join(GRUB_DEFAULTS_PATH), "GRUB_DEFAULT=0\n").unwrap();

        install(&root, &boot(10)).unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join(GRUB_DEFAULTS_PATH)).unwrap(),
            "GRUB_DEFAULT=0\nGRUB_TIMEOUT=10\n"
        );
    }

    /// No `grub` package installed, so no `/etc/default/grub` to patch and
    /// nothing that will ever read one. Not an error.
    #[test]
    fn a_missing_file_is_not_an_error() {
        let root = root("absent");
        install(&root, &boot(0)).unwrap();
        assert!(!root.join(GRUB_DEFAULTS_PATH).exists());
    }
}
