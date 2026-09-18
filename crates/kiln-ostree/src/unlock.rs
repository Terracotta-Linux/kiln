//! `ostree admin unlock`, development only.
//!
//! Kiln exposes exactly one unlocked state: libostree's **development**
//! state — the plain `ostree admin unlock` with no flags. It bind-mounts the
//! booted deployment's `/usr` writable for the rest of this boot, and nothing
//! about it survives — not `--hotfix`, which commits a separate overlay
//! libostree would otherwise keep around across reboots. That exists for a
//! workflow Kiln does not have: there is no live-shipping path here to
//! protect, and a state that outlives a reboot is exactly what a command
//! whose whole premise is "for testing, discarded on reboot" must not leave
//! behind by accident — including a reboot back into the very generation
//! that was unlocked.
//!
//! Kiln does *not* use libostree's `--transient` / `Transient` state, despite
//! the name: that state mounts the overlay **read-only**, writable only after
//! a manual `mount -o remount,rw /usr` inside a freshly unshared mount
//! namespace. Kiln's own "transient" (temporary, discarded on reboot) is a
//! description of the development state's behavior, not a reference to
//! libostree's distinct `Transient` enum variant.
//!
//! This is dev/test scratch space, not a second way to change a deployed
//! system: it never touches `plan_id`, the build record, or which generation
//! is committed or boots next.

use ostree::DeploymentUnlockedState as Raw;

/// Whether, and how, the booted deployment is unlocked right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockState {
    Locked,
    /// What `kiln unlock` and `kiln live` ask for, and the only state Kiln
    /// ever requests: a writable overlay, discarded on reboot.
    Development,
    /// Reported, never requested by Kiln: seeing this means `ostree admin
    /// unlock --hotfix` ran by hand, outside Kiln.
    Hotfix,
    /// Reported, never requested by Kiln: seeing this means `ostree admin
    /// unlock --transient` ran by hand, outside Kiln. Despite the name, this
    /// state mounts the overlay read-only until manually remounted read-write
    /// inside a fresh mount namespace — not the writable state Kiln wants.
    Transient,
}

impl UnlockState {
    pub(crate) fn of(raw: Raw) -> UnlockState {
        match raw {
            Raw::Transient => UnlockState::Transient,
            Raw::Hotfix => UnlockState::Hotfix,
            Raw::Development => UnlockState::Development,
            // `Raw::None` and any state a future libostree adds that this
            // Kiln does not know about both mean "not one of ours" — reporting
            // an unknown state as unlocked would tell `kiln live` it is safe
            // to write into a `/usr` that never actually became writable.
            _ => UnlockState::Locked,
        }
    }
}
