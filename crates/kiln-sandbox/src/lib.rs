//! `kiln-sandbox` — build isolation.
//!
//! The trait exists so both backends are swappable and so the whole layer can
//! be faked in tests (*sandbox tests assert on the exact `SandboxSpec`*).
//! What is actually asserted here is one step stronger — the argv each backend
//! produces — because a spec that says `Network::Disabled` and a backend that
//! forgets `--unshare-net` is exactly the failure a spec-only test misses.
//!
//! It is a **namespace sandbox, not a VM**: a kernel LPE escapes it.
//! states that plainly rather than implying it away.

pub mod bwrap;
pub mod nspawn;
pub mod spec;

pub use bwrap::Bubblewrap;
pub use nspawn::Nspawn;
pub use spec::{
    Bind, BindMode, Limits, Network, SandboxSpec, SandboxUser, Shim, SHIM_DIR, SHIM_LOG,
};

use std::fmt;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The backend binary is not installed.
    Missing { backend: &'static str, hint: String },
    /// The spec asks for something this backend cannot enforce. Refusing is
    /// deliberate: silently not applying a limit is worse than not having one,
    /// because the caller believes it is protected.
    Unsupported { backend: &'static str, what: String },
    /// The command ran and failed. `stderr` is the tail, not the whole log.
    Failed {
        command: String,
        status: i32,
        stderr: String,
    },
    /// Killed after `Limits::wall`.
    TimedOut {
        command: String,
        after: std::time::Duration,
    },
    Io {
        doing: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Missing { backend, hint } => write!(f, "{backend} is not available: {hint}"),
            Error::Unsupported { backend, what } => {
                write!(f, "the {backend} sandbox cannot enforce {what}")
            }
            Error::Failed {
                command,
                status,
                stderr,
            } => {
                write!(f, "`{command}` failed with exit status {status}")?;
                if !stderr.is_empty() {
                    write!(f, "\n{stderr}")?;
                }
                Ok(())
            }
            Error::TimedOut { command, after } => {
                write!(f, "`{command}` did not finish within {after:?}")
            }
            Error::Io {
                doing,
                path,
                source,
            } => {
                write!(f, "{doing} {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for Error {}

/// What a sandboxed run produced.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// Every shimmed call, in the order it happened. `kiln build -v`
    /// should show `shimmed: systemctl daemon-reload`, because a scriptlet
    /// quietly failing to do what it thinks it did is worth knowing about.
    pub shimmed: Vec<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

pub trait Sandbox {
    /// A name for diagnostics and for `kiln build -v`.
    fn name(&self) -> &'static str;

    /// The argv this backend would run. Separated from `run` so that what the
    /// isolation actually *is* can be asserted in a test without needing root,
    /// a container, or the backend to be installed.
    fn argv(&self, spec: &SandboxSpec) -> Result<Vec<String>>;

    fn run(&self, spec: &SandboxSpec) -> Result<Outcome>;
}

/// Shared machinery: materialize the shims, spawn, enforce the wall clock,
/// collect the shim log. Both backends want all four and neither should have
/// its own version.
pub(crate) mod exec;
