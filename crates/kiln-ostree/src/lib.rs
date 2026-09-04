//! `kiln-ostree` — libostree integration.
//!
//! Through the GObject-introspection bindings, not `ostree(1)`. Shelling out
//! would mean parsing human-readable output for machine decisions, and the two
//! decisions this crate makes — which commit is which generation, and which
//! deployment boots next — are exactly the ones where a changed output format
//! becomes a wrong boot.
//!
//! Two things about libostree that the CLI documents and the library inherits,
//! both verified against ostree 2026.4 in the phase 0 spike:
//!
//! - **There is no `rollback` verb.** `set-default`, `undeploy` and `pin` are
//!   what exist. `kiln rollback` is Kiln's own operation over the deployment
//!   list, not a passthrough to something that exists.
//! - **BLS boot order is the inverse of the entry filenames.** ostree writes
//!   `ostree-1.conf`, `ostree-2.conf`, …, and the deployment that boots is the
//!   entry with the *highest* BLS `version`, which is the highest-numbered
//!   file. Anything sorting entries by filename to decide what boots next picks
//!   the rollback deployment.

pub mod commit;
pub mod deploy;
pub mod drift;
pub mod entries;
pub mod generation;
pub mod grubcfg;
pub mod grubenv;

pub use commit::{commit, CommitOptions, Committed};
pub use deploy::{
    deploy, rollback, Backend, Counter, Deployed, Generation, Removal, Sysroot, BASELINE,
};
pub use drift::{Change, How};
pub use generation::{Metadata, KEY_PREFIX};
pub use grubenv::Counting;

use std::fmt;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// libostree said no. `doing` is Kiln's words for what was being attempted,
    /// because glib's own messages name a function, not an intention.
    Ostree {
        doing: &'static str,
        message: String,
    },
    /// The commit is not one of Kiln's, or is from a Kiln that recorded
    /// something this one cannot read.
    NotOurs {
        checksum: String,
        why: String,
    },
    NoSuchGeneration {
        wanted: u64,
        available: Vec<u64>,
    },
    Io {
        doing: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn of(doing: &'static str) -> impl Fn(glib::Error) -> Error {
        move |e| Error::Ostree {
            doing,
            message: e.message().to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ostree { doing, message } => write!(f, "{doing}: {message}"),
            Error::NotOurs { checksum, why } => {
                write!(f, "commit {checksum} {why}")
            }
            Error::NoSuchGeneration { wanted, available } => {
                write!(f, "there is no generation {wanted}")?;
                if available.is_empty() {
                    // "generations", not "deployments": this error is raised
                    // both by the deployment list and by a search over commits
                    // (`find_generation`), and a generation that is committed
                    // and not deployed is still one this machine has.
                    write!(f, "; this machine has no Kiln generations yet")
                } else {
                    let list: Vec<String> = available.iter().map(u64::to_string).collect();
                    write!(f, "; this machine has {}", list.join(", "))
                }
            }
            Error::Io {
                doing,
                path,
                source,
            } => write!(f, "{doing} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}
