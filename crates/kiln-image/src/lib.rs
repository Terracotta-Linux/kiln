//! `kiln-image` — filesystem tree assembly and normalization.
//!
//! Arch is not an OSTree distribution, and most of what could kill this project
//! lives here. Phase 0 proved the contract is satisfiable; this crate is
//! the real implementation of what the spike proved, with the findings from
//! `spike/README.md` built in rather than rediscovered.
//!
//! ```text
//!  1 skeleton          usr/lib/sysimage/pacman and the mountpoints, nothing else
//!  2 base transaction  `filesystem` alone, so step 3 has an /etc/passwd to seed
//!  3 UID seed          replay the previous generation's ids
//!  4 transaction       everything else, hooks shadowed
//!  6 overlay           [[file]], checked against the pacman file database
//!  7 unit state        presets, masks
//!  9 kernel            depmod, initramfs, /boot cleared
//! 10 normalize         the OSTree contract: /etc, /var, the top level
//! 11 self-description  usr/lib/kiln/{manifest.json,record.json}
//! ```
//!
//! Plus `bootcount`, which is not a step of its own: automatic rollback
//! is three files, written alongside steps 6 and 7 because two of them are
//! ordinary image content and the third is a unit like any other.
//!
//! Steps 5 and 8 are build scripts, which phase 3 owns.

pub mod assemble;
pub mod bootcount;
pub mod determinism;
pub mod drain;
pub mod hooks;
pub mod kernel;
pub mod normalize;
pub mod overlay;
pub mod scripts;
pub mod skeleton;
pub mod tree;
pub mod uid;
pub mod units;
pub mod verify;

pub use tree::{Error, Result};
