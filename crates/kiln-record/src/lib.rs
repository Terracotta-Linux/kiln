//! `kiln-record` — the build record.
//!
//! **There is no lockfile.** Nothing about resolution lives in the
//! configuration directory and nothing goes in git. Every commit carries a
//! complete record of what went into it, in two places: commit metadata
//! (`kiln.record`, zstd JSON) and `/usr/lib/kiln/record.json` inside the tree.
//!
//! That falls out of having OSTree underneath. Cargo and npm need a lockfile
//! because there is nowhere else to persist a resolution; here there is already
//! a content-addressed, versioned, self-describing store holding every build
//! that has ever happened. A parallel file in `/etc` would be a second source
//! of truth that can disagree with the first, that produces merge conflicts,
//! that goes stale on a machine someone rolled back, and that the user is
//! expected to carry around.
//!
//! JSON rather than TOML precisely *because* it is not user-facing: machine
//! written, machine read, and there is no reason to spend readability on a file
//! nobody opens. It is internal machinery for update checking, `kiln diff` and
//! `kiln rebuild` — not a user-facing file, and not a stable interface.

pub mod record;

pub use record::{
    AurEntry, BuiltEntry, LocalFile, LocalPackage, Record, RecordedIds, RecordedUser, RepoEntry,
    RepoSnapshot, SourceEntry, FORMAT, IN_IMAGE, METADATA_KEY,
};

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// A record from a newer Kiln. Refusing beats guessing: the record drives
    /// UID replay and `kiln rebuild`, and half-understanding one produces a
    /// wrong image rather than an error.
    Unsupported {
        found: u32,
        understood: u32,
    },
    Malformed(serde_json::Error),
    Io {
        doing: &'static str,
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported { found, understood } => write!(
                f,
                "this commit's build record is format {found}; this Kiln understands \
                 {understood}. The image was built by a newer Kiln — upgrade, or build a \
                 new generation from your configuration instead of reading this one"
            ),
            Error::Malformed(e) => write!(f, "the build record is not readable: {e}"),
            Error::Io {
                doing,
                path,
                source,
            } => write!(f, "{doing} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}
