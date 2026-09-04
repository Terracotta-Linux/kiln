//! `kiln-aur` — AUR resolution.
//!
//! > The AUR is the largest security and reproducibility hole in the design, so
//! > it is handled with visible seams.
//!
//! The seams, concretely:
//!
//! - **Identity is the git commit**, not the version string, so a force-push
//!   with an unchanged `pkgver` is a detected change.
//! - **Nothing enters the image anonymously**: every transitively pulled
//!   package records what required it.
//! - **VCS packages are volatile, not guessed.** An untrustworthy
//!   `kiln check` is worse than no `kiln check`.
//! - **Building is the ordinary PKGBUILD path**. There is no separate
//!   AUR builder, which is why there is no builder in this crate.

pub mod closure;
pub mod rpc;
pub mod transport;

pub use closure::{resolve, Closure, Error, Resolved};
pub use rpc::Info;
pub use transport::{Network, Recorded, Transport};

/// The git repository backing an AUR package base.
pub fn repository(pkgbase: &str) -> String {
    format!("https://aur.archlinux.org/{pkgbase}.git")
}
