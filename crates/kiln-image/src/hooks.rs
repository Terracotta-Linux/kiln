//! Neutralizing package-shipped alpm hooks.
//!
//! Package hooks in `/usr/share/libalpm/hooks` **always run**: libalpm scans
//! that directory unconditionally and `--hookdir` does not suppress it. The
//! only lever is *same-filename shadowing* — a hook file of the same name in a
//! later `HookDir` overrides the package's. Twenty-six hooks fire during a base
//! image build.
//!
//! The policy: **shadow hooks that write runtime state, or that Kiln owns; keep
//! the rest.** Everything not listed here writes to `/usr` or `/etc` and is
//! legitimate image content — locale-gen, ldconfig, iconvconfig, sysusers,
//! ca-trust, hwdb, the journal catalog, binfmt and the glib schema hooks.

use crate::tree::{self, Result};
use std::path::{Path, PathBuf};

/// A hook Kiln overrides, and why. The reason is written into the shadow file,
/// because the next person to find one of these in a build tree will want to
/// know what it is before deleting it.
pub struct Shadowed {
    pub filename: &'static str,
    pub reason: &'static str,
}

pub const SHADOWED: &[Shadowed] = &[
    Shadowed {
        filename: "21-systemd-tmpfiles.hook",
        reason: "it runs `systemd-tmpfiles --create` inside the chroot, materializing \
                 /root/.ssh and similar *machine* state into the image. tmpfiles runs at \
                 boot; that is the whole point of the /var drain.",
    },
    Shadowed {
        filename: "90-dracut-install.hook",
        reason: "Kiln generates the initramfs itself, once, at a fixed point. \
                 Letting dracut fire per-package wastes minutes and writes /boot, which \
                 normalization then deletes.",
    },
    Shadowed {
        filename: "60-dracut-remove.hook",
        reason: "the other half of 90-dracut-install.",
    },
    Shadowed {
        filename: "60-depmod.hook",
        reason: "Kiln runs depmod itself, deterministically, after the transaction \
.",
    },
];

/// Write the shadow hooks into `dir` and return it, for registering as a
/// `HookDir` *after* the default one — later wins by filename.
///
/// The directory lives beside the build rather than inside the staging root: a
/// shadow file left in the image would ship a hook that does nothing to a
/// system that never runs alpm hooks again.
pub fn materialize(dir: &Path) -> Result<PathBuf> {
    tree::mkdir(dir)?;
    for hook in SHADOWED {
        let body = format!(
            "# Shadowed by Kiln. \n\
             #\n\
             # A package-shipped alpm hook cannot be disabled — libalpm scans\n\
             # /usr/share/libalpm/hooks unconditionally — so this file overrides it by\n\
             # having the same name in a later HookDir. It declares no trigger and no\n\
             # action, so it never fires.\n\
             #\n\
             # {}\n",
            wrap(hook.reason, 74, "# ")
        );
        tree::write(&dir.join(hook.filename), &body)?;
    }
    Ok(dir.to_path_buf())
}

/// True when `name` is one Kiln shadows — for reporting which of the hooks that
/// ran were the package's own and which were Kiln's no-ops.
pub fn is_shadowed(name: &str) -> bool {
    SHADOWED.iter().any(|h| h.filename == name)
}

/// Wrap at `width`, continuing with `prefix`. A comment explaining a decision
/// is only useful if it is readable in an editor.
fn wrap(text: &str, width: usize, prefix: &str) -> String {
    let mut out = String::new();
    let mut column = 0;
    for word in text.split_whitespace() {
        if column > 0 && column + 1 + word.len() > width {
            out.push('\n');
            out.push_str(prefix);
            column = 0;
        } else if column > 0 {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += word.len();
    }
    out
}
