//! What libalpm can refuse to do, in terms Kiln can explain.
//!
//! Every variant carries the *names* involved, never a formatted sentence, so
//! that `kiln-resolve` can point the message at the TOML line that asked for
//! the package. A pre-rendered string would throw that away.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A package named in the configuration is in no registered repository.
    NotFound { name: String },
    /// A dependency nothing in the repositories provides.
    Unsatisfied {
        /// The package that wants it, when libalpm says.
        wanted_by: Option<String>,
        dep: String,
    },
    /// Two packages in the solution cannot coexist.
    Conflict {
        first: String,
        second: String,
        reason: String,
    },
    /// `packages.exclude` names something the solution contains anyway.
    /// Kiln refuses rather than dropping it: silently removing a dependency
    /// produces an image that is broken in a way nobody asked for.
    Excluded {
        name: String,
        /// The packages in the solution that depend on it, sorted.
        pulled_in_by: Vec<String>,
    },
    /// A package built for an architecture this image is not.
    WrongArch { name: String, arch: String },
    /// Two packages ship the same path, or a package would overwrite a file
    /// something else owns. Reported by alpm at assembly time with a
    /// precise message, rather than discovered at runtime.
    FileConflict {
        package: String,
        path: String,
        /// The other package, when there is one. A conflict against an
        /// *unowned* file on disk has no second package, and saying so is the
        /// difference between a real answer and a confusing one.
        owner: Option<String>,
    },
    /// builds run as root, always.
    NotRoot,
    /// libalpm reported the commit as successful while logging errors — the
    /// shape a failed scriptlet takes (see `Session::install`).
    TransactionErrors {
        /// What was in flight — a package operation or an alpm hook — in words
        /// rather than as a bare name, because the two read very differently
        /// and libalpm's ERROR log distinguishes neither.
        during: Option<String>,
        messages: Vec<String>,
    },
    /// A `.pkg.tar.zst` handed to the transaction directly — something Kiln
    /// built, or a `packages.file` entry — that libalpm could not read as a
    /// package.
    UnreadablePackage {
        path: std::path::PathBuf,
        message: String,
    },
    /// A cached package failed verification — a bad signature, or a checksum
    /// that does not match what the database said.
    PackageInvalid { name: String },
    /// A kernel filesystem could not be mounted into the root a transaction is
    /// about to run against. See `mounts`: without them, a package's hooks fail
    /// in ways that name the wrong file entirely.
    Mount {
        at: std::path::PathBuf,
        message: String,
    },
    /// Refreshing a sync database failed. `repo` names which one.
    Refresh { repo: String, message: String },
    /// Anything libalpm reported that has no better shape here.
    Alpm {
        doing: &'static str,
        message: String,
    },
}

impl Error {
    pub(crate) fn alpm(doing: &'static str, e: alpm::Error) -> Error {
        Error::Alpm {
            doing,
            message: e.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound { name } => {
                write!(f, "no package named `{name}` in any configured repository")
            }
            Error::Unsatisfied { wanted_by, dep } => match wanted_by {
                Some(w) => write!(f, "`{w}` requires `{dep}`, which nothing provides"),
                None => write!(f, "nothing provides `{dep}`"),
            },
            Error::Mount { at, message } => {
                write!(f, "preparing {} for a transaction: {message}", at.display())
            }
            Error::UnreadablePackage { path, message } => write!(
                f,
                "`{}` is not a readable pacman package: {message}",
                path.display()
            ),
            Error::Conflict {
                first,
                second,
                reason,
            } => write!(f, "`{first}` and `{second}` conflict over `{reason}`"),
            Error::Excluded { name, pulled_in_by } => {
                write!(f, "`{name}` is excluded but the image would contain it")?;
                if !pulled_in_by.is_empty() {
                    write!(f, "; required by {}", join(pulled_in_by))?;
                }
                Ok(())
            }
            Error::FileConflict {
                package,
                path,
                owner,
            } => match owner {
                Some(o) => write!(f, "`{package}` and `{o}` both ship `{path}`"),
                None => write!(
                    f,
                    "`{package}` would overwrite `{path}`, which it does not own"
                ),
            },
            Error::NotRoot => f.write_str(
                "a package transaction needs root: without it every chown fails, libalpm \
                 reports success anyway, and the image gets the wrong ownership, setuid \
                 bits and file capabilities",
            ),
            Error::TransactionErrors { during, messages } => {
                match during {
                    Some(what) => write!(f, "the transaction failed while running {what}")?,
                    None => write!(f, "the transaction reported errors")?,
                }
                for m in messages {
                    write!(f, "\n  {m}")?;
                }
                Ok(())
            }
            Error::PackageInvalid { name } => {
                write!(f, "`{name}` failed verification")
            }
            Error::WrongArch { name, arch } => {
                write!(f, "`{name}` is built for {arch}")
            }
            Error::Refresh { repo, message } => {
                write!(f, "could not refresh the `{repo}` database: {message}")
            }
            Error::Alpm { doing, message } => write!(f, "{doing}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

fn join(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [a] => format!("`{a}`"),
        [a, b] => format!("`{a}` and `{b}`"),
        [rest @ .., last] => {
            let head: Vec<String> = rest.iter().map(|n| format!("`{n}`")).collect();
            format!("{}, and `{last}`", head.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_names_every_dependent() {
        let e = Error::Excluded {
            name: "nano".into(),
            pulled_in_by: vec!["base".into(), "vi".into(), "zsh".into()],
        };
        assert_eq!(
            e.to_string(),
            "`nano` is excluded but the image would contain it; \
             required by `base`, `vi`, and `zsh`"
        );
    }

    #[test]
    fn excluded_with_no_dependents_is_a_direct_request() {
        let e = Error::Excluded {
            name: "nano".into(),
            pulled_in_by: vec![],
        };
        assert_eq!(
            e.to_string(),
            "`nano` is excluded but the image would contain it"
        );
    }
}
