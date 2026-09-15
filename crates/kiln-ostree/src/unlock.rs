//! `ostree admin unlock`, transient only.
//!
//! Kiln exposes exactly one unlocked state: **transient**. It bind-mounts the
//! booted deployment's `/usr` writable for the rest of this boot, and nothing
//! about it survives — not `--hotfix`, which commits a separate overlay
//! libostree would otherwise keep around across reboots, and not
//! `--development` (hotfix plus a marker asking `apply`/`upgrade` to leave it
//! alone). Both exist for workflows Kiln does not have: there is no
//! live-shipping path here to protect, and a state that outlives a reboot is
//! exactly what a command whose whole premise is "for testing, discarded on
//! reboot" must not leave behind by accident — including a reboot back into
//! the very generation that was unlocked.
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
    /// ever requests.
    Transient,
    /// Reported, never requested by Kiln: seeing this means `ostree admin
    /// unlock --hotfix` ran by hand, outside Kiln.
    Hotfix,
    /// Same, for `ostree admin unlock --development`.
    Development,
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
