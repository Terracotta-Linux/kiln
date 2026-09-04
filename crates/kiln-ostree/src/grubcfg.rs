//! The GRUB configuration file, and the link that makes libostree's atomic
//! entry swap reach it.
//!
//! libostree's grub2 backend does not write `/boot/grub/grub.cfg`. It runs
//! `grub-mkconfig` chrooted into the deployment and writes the result to
//! `<sysroot>/boot/loader.N/grub.cfg`, alternating N between 0 and 1 so that
//! the entire set of boot entries is swapped by renaming a single symlink,
//! `/boot/loader`. GRUB reads `$prefix/grub.cfg` and knows nothing about any
//! of that.
//!
//! One symlink joins the two — `/boot/grub/grub.cfg` → `../loader/grub.cfg` —
//! and Fedora Silverblue, the arrangement Kiln copied, ships it. `grub-install`
//! on Arch does not: `grub-mkconfig -o /boot/grub/grub.cfg` writes a regular
//! file, and a regular file there is correct exactly once.
//!
//! **The failure it causes is total, delayed, and unrecoverable in place.** A
//! generated config names the bootversion that was current when it ran —
//! `ostree=/ostree/boot.1/kiln/<bootcsum>/0` — and the next deploy flips the
//! bootversion, renaming that directory to `boot.0`. GRUB then hands the kernel
//! a path that no longer exists, `ostree-prepare-root` cannot find the
//! deployment, and the machine stops in the initramfs emergency shell. Kiln's
//! automatic rollback cannot save it either: a config generated when the
//! machine had one deployment holds one menuentry, so the `default="1"` the
//! counter selects when it runs out is whatever GRUB emitted next — on a UEFI
//! machine, the firmware setup entry.
//!
//! Which is why this is repaired rather than reported. Every other degradation
//! in that mechanism costs a safety net; this one costs the boot.

use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// `<sysroot>/boot/grub/grub.cfg`.
///
/// One path, not a search, for the same reason as [`crate::grubenv::path`]:
/// Fedora's `/boot/grub2` is a Red Hat rename that an Arch-derived image does
/// not have.
pub fn path(sysroot: &Path) -> PathBuf {
    sysroot.join("boot/grub/grub.cfg")
}

/// The file libostree regenerates, reached through the symlink it swaps.
fn generated(sysroot: &Path) -> PathBuf {
    sysroot.join("boot/loader/grub.cfg")
}

/// Relative on purpose. GRUB resolves the link with `/boot` as its own
/// filesystem root, where an absolute `/boot/loader/grub.cfg` names
/// nothing — it would only resolve by way of the `boot -> .` symlink libostree
/// leaves there, which is a compatibility shim to lean on, not a contract.
const TARGET: &str = "../loader/grub.cfg";

/// What [`link`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Already a symlink: libostree's swap reaches GRUB.
    Already,
    /// It was a regular file, frozen at whichever bootversion generated it.
    /// Replaced.
    Repaired,
    /// This sysroot has no `grub.cfg` at all. Nothing has run `grub-install`
    /// against it yet, and doing so is the installer's job.
    Absent,
}

/// Make `/boot/grub/grub.cfg` the symlink libostree's grub2 backend needs.
///
/// Called *before* the deployment is written rather than after, because a
/// staged deployment is finalized at shutdown by
/// `ostree-finalize-staged.service`: Kiln's process is long gone by the time
/// `loader.N/grub.cfg` is generated, so there is no "after" to repair in.
///
/// That ordering is why the current configuration is seeded into the loader
/// directory first. Between this call and the finalize, `../loader/grub.cfg`
/// has to resolve to something bootable, and on a machine whose deploys have
/// all gone through `--sysroot` there is no `loader.N/grub.cfg` yet (the
/// grub2 backend cannot run against a sysroot that is not `/`, so an
/// installer's deploys write BLS entries and no `grub.cfg`). A dangling symlink
/// there would leave GRUB with no configuration at all if the machine lost
/// power before it shut down. The bytes copied are the ones the machine booted
/// with, and the finalize overwrites them.
///
/// Failure is an error rather than a warning, and deliberately so. Unlike an
/// unarmed boot counter, what is missing here is not a safety net but the
/// machine's ability to boot what is about to be staged; refusing before
/// anything is written is the outcome a user can recover from.
pub fn link(sysroot: &Path) -> Result<Link> {
    let cfg = path(sysroot);
    let meta = match std::fs::symlink_metadata(&cfg) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Link::Absent),
        Err(source) => {
            return Err(Error::Io {
                doing: "reading the GRUB configuration at",
                path: cfg,
                source,
            })
        }
    };
    // Any symlink, without inspecting its target: a link here is the
    // arrangement libostree expects, and whoever made one pointed it at the
    // loader. Only a regular file is the broken case.
    if meta.file_type().is_symlink() {
        return Ok(Link::Already);
    }

    let generated = generated(sysroot);
    if !generated.exists() {
        std::fs::copy(&cfg, &generated).map_err(|source| Error::Io {
            doing: "seeding the loader's GRUB configuration at",
            path: generated,
            source,
        })?;
    }

    // Through a temporary name and a rename, so that GRUB finds either the old
    // regular file or the new symlink and never neither.
    let staging = cfg.with_file_name("grub.cfg.kiln-new");
    let _ = std::fs::remove_file(&staging);
    std::os::unix::fs::symlink(TARGET, &staging).map_err(|source| Error::Io {
        doing: "creating the GRUB configuration symlink at",
        path: staging.clone(),
        source,
    })?;
    std::fs::rename(&staging, &cfg).map_err(|source| Error::Io {
        doing: "replacing the GRUB configuration at",
        path: cfg,
        source,
    })?;
    Ok(Link::Repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A sysroot's `/boot` as an installed machine has it: GRUB's own prefix
    /// directory, and libostree's `loader` symlink onto the current
    /// bootversion. `generated` is what libostree has written there, if
    /// anything has yet.
    fn boot(name: &str, cfg: Option<&str>, generated: Option<&str>) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-roots")
            .join(format!("grubcfg-{name}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("boot/grub")).unwrap();
        fs::create_dir_all(root.join("boot/loader.0")).unwrap();
        std::os::unix::fs::symlink("loader.0", root.join("boot/loader")).unwrap();
        if let Some(text) = cfg {
            fs::write(root.join("boot/grub/grub.cfg"), text).unwrap();
        }
        if let Some(text) = generated {
            fs::write(root.join("boot/loader.0/grub.cfg"), text).unwrap();
        }
        root
    }

    /// The repair itself, on the machine it was written for: `grub-install`
    /// left a regular file, libostree has since regenerated a config the
    /// bootloader never read, and after this GRUB reads the regenerated one.
    #[test]
    fn a_regular_config_becomes_the_generated_one() {
        let root = boot(
            "regular",
            Some("stale, ostree=/ostree/boot.1/…"),
            Some("fresh"),
        );
        assert_eq!(link(&root).unwrap(), Link::Repaired);

        let cfg = path(&root);
        assert_eq!(fs::read_link(&cfg).unwrap(), Path::new(TARGET));
        // Through the link, and through libostree's `loader` symlink under it.
        assert_eq!(fs::read_to_string(&cfg).unwrap(), "fresh");
    }

    /// The ordering trap. A machine installed through `--sysroot` has a
    /// `grub.cfg` that `grub-install` generated and none that libostree did, so
    /// linking without seeding would leave GRUB with no configuration at all
    /// until the staged deployment is finalized at the next shutdown — and a
    /// machine that lost power in between would have nothing to boot.
    #[test]
    fn the_config_in_hand_is_seeded_before_the_link_replaces_it() {
        let root = boot("seed", Some("the config this machine booted"), None);
        assert_eq!(link(&root).unwrap(), Link::Repaired);

        assert_eq!(
            fs::read_to_string(path(&root)).unwrap(),
            "the config this machine booted"
        );
        assert!(root.join("boot/loader.0/grub.cfg").exists());
    }

    /// Idempotent, and without inspecting the target: a link there is the
    /// arrangement libostree expects, and rewriting an absolute one somebody
    /// else made would be a repair reported on every deploy forever.
    #[test]
    fn an_existing_link_is_left_alone() {
        let root = boot("linked", None, Some("fresh"));
        std::os::unix::fs::symlink("/boot/loader/grub.cfg", path(&root)).unwrap();

        assert_eq!(link(&root).unwrap(), Link::Already);
        assert_eq!(
            fs::read_link(path(&root)).unwrap(),
            Path::new("/boot/loader/grub.cfg")
        );
    }

    /// Nothing has run `grub-install` against this sysroot. That is the state
    /// an installer hands over mid-way and it is not an error — there
    /// is no file to repair and creating one is not Kiln's job.
    #[test]
    fn no_configuration_at_all_is_not_an_error() {
        let root = boot("absent", None, None);
        assert_eq!(link(&root).unwrap(), Link::Absent);
        assert!(!path(&root).exists());
    }
}
