//! The transaction: packages into a staging root.
//!
//! Split into two verbs on purpose, along the line the plan/realize split draws through the whole
//! build: **`fetch` has the network and `install` does not.** libalpm would
//! happily do both in one sync transaction, downloading as it goes; separating
//! them is what lets assembly run with `CLONE_NEWNET` and no interfaces, and
//! therefore what makes a build's output a function of hashed inputs rather
//! than of whatever a mirror served at the time.

use crate::error::{Error, Result};
use crate::session::Session;
use alpm::{Event, LogLevel, PackageOperation, TransFlag};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What one transaction did, for the build log and for `kiln build -v`.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Installed package names, sorted.
    pub installed: Vec<String>,
    /// Scriptlet output, keyed by the package whose scriptlet produced it.
    /// captured per package, so a failure names the package and the last
    /// forty lines rather than a wall of undifferentiated text.
    pub scriptlets: Vec<ScriptletOutput>,
    /// Package-shipped alpm hooks that ran. They always run and cannot be
    /// disabled, so the honest thing is to record which ones did.
    pub hooks: Vec<String>,
    /// Everything libalpm logged at ERROR level, each paired with what was in
    /// flight when it was logged. Empty on a healthy transaction — see
    /// `install`, which refuses to call one successful while this is not.
    ///
    /// Paired *at the time*, not at the end. libalpm keeps going after a hook
    /// fails, so by the end of the transaction the last action is whichever
    /// hook happened to run last — and naming that one sends the reader to a
    /// hook that worked.
    pub errors: Vec<(Option<String>, String)>,
    /// The package operation or hook currently in flight, in Kiln's words. The
    /// only context a libalpm ERROR log gets.
    pub last_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptletOutput {
    pub package: String,
    pub lines: Vec<String>,
}

/// A set of packages to put into the root.
#[derive(Debug, Clone, Default)]
pub struct Transaction {
    /// Dep strings, resolved against the sync databases.
    pub packages: Vec<String>,
    /// Names to record as explicitly installed, so `pacman -Qe` on the booted
    /// image distinguishes what the configuration asked for from what came
    /// along. Anything not listed is marked as a dependency.
    pub explicit: Vec<String>,
    /// `.pkg.tar.zst` files installed from disk rather than resolved by name:
    /// everything realization *made* or was handed — an AUR package, a
    /// `packages.build` recipe's output, an out-of-tree kernel module, a
    /// `packages.file` blob.
    ///
    /// A separate list rather than more entries in `packages`, because they are
    /// a different question to libalpm. A name is looked up in a sync database;
    /// a file is loaded, and no database anywhere knows it exists. Putting a
    /// path in `packages` produces libalpm's "no package named
    /// `pkgbuilds/mytool`", which is a true statement about the wrong question.
    pub locals: Vec<PathBuf>,
}

impl Transaction {
    pub fn new(packages: impl IntoIterator<Item = String>) -> Transaction {
        Transaction {
            packages: packages.into_iter().collect(),
            explicit: Vec::new(),
            locals: Vec::new(),
        }
    }

    pub fn explicitly(mut self, names: impl IntoIterator<Item = String>) -> Transaction {
        self.explicit = names.into_iter().collect();
        self
    }

    /// Add package files to install from disk. Their dependencies are still
    /// resolved against the sync databases, which is what makes an AUR package
    /// that needs `qt6-base` work without the plan having to name it.
    pub fn with_locals(mut self, files: impl IntoIterator<Item = PathBuf>) -> Transaction {
        self.locals = files.into_iter().collect();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.locals.is_empty()
    }
}

impl Session {
    /// Download every package the transaction needs into the cache, and nothing
    /// else. **This is the only step in assembly that touches the network.**
    ///
    /// Returns the cached file paths, so realization can hand assembly a set of
    /// artifacts rather than a promise.
    ///
    /// The locals are added here too, even though a file on disk needs no
    /// downloading. They are what pulls their *dependencies* into the cache: an
    /// AUR package needs `qt6-base`, nothing in the plan names it (stops
    /// the closure wherever the official repositories can satisfy it), and
    /// assembly runs with the network off. Without this the transaction would
    /// resolve that dependency and then fail reaching for a mirror that is not
    /// there.
    pub fn fetch(&mut self, transaction: &Transaction) -> Result<Vec<PathBuf>> {
        let files = RefCell::new(Vec::new());
        self.run_transaction(
            transaction,
            TransFlag::DOWNLOAD_ONLY,
            Rc::new(RefCell::new(Report::default())),
        )?;

        // libalpm names the cached file after the package's `filename`, in the
        // first cachedir. Asking it where it put things is not part of the API,
        // so the paths are reconstructed from what was resolved.
        let cache = self
            .config
            .cachedirs
            .first()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/var/cache/pacman/pkg"));
        for name in &transaction.packages {
            if let Some(pkg) = self.alpm.syncdbs().find_satisfier(name.as_str()) {
                if let Some(filename) = pkg.filename() {
                    files.borrow_mut().push(cache.join(filename));
                }
            }
        }
        Ok(files.into_inner())
    }

    /// Install into the root. Assumes every package is already in the cache —
    /// see `fetch`. Runs with no network in the caller's namespace.
    pub fn install(&mut self, transaction: &Transaction) -> Result<Report> {
        // builds run as root, always. This is not ceremony. As an
        // ordinary user libalpm extracts the archive, fails every `chown`, logs
        // "Can't set user=0/group=0" as a *warning*, and reports the commit as
        // successful — producing a tree whose ownership, setuid bits and file
        // capabilities are all wrong, with no error anywhere. Refusing up front
        // is the difference between a clear message and an image that misbehaves
        // only once it is booted.
        if effective_uid() != 0 {
            return Err(Error::NotRoot);
        }

        let report = Rc::new(RefCell::new(Report::default()));
        self.run_transaction(transaction, TransFlag::empty(), Rc::clone(&report))?;

        let mut report = report.borrow().clone();
        report.installed.sort();
        report.installed.dedup();
        report.hooks.sort();
        report.hooks.dedup();

        // libalpm does **not** fail a transaction when a scriptlet fails. It
        // logs `command failed to execute correctly` at ERROR level and returns
        // success, which is the right call for `pacman -Syu` — a broken
        // scriptlet should not brick a running system — and the wrong one for
        // an image build, where the build is guaranteed to abort. Treating any
        // ERROR-level log during a commit as fatal is locale-independent, which
        // matching that sentence would not be.
        if !report.errors.is_empty() {
            // The build record promises "the package name and the last 40 lines". libalpm's
            // ERROR log is one line — `command failed to execute correctly` —
            // and everything that would explain it went to `ScriptletInfo`,
            // which is captured and was, until this, dropped on the floor.
            let mut messages: Vec<String> = report.errors.iter().map(|(_, m)| m.clone()).collect();
            messages.extend(tail(&report, 40));
            return Err(Error::TransactionErrors {
                during: report.errors[0].0.clone(),
                messages,
            });
        }
        Ok(report)
    }

    fn run_transaction(
        &mut self,
        transaction: &Transaction,
        flags: TransFlag,
        report: Rc<RefCell<Report>>,
    ) -> Result<()> {
        // libalpm's callbacks are the only route back into Rust from C, and
        // they must outlive the transaction, so the report is shared rather
        // than borrowed.
        let events = Rc::clone(&report);
        self.alpm.set_event_cb((), move |event, ()| {
            record(&mut events.borrow_mut(), &event.event())
        });
        let logs = Rc::clone(&report);
        self.alpm.set_log_cb((), move |level, msg, ()| {
            if level.contains(LogLevel::ERROR) {
                let mut report = logs.borrow_mut();
                let during = report.last_action.clone();
                report.errors.push((during, msg.trim_end().to_string()));
            }
        });

        self.alpm
            .trans_init(flags)
            .map_err(|e| Error::alpm("starting the transaction", e))?;

        let outcome = self.add_and_commit(transaction);

        let _ = self.alpm.trans_release();
        outcome
    }

    fn add_and_commit(&mut self, transaction: &Transaction) -> Result<()> {
        {
            let syncdbs = self.alpm.syncdbs();
            for want in &transaction.packages {
                let pkg = syncdbs
                    .find_satisfier(want.as_str())
                    .ok_or_else(|| Error::NotFound { name: want.clone() })?;
                if let Err(e) = self.alpm.trans_add_pkg(pkg) {
                    if e.error != alpm::Error::TransDupTarget {
                        return Err(Error::alpm("selecting a package", e.error));
                    }
                }
            }
        }
        for file in &transaction.locals {
            self.add_local(file)?;
        }

        if let Err(e) = self.alpm.trans_prepare() {
            return Err(crate::solve::prepare_error(&e));
        }
        if let Err(e) = self.alpm.trans_commit() {
            return Err(commit_error(&e));
        }
        Ok(())
    }

    /// Load a `.pkg.tar.zst` from disk and add it to the open transaction —
    /// what `pacman -U` does, without pacman.
    ///
    /// **`full = true`**: the whole archive is read and its `.MTREE` parsed, so
    /// the file list is known. Without it libalpm has only the metadata header,
    /// and a package whose file list it does not know cannot be checked for
    /// conflicts against the tree — which is the guarantee Kiln makes about
    /// packaged content, and the reason this route exists rather than
    /// `tar -x`.
    ///
    /// **No signature is required.** These are artifacts Kiln built in its own
    /// sandbox minutes ago, or a `packages.file` blob whose sha256 resolution
    /// already verified against the configuration — there is nobody to
    /// have signed them. That is not the `TRUST_ALL` Kiln refuses: nothing here
    /// came off a mirror, and every package that *did* still goes through
    /// `Trust::Required`.
    fn add_local(&mut self, file: &Path) -> Result<()> {
        let path = file.to_str().ok_or_else(|| Error::UnreadablePackage {
            path: file.to_path_buf(),
            message: "the path is not valid UTF-8".into(),
        })?;
        let loaded = self
            .alpm
            .pkg_load(path, true, alpm::SigLevel::empty())
            .map_err(|e| Error::UnreadablePackage {
                path: file.to_path_buf(),
                message: e.to_string(),
            })?;
        match self.alpm.trans_add_pkg(loaded) {
            Ok(()) => Ok(()),
            // The same package twice — two plan inputs that resolved to one
            // artifact, or a split recipe whose outputs overlap. Harmless.
            Err(e) if e.error == alpm::Error::TransDupTarget => Ok(()),
            Err(e) => Err(Error::UnreadablePackage {
                path: file.to_path_buf(),
                message: e.error.to_string(),
            }),
        }
    }
}

/// The last `n` lines a scriptlet or hook printed, newest bucket last.
///
/// Hook output arrives through the same `ScriptletInfo` event as a package's,
/// so this covers both — which matters, because a hook failure is the case
/// where the one-line ERROR log explains least.
fn tail(report: &Report, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = report
        .scriptlets
        .iter()
        .flat_map(|s| s.lines.iter().cloned())
        .collect();
    if lines.len() > n {
        lines = lines.split_off(lines.len() - n);
    }
    lines
}

/// The effective uid, read from `/proc`. Kiln is Linux-only — it builds OSTree
/// images from pacman packages — so this is not worth a libc dependency.
/// Field two of `Uid:` is the effective uid.
fn effective_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(2)?
                .parse()
                .ok()
        })
        // Unknown is treated as "not root": refusing a build that might have
        // worked is recoverable, and shipping an image with broken ownership is
        // not.
        .unwrap_or(u32::MAX)
}

fn record(report: &mut Report, event: &Event<'_>) {
    match event {
        Event::PackageOperationStart(op) => {
            if let PackageOperation::Install(pkg) = op.operation() {
                report.installed.push(pkg.name().to_string());
                report.last_action = Some(format!("the package `{}`", pkg.name()));
                // A new package's scriptlet output belongs to it, so open a
                // bucket now rather than guessing later.
                report.scriptlets.push(ScriptletOutput {
                    package: pkg.name().to_string(),
                    lines: Vec::new(),
                });
            }
        }
        Event::ScriptletInfo(info) => {
            let line = info.line().trim_end().to_string();
            if line.is_empty() {
                return;
            }
            match report.scriptlets.last_mut() {
                Some(bucket) => bucket.lines.push(line),
                // A scriptlet with no package operation before it should not
                // happen; keeping the line under a name that says so beats
                // dropping it.
                None => report.scriptlets.push(ScriptletOutput {
                    package: "<unknown>".into(),
                    lines: vec![line],
                }),
            }
        }
        Event::HookRunStart(hook) => {
            report.hooks.push(hook.name().to_string());
            report.last_action = Some(format!("the alpm hook `{}`", hook.name()));
        }
        _ => {}
    }
}

/// Reinterpret what `CommitData::FileConflict` hands back as what libalpm
/// actually put there.
///
/// The `alpm` crate types that list as `AlpmList<&Conflict>` — a *package*
/// conflict, with `package1`/`package2`/`reason`. libalpm returns
/// `alpm_fileconflict_t` there, with `target`/`file`/`conflicting_target`, and
/// the crate's own `Drop for CommitError` agrees: it frees the list as
/// `OwnedFileConflict`. Calling `Conflict::package1()` on one of these
/// pointers would read the wrong struct.
///
/// Both `Conflict` and `FileConflict` are `#[repr(transparent)]` newtypes over
/// their respective C structs, so reinterpreting a reference whose pointee is
/// genuinely an `alpm_fileconflict_t` is well-defined. This is the only
/// `unsafe` in Kiln, it exists to work around an upstream mistyping, and it
/// should be deleted the moment `alpm` fixes the signature — at which point
/// this function stops compiling, which is the right way to be reminded.
fn as_file_conflict(c: &alpm::Conflict) -> &alpm::FileConflict {
    unsafe { &*(c as *const alpm::Conflict as *const alpm::FileConflict) }
}

fn commit_error(e: &alpm::CommitError) -> Error {
    match e.data() {
        Some(alpm::CommitData::FileConflict(conflicts)) => conflicts
            .into_iter()
            .next()
            .map(|c| {
                let c = as_file_conflict(c);
                Error::FileConflict {
                    package: c.target().to_string(),
                    path: c.file().to_string(),
                    owner: c.conflicting_target().map(str::to_string),
                }
            })
            .unwrap_or_else(|| Error::alpm("committing the transaction", e.error())),
        Some(alpm::CommitData::PkgInvalid(pkgs)) => pkgs
            .into_iter()
            .next()
            .map(|name| Error::PackageInvalid {
                name: name.to_string(),
            })
            .unwrap_or_else(|| Error::alpm("committing the transaction", e.error())),
        None => Error::alpm("committing the transaction", e.error()),
    }
}
