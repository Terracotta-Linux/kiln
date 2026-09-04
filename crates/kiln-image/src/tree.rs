//! Small filesystem helpers, and the error they all report.
//!
//! Every operation names the path it was working on. A normalization failure
//! that says only "No such file or directory" is a bad afternoon.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io {
        doing: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// The tree is not in the shape this step requires. Distinct from an I/O
    /// error because it means a *previous* step went wrong, and saying so saves
    /// the reader from chasing a missing file that was never supposed to exist.
    Shape {
        what: String,
    },
    /// Content the configuration asked for that cannot go where it was asked to
    /// go. Plural on purpose: this reports every error in a phase, so four
    /// impossible targets are four lines and one run, not four builds.
    Refused {
        /// What is being refused, singular and plural. The step that found the
        /// problem names it: a unit refusal that calls itself a `[[file]]`
        /// entry sends the reader to the wrong part of their configuration.
        noun: (&'static str, &'static str),
        problems: Vec<crate::overlay::Refusal>,
    },
    Sandbox(kiln_sandbox::Error),
    Alpm(kiln_alpm::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io {
                doing,
                path,
                source,
            } => {
                write!(f, "{doing} {}: {source}", path.display())
            }
            Error::Shape { what } => f.write_str(what),
            Error::Refused { noun, problems } => {
                let n = problems.len();
                let word = if n == 1 { noun.0 } else { noun.1 };
                writeln!(f, "{n} {word} cannot be realized:")?;
                for p in problems {
                    writeln!(f, "  {} — {}", p.target, p.why)?;
                    if let Some(hint) = &p.hint {
                        writeln!(f, "      {hint}")?;
                    }
                }
                Ok(())
            }
            Error::Sandbox(e) => write!(f, "{e}"),
            Error::Alpm(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<kiln_sandbox::Error> for Error {
    fn from(e: kiln_sandbox::Error) -> Error {
        Error::Sandbox(e)
    }
}

impl From<kiln_alpm::Error> for Error {
    fn from(e: kiln_alpm::Error) -> Error {
        Error::Alpm(e)
    }
}

pub fn io<'p>(doing: &'static str, path: &'p Path) -> impl Fn(std::io::Error) -> Error + 'p {
    move |source| Error::Io {
        doing,
        path: path.to_path_buf(),
        source,
    }
}

pub fn shape(what: impl Into<String>) -> Error {
    Error::Shape { what: what.into() }
}

/// The mode every directory Kiln creates itself gets.
///
/// Directories a *package* ships keep whatever the package declared; this is
/// only for the ones Kiln makes — the skeleton, the parents of a `[[file]]`
/// target, `usr/lib/kiln`.
pub const DIR_MODE: u32 = 0o755;

/// Create a directory and its parents, with an explicit mode.
///
/// **Not `create_dir_all`.** `mkdir(2)` masks the mode it is given by the
/// calling process's umask, so `create_dir_all` puts the *builder's* umask into
/// the image: the same configuration built by a shell at `umask 022` and one at
/// `umask 0` produces `0755` and `0777` directories respectively, and the two
/// commits differ. exists to stop exactly that — a build must not be able
/// to tell anything about the machine it ran on — and this was the last place
/// it could.
///
/// It surfaced as a snapshot test that flapped between two mode columns
/// depending on how the suite was invoked, which reads like a flaky test and
/// was a real difference in the image.
///
/// Only directories this call *creates* are chmodded. One that already exists
/// keeps its mode, because it belongs to whatever put it there — usually a
/// package, and re-moding a package's directory is a change Kiln has no
/// business making.
pub fn mkdir(path: &Path) -> Result<()> {
    let mut at = PathBuf::new();
    for component in path.components() {
        at.push(component);
        match std::fs::create_dir(&at) {
            Ok(()) => set_mode(&at, DIR_MODE)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io("creating", &at)(e)),
        }
    }
    Ok(())
}

pub fn write(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        mkdir(parent)?;
    }
    std::fs::write(path, body).map_err(io("writing", path))
}

pub fn symlink(target: &str, at: &Path) -> Result<()> {
    if at.symlink_metadata().is_ok() {
        remove(at)?;
    }
    if let Some(parent) = at.parent() {
        mkdir(parent)?;
    }
    std::os::unix::fs::symlink(target, at).map_err(io("linking", at))
}

pub fn remove(path: &Path) -> Result<()> {
    let md = match path.symlink_metadata() {
        Ok(md) => md,
        Err(_) => return Ok(()),
    };
    // `is_dir()` follows symlinks; a symlink *to* a directory must be unlinked,
    // not recursed into and deleted.
    if md.file_type().is_dir() {
        std::fs::remove_dir_all(path).map_err(io("removing", path))
    } else {
        std::fs::remove_file(path).map_err(io("removing", path))
    }
}

/// Directory entries, sorted by name. Every walk in this crate goes through
/// here, because the order the kernel hands back entries is not stable and the
/// commit must be.
pub fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io("listing", dir)(e)),
    };
    out.sort();
    Ok(out)
}

pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(io("setting the mode of", path))
}

pub fn tree_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        for e in entries(&at).unwrap_or_default() {
            match e.symlink_metadata() {
                Ok(md) if md.file_type().is_dir() => stack.push(e),
                Ok(md) => total += md.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// `id → name`, read from a passwd- or group-formatted file.
///
/// The drain writes tmpfiles lines naming a *user*, not a uid, because the
/// numbers are allocated at build time and the names are what a person reading
/// `/usr/lib/tmpfiles.d/kiln-var.conf` can check. The first name for an id
/// wins, matching what `getpwuid` would answer.
pub fn id_map(path: &Path) -> BTreeMap<u32, String> {
    let mut m = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return m;
    };
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if let (Some(name), Some(Ok(id))) =
            (fields.first(), fields.get(2).map(|s| s.parse::<u32>()))
        {
            m.entry(id).or_insert_with(|| name.to_string());
        }
    }
    m
}

/// A human-readable size, for build output and warnings.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A directory Kiln creates must have the same mode whatever the
    /// builder's umask is.
    ///
    /// This was a real bug: `mkdir` was `create_dir_all`, `mkdir(2)` masks its
    /// mode by the umask, and the same configuration built from a shell at
    /// `umask 022` and one at `umask 0` produced `0755` and `0777` directories
    /// — two different commits from one configuration. It showed up as a
    /// snapshot test that flapped depending on how the suite was invoked, which
    /// reads like flakiness and was a real difference in the image.
    ///
    /// The umask is process-wide, so this test sets and restores it. It is the
    /// only test in the crate that does, and it is the one that has to.
    #[test]
    fn a_directory_kiln_creates_does_not_inherit_the_builders_umask() {
        let base = std::env::temp_dir().join(format!("kiln-umask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let mut modes = Vec::new();
        for mask in [0o022, 0o000, 0o077] {
            // SAFETY: `umask` is always successful and has no preconditions. It
            // is process-wide, which is why the previous value is restored.
            let previous = unsafe { libc::umask(mask) };
            let at = base.join(format!("{mask:o}/deep/nested"));
            mkdir(&at).expect("creating a directory");
            unsafe { libc::umask(previous) };

            modes.push((
                mask,
                std::fs::metadata(&at).unwrap().permissions().mode() & 0o7777,
                std::fs::metadata(at.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
            ));
        }
        let _ = std::fs::remove_dir_all(&base);

        for (mask, leaf, parent) in modes {
            assert_eq!(leaf, DIR_MODE, "umask {mask:o} reached the leaf directory");
            assert_eq!(
                parent, DIR_MODE,
                "umask {mask:o} reached a directory created on the way"
            );
        }
    }

    /// A directory that already exists keeps its mode. It belongs to whatever
    /// put it there — usually a package — and re-moding a package's directory
    /// is a change Kiln has no business making.
    #[test]
    fn an_existing_directory_keeps_the_mode_it_had() {
        let at = std::env::temp_dir().join(format!("kiln-existing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).unwrap();
        set_mode(&at, 0o700).unwrap();

        mkdir(&at).expect("creating a directory that is already there");

        let mode = std::fs::metadata(&at).unwrap().permissions().mode() & 0o7777;
        let _ = std::fs::remove_dir_all(&at);
        assert_eq!(mode, 0o700);
    }
}
