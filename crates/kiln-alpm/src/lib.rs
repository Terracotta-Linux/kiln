//! `kiln-alpm` — libalpm, as a library rather than a subprocess.
//!
//! Kiln constructs the alpm handle programmatically — root, dbpath, cachedir,
//! arch, siglevel, registered sync DBs — rather than generating a `pacman.conf`
//! and shelling out. That buys the real dependency solver, structured
//! transaction callbacks, and direct queries for `kiln why` / `kiln owns`.
//!
//! This crate has no dependency on `kiln-diag`. Its errors are structured
//! values naming packages and dependencies; attaching them to the TOML line
//! that asked for the package is `kiln-resolve`'s job, because that is where
//! the manifest's provenance lives. Keeping the split means the libalpm layer
//! can be reasoned about — and tested — without a config in sight.
//!
//! ```text
//! Session::open(&Config)  →  refresh()  →  solve(&Request)  →  Solution
//!                                                              (metadata only:
//!                                                               nothing is
//!                                                               downloaded or
//!                                                               unpacked)
//! ```

pub mod error;
pub mod keyring;
pub mod mounts;
pub mod repo;
pub mod session;
pub mod solve;
pub mod transact;

pub use error::{Error, Result};
pub use mounts::Mounts;
pub use repo::{mirrors, RepoSpec, Trust};
pub use session::{Config, Installed, Session};
pub use solve::{Request, Solution, SolvedPackage};
pub use transact::{Report, ScriptletOutput, Transaction};

/// The sha256 of a file, computed by libalpm.
///
/// Kiln requires a checksum on every local package, and records one for
/// every repository package; both are sha256 because that is what pacman's
/// databases and `.PKGBUILD`s use. Borrowing libalpm's implementation keeps a
/// second hash library out of the workspace and guarantees the digest matches
/// the one the transaction will check.
pub fn sha256(path: &std::path::Path) -> Option<String> {
    alpm::compute_sha256sum(path.to_str()?).ok()
}

/// The libalpm this binary is linked against. Recorded in the build record so a
/// past build says which solver produced it.
pub fn libalpm_version() -> &'static str {
    alpm::version()
}
