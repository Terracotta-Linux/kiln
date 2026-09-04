//! Steps 5 and 8: build scripts, the escape hatch.
//!
//! The whole module rests on one sentence from the design: *a script does not
//! write to the staging root directly, it runs against an overlayfs upper
//! layer, which means the upper layer **is** the set of changes it made*. That
//! is not an optimization over diffing the tree — it is what makes the four
//! promises affordable at all. Visibility, conflict detection,
//! ownership and the determinism audit all fall out of reading one directory
//! that the kernel filled in for us.
//!
//! ```text
//!   lowerdir = the staging root, read-only from the script's point of view
//!   upperdir = empty when the script starts; afterwards, exactly its changes
//!   merged   = what the script sees as `/`, chrooted, with no network
//! ```
//!
//! After the run the upper layer is read, checked, hashed into the build
//! record, and then merged down into the staging root. The staging root is
//! never what the script wrote to, so a script that fails leaves it untouched.
//!
//! Three mount options are load-bearing and none of them is a default:
//!
//! - `metacopy=off` — with metacopy on, changing only a file's mode copies up
//!   an empty file with a redirect xattr instead of the data. The upper layer
//!   then contains a zero-byte stand-in for a file that still has its contents,
//!   and merging it down would truncate a package's binary to nothing.
//! - `redirect_dir=off` — a renamed directory is otherwise recorded as an
//!   xattr pointing at the old path rather than as content, which this module
//!   would have to interpret to avoid losing the rename. With it off the kernel
//!   copies the directory up, and the upper layer stays readable as "what is
//!   now here".
//! - `index=off` — the index is a cross-mount optimization with nothing to
//!   offer a mount that exists for one command and is then torn down.
//!
//! **Deletions.** overlayfs records a removed path as a character device with
//! device number 0, and a directory that was removed and recreated as a real
//! directory carrying `trusted.overlay.opaque`. Both are read here and both
//! become real removals in the staging root. Missing the opaque case would
//! leave the old directory's files in the image, present and stale, which is
//! the kind of wrong that no test notices until something boots strangely.

use crate::overlay::{Owners, Refusal};
use crate::tree::{self, Error, Result};
use kiln_manifest::{Hash, Script, ScriptPhase};
use kiln_sandbox::{Bind, Sandbox, SandboxSpec};
use std::collections::BTreeMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the script's own text is bind-mounted inside the sandbox.
///
/// Under `/run`, which the sandbox gives every command as a private tmpfs, so
/// the script is visible to itself and to nothing that outlives the run. Fixed
/// rather than generated, for the same reason `SHIM_DIR` is: a path printed in
/// a log should be the same path in every build.
pub const SCRIPT_IN_SANDBOX: &str = "/run/kiln/script";

/// A path the script created or replaced, staging-root-relative made absolute.
///
/// The path is the *image's*, as the user would type it — `/etc/locale.gen`,
/// not `/usr/etc/locale.gen`. Normalization has not moved `/etc` yet when a
/// script runs, and naming a path that does not exist at the moment it is
/// reported would be a small lie in the one output a user reads to understand
/// what their script did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub path: String,
    pub bytes: u64,
}

/// A path the script wrote that a package already owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clobber {
    pub path: String,
    pub package: String,
}

/// What one script did, as read out of its overlay upper layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changeset {
    /// Files and symlinks, sorted. Directories are not listed: a script that
    /// writes one file into a new tree made four directories on the way, and
    /// saying so four times obscures the one thing it actually did.
    pub wrote: Vec<Written>,
    /// Paths that are gone from the image because the script removed them.
    pub deleted: Vec<String>,
}

impl Changeset {
    pub fn is_empty(&self) -> bool {
        self.wrote.is_empty() && self.deleted.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Ran {
    pub name: String,
    pub phase: ScriptPhase,
    pub changeset: Changeset,
    /// The changeset's identity, for the build record. Two
    /// builds of the same script text that produce different digests have found
    /// the one script in the configuration that is not reproducible.
    pub digest: Hash,
    pub clobbered: Vec<Clobber>,
    /// Whatever the script printed, both streams. Shown by `kiln build -v`, and
    /// in full on a failure.
    pub output: String,
}

#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub ran: Vec<Ran>,
    /// Things worth saying that are not failures — a script that wrote nothing,
    /// a script that overwrote a package's file.
    pub notes: Vec<String>,
}

impl Applied {
    /// Name → changeset digest, as the build record stores it.
    pub fn effects(&self) -> BTreeMap<String, String> {
        self.ran
            .iter()
            .map(|r| (r.name.clone(), r.digest.to_string()))
            .collect()
    }

    pub fn absorb(&mut self, other: Applied) {
        self.ran.extend(other.ran);
        self.notes.extend(other.notes);
    }
}

pub struct Options<'a> {
    /// The staging root. Both the overlay's lower layer and where the
    /// changeset lands afterwards.
    pub root: &'a Path,
    /// `<build>/scripts` — where the upper, work and merged directories go.
    /// Outside the root on purpose: they are facts about the build.
    pub work: &'a Path,
    /// Resolves a script's `source`, already checked to be inside the config
    /// root and already hashed into `config_id`.
    pub config_root: &'a Path,
    /// `KILN_IMAGE`.
    pub image: &'a str,
    /// `KILN_GENERATION`.
    pub generation: u64,
    /// The image's architecture, for the foreign-architecture check.
    pub arch: &'a str,
}

/// Run every script belonging to `phase`, in lexicographic order by name.
///
/// *phase first (`packages` before `files`), then lexicographic by
/// name*. The manifest keys scripts by name in a `BTreeMap`, so the second half
/// of that is already true here and the caller supplies the first by calling
/// this twice.
pub fn run(
    phase: ScriptPhase,
    scripts: &BTreeMap<String, Script>,
    opts: &Options<'_>,
    owners: &dyn Owners,
    sandbox: &dyn Sandbox,
) -> Result<Applied> {
    let due: Vec<&Script> = scripts.values().filter(|s| s.after == phase).collect();
    let mut applied = Applied::default();
    if due.is_empty() {
        return Ok(applied);
    }
    foreign_architecture(opts.arch)?;

    for script in due {
        let ran = one(script, opts, owners, sandbox)?;
        if ran.changeset.is_empty() {
            // An empty changeset asks for a warning rather than a failure. A script whose
            // work normalization was going to do anyway — `ldconfig`, `depmod`,
            // `fc-cache` — legitimately writes nothing, and the honest thing is
            // to say so and name the alternative rather than fail a build over
            // a redundant line.
            applied.notes.push(format!(
                "script {} wrote nothing. Either it did not do what you expected, or \
                 normalization already does its job — `ldconfig`, `depmod`, `fc-cache`, \
                 `gtk-update-icon-cache`, `update-desktop-database`, `dracut` and \
                 `systemctl preset` all run without a script",
                script.name
            ));
        }
        for clobber in &ran.clobbered {
            applied.notes.push(format!(
                "script {} overwrote {}, which the package `{}` owns — an update to `{}` \
                 will not bring the script's version back",
                script.name, clobber.path, clobber.package, clobber.package
            ));
        }
        applied.ran.push(ran);
    }
    Ok(applied)
}

fn one(
    script: &Script,
    opts: &Options<'_>,
    owners: &dyn Owners,
    sandbox: &dyn Sandbox,
) -> Result<Ran> {
    let text = text_of(script, opts.config_root)?;
    let base = opts.work.join(&script.name);
    // A previous failed build's leftovers would read as this script's
    // changeset, which is worse than any error it could produce.
    let _ = std::fs::remove_dir_all(&base);

    let on_host = base.join("script");
    tree::write(&on_host, &text)?;
    // Executable because a script with a shebang is exec'd, not fed to an
    // interpreter — that is what "the file's shebang if it has one" means.
    tree::set_mode(&on_host, 0o755)?;

    let mut overlay = Overlay::mount(opts.root, &base)?;
    let spec = spec(
        &overlay.merged,
        &on_host,
        &text,
        opts.image,
        opts.generation,
    );
    let outcome = sandbox.run(&spec);

    // Unmounted before the upper layer is read, so what is read is the
    // filesystem's settled state rather than a live mount's view of it — and
    // before the error path returns, so a failed script does not leave an
    // overlay mounted under the build directory.
    overlay.unmount();

    let outcome = match outcome {
        Ok(outcome) => outcome,
        // *non-zero exit fails the build, with the script's output
        // inline*. `Sandbox::run` already carries the tail of stderr.
        Err(e) => {
            return Err(tree::shape(format!(
                "build script `{}` failed:\n{e}",
                script.name
            )))
        }
    };

    let entries = scan(&overlay.upper)?;
    refuse_boot(&script.name, &entries)?;

    let changeset = changeset(&entries);
    let digest = digest_of(&entries);
    let clobbered = clobbered(&changeset, owners);

    merge_down(&overlay.upper, opts.root, &entries)?;

    Ok(Ran {
        name: script.name.clone(),
        phase: script.after,
        changeset,
        digest,
        clobbered,
        output: [outcome.stdout.as_str(), outcome.stderr.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

/// The sandbox a script runs in, expressed as a value.
///
/// Public because the tests ask for exactly this: a test that asserts on the spec
/// rather than on a run, so that "the build phase really has
/// `Network::Disabled`" is checked without root, without bubblewrap, and
/// without a staging root.
pub fn spec(
    merged: &Path,
    script_on_host: &Path,
    text: &str,
    image: &str,
    generation: u64,
) -> SandboxSpec {
    // *the file's shebang if it has one, otherwise `bash -euo
    // pipefail`*. The defaults are not decoration — without `-e` a script that
    // fails on its second line still exits 0, and Kiln would record a changeset
    // for a script that did half its job.
    let command = if text.starts_with("#!") {
        vec![SCRIPT_IN_SANDBOX.to_string()]
    } else {
        vec![
            "bash".to_string(),
            "-euo".into(),
            "pipefail".into(),
            SCRIPT_IN_SANDBOX.into(),
        ]
    };

    // `in_root` already means no network, and that is the point rather than a
    // convenience: with `CLONE_NEWNET` and no interfaces, a script's output is
    // a pure function of its text and the staging root — the two things Kiln
    // already hashes. Everything else in this module rests on it.
    SandboxSpec::in_root(merged, command)
        .with_bind(Bind::ro(script_on_host, SCRIPT_IN_SANDBOX))
        .with_env("KILN_IMAGE", image)
        .with_env("KILN_GENERATION", generation.to_string())
}

fn text_of(script: &Script, config_root: &Path) -> Result<String> {
    if let Some(content) = &script.content {
        return Ok(content.clone());
    }
    let Some(source) = &script.source else {
        // The semantic phase rejects a script with neither; reaching here means
        // a manifest that did not come through it.
        return Err(tree::shape(format!(
            "build script `{}` has neither `source` nor `content`",
            script.name
        )));
    };
    let from = config_root.join(source);
    std::fs::read_to_string(&from).map_err(tree::io("reading the build script", &from))
}

/// *building for a foreign architecture requires `qemu-user` binfmt
/// registration. Kiln detects the mismatch and says so rather than failing
/// obscurely.*
///
/// The obscure failure this replaces is `Exec format error` from a shell that
/// is plainly present and executable — which sends the reader to look at the
/// script, the mount and the sandbox before the architecture.
fn foreign_architecture(arch: &str) -> Result<()> {
    let host = uname_machine().unwrap_or_default();
    if host.is_empty() || host == arch || arch == "any" {
        return Ok(());
    }
    let registered = Path::new("/proc/sys/fs/binfmt_misc").join(format!("qemu-{arch}"));
    if registered.exists() {
        return Ok(());
    }
    Err(tree::shape(format!(
        "build scripts run the image's own binaries under chroot, and this image is {arch} on \
         an {host} host.\nRegister qemu-user for {arch} — `qemu-user-static-binfmt` on Arch, \
         then `systemctl restart systemd-binfmt` — or build on an {arch} machine.\nWithout it \
         the first script fails with `Exec format error` from a shell that is present and \
         executable, which explains nothing"
    )))
}

fn uname_machine() -> Option<String> {
    let out = Command::new("uname").arg("-m").output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ── the overlay ─────────────────────────────────────────────────────────────

/// Mounted for as long as this is alive. Torn down on the error path too: an
/// overlay left mounted under `/var/lib/kiln` outlives the build, and the next
/// one would take its upper layer for a changeset.
struct Overlay {
    upper: PathBuf,
    merged: PathBuf,
    mounted: bool,
}

impl Overlay {
    fn mount(root: &Path, base: &Path) -> Result<Overlay> {
        let upper = base.join("upper");
        let work = base.join("work");
        let merged = base.join("merged");
        for dir in [&upper, &work, &merged] {
            tree::mkdir(dir)?;
        }

        // A comma or colon in a path would be read as an option separator, and
        // the mount would either fail obscurely or — worse — succeed against
        // the wrong directory. Every path here is Kiln's own, so this is a
        // guard on an invariant rather than a limitation on the user.
        for path in [root, upper.as_path(), work.as_path()] {
            let text = path.to_string_lossy();
            if text.contains(',') || text.contains(':') {
                return Err(tree::shape(format!(
                    "the build directory {text} contains `,` or `:`, which overlayfs mount \
                     options cannot express"
                )));
            }
        }

        let options = format!(
            "lowerdir={},upperdir={},workdir={},index=off,metacopy=off,redirect_dir=off",
            root.display(),
            upper.display(),
            work.display()
        );
        let out = Command::new("mount")
            .args(["-t", "overlay", "kiln-script", "-o", &options])
            .arg(&merged)
            .output()
            .map_err(tree::io("running mount for", &merged))?;
        if !out.status.success() {
            return Err(tree::shape(format!(
                "could not mount the overlay a build script runs against: {}\n\
                 lowerdir={} upperdir={}",
                String::from_utf8_lossy(&out.stderr).trim(),
                root.display(),
                upper.display()
            )));
        }
        Ok(Overlay {
            upper,
            merged,
            mounted: true,
        })
    }

    fn unmount(&mut self) {
        if !self.mounted {
            return;
        }
        self.mounted = false;
        // Lazy, so a descriptor the script left open cannot keep an overlay
        // mounted under the build directory forever.
        let _ = Command::new("umount")
            .args(["-l"])
            .arg(&self.merged)
            .output();
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.unmount();
    }
}

// ── reading the upper layer ─────────────────────────────────────────────────

/// One thing the upper layer says happened. Relative paths throughout, so the
/// same value addresses both the upper layer and the staging root.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    File {
        rel: String,
        mode: u32,
        uid: u32,
        gid: u32,
        bytes: u64,
        digest: Hash,
    },
    Link {
        rel: String,
        target: String,
    },
    Dir {
        rel: String,
        mode: u32,
        uid: u32,
        gid: u32,
        /// The lower directory of the same name is gone, not merged with:
        /// `trusted.overlay.opaque`.
        opaque: bool,
    },
    /// A character device with device number 0 — overlayfs's whiteout.
    Deleted {
        rel: String,
    },
}

impl Change {
    fn rel(&self) -> &str {
        match self {
            Change::File { rel, .. }
            | Change::Link { rel, .. }
            | Change::Dir { rel, .. }
            | Change::Deleted { rel } => rel,
        }
    }
}

/// Walk the upper layer. Sorted by path, so the changeset — and the digest over
/// it — does not depend on the order the kernel happened to return entries in.
fn scan(upper: &Path) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    walk(upper, upper, &mut out)?;
    out.sort_by(|a, b| a.rel().cmp(b.rel()));
    Ok(out)
}

fn walk(base: &Path, at: &Path, out: &mut Vec<Change>) -> Result<()> {
    for path in tree::entries(at)? {
        let md = path
            .symlink_metadata()
            .map_err(tree::io("reading the script changeset entry", &path))?;
        let rel = path
            .strip_prefix(base)
            .expect("walked from base")
            .to_string_lossy()
            .into_owned();
        let kind = md.file_type();

        if kind.is_char_device() && md.rdev() == 0 {
            out.push(Change::Deleted { rel });
            continue;
        }
        if kind.is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(tree::io("reading the link", &path))?
                .to_string_lossy()
                .into_owned();
            out.push(Change::Link { rel, target });
            continue;
        }
        if kind.is_dir() {
            out.push(Change::Dir {
                rel,
                mode: md.permissions().mode() & 0o7777,
                uid: md.uid(),
                gid: md.gid(),
                opaque: is_opaque(&path),
            });
            walk(base, &path, out)?;
            continue;
        }
        let bytes = std::fs::read(&path).map_err(tree::io("reading", &path))?;
        out.push(Change::File {
            rel,
            mode: md.permissions().mode() & 0o7777,
            uid: md.uid(),
            gid: md.gid(),
            bytes: md.len(),
            digest: Hash::of(&bytes),
        });
    }
    Ok(())
}

/// `trusted.overlay.opaque` — set when a directory was removed and recreated,
/// meaning the lower one is gone rather than merged with.
///
/// A `trusted.*` xattr needs `CAP_SYS_ADMIN` to read, which assembly has.
/// A failure to read is reported as "not opaque", which is the
/// conservative answer: it merges rather than deletes.
fn is_opaque(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let name = c"trusted.overlay.opaque";
    let mut buf = [0u8; 4];
    // SAFETY: both pointers are valid for the duration of the call, and the
    // length passed is the buffer's own.
    let n = unsafe {
        libc::lgetxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    n > 0 && buf[0] == b'y'
}

/// *writes to `/var` are fine — the drain runs afterwards. Writes to
/// `/boot` are rejected, as everywhere else.*
fn refuse_boot(name: &str, entries: &[Change]) -> Result<()> {
    let problems: Vec<Refusal> = entries
        .iter()
        .filter(|c| c.rel() == "boot" || c.rel().starts_with("boot/"))
        // A directory entry for `boot` itself is not a write *into* it: the
        // skeleton already made it, and overlayfs copies a parent up whenever
        // anything below it changes.
        .filter(|c| !matches!(c, Change::Dir { rel, .. } if rel == "boot"))
        .map(|c| Refusal {
            target: format!("/{}", c.rel()),
            why: format!("build script `{name}` wrote under /boot"),
            hint: Some(
                "OSTree owns /boot; the kernel and initramfs are placed by Kiln at \
                 /usr/lib/modules/$kver, and a bootloader entry is written at deploy"
                    .into(),
            ),
        })
        .collect();
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Refused {
        noun: ("build script write", "build script writes"),
        problems,
    })
}

fn changeset(entries: &[Change]) -> Changeset {
    let mut out = Changeset::default();
    for change in entries {
        match change {
            Change::File { rel, bytes, .. } => out.wrote.push(Written {
                path: format!("/{rel}"),
                bytes: *bytes,
            }),
            Change::Link { rel, .. } => out.wrote.push(Written {
                path: format!("/{rel}"),
                bytes: 0,
            }),
            Change::Deleted { rel } => out.deleted.push(format!("/{rel}")),
            Change::Dir { rel, opaque, .. } => {
                if *opaque {
                    out.deleted.push(format!("/{rel}"));
                }
            }
        }
    }
    out
}

/// The changeset's identity: what the whole determinism audit runs on.
///
/// What goes in is what would make the image differ: the path, what kind of
/// thing it is, its content, and the mode and ownership that a package's setuid
/// bit and a service account's directory both depend on. What stays out is
/// timestamps and sizes — the first is not reproducible and the second is
/// implied by the content hash.
fn digest_of(entries: &[Change]) -> Hash {
    let mut encoded = String::new();
    for change in entries {
        match change {
            Change::File {
                rel,
                mode,
                uid,
                gid,
                digest,
                ..
            } => encoded.push_str(&format!("+f {mode:o} {uid} {gid} {digest} {rel}\n")),
            Change::Link { rel, target } => encoded.push_str(&format!("+l {target} {rel}\n")),
            Change::Dir {
                rel,
                mode,
                uid,
                gid,
                opaque,
            } => encoded.push_str(&format!(
                "+d {mode:o} {uid} {gid} {} {rel}\n",
                if *opaque { "opaque" } else { "merge" }
            )),
            Change::Deleted { rel } => encoded.push_str(&format!("- {rel}\n")),
        }
    }
    Hash::of(encoded.as_bytes())
}

/// Reported, not refused.
///
/// A `[[file]]` writing over a package's file is refused, because there is
/// almost always a drop-in that does the job and survives an update. A script
/// is the case where there is not: `locale-gen` rewrites glibc's own
/// `locale.gen`, and refusing that would make the escape hatch useless for the
/// example opens with. So the rule here is the weaker half of the same
/// idea — scripts cannot *silently* clobber package content.
fn clobbered(changeset: &Changeset, owners: &dyn Owners) -> Vec<Clobber> {
    changeset
        .wrote
        .iter()
        .filter_map(|w| {
            owners.owner_of(&w.path).map(|package| Clobber {
                path: w.path.clone(),
                package,
            })
        })
        .collect()
}

// ── merging the changeset down ──────────────────────────────────────────────

/// Apply the upper layer to the staging root.
///
/// Deletions first, then a single `cp -a` of what is left. `cp` rather than a
/// hand-rolled copy because it preserves what an image depends on and a naive
/// walk drops: ownership, setuid and setgid bits, file capabilities and other
/// xattrs, hard links, and symlinks as symlinks. Getting any one of those wrong
/// produces an image that is subtly, silently not what the packages declared.
fn merge_down(upper: &Path, root: &Path, entries: &[Change]) -> Result<()> {
    // Shallowest first, so removing a directory takes its whiteouts with it and
    // the deeper entries are simply already gone.
    let mut removals: Vec<&str> = entries
        .iter()
        .filter_map(|c| match c {
            Change::Deleted { rel } => Some(rel.as_str()),
            Change::Dir {
                rel, opaque: true, ..
            } => Some(rel.as_str()),
            _ => None,
        })
        .collect();
    removals.sort_by_key(|rel| rel.matches('/').count());

    for rel in removals {
        tree::remove(&root.join(rel))?;
    }

    // The whiteout nodes themselves are markers, not content. Left in place,
    // `cp -a` would faithfully copy a character device into the image at the
    // path the script deleted.
    for change in entries {
        if let Change::Deleted { rel } = change {
            tree::remove(&upper.join(rel))?;
        }
    }

    if entries.iter().all(|c| matches!(c, Change::Deleted { .. })) {
        return Ok(());
    }

    // `upper/.` rather than `upper`: the contents are merged into the root
    // rather than landing in a directory named `upper` inside it.
    let out = Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", upper.display()))
        .arg(root)
        .output()
        .map_err(tree::io("running cp for", upper))?;
    if !out.status.success() {
        return Err(tree::shape(format!(
            "merging a build script's changeset into the staging root failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(name: &str, content: &str) -> Script {
        Script {
            name: name.into(),
            source: None,
            content: Some(content.into()),
            after: ScriptPhase::Files,
        }
    }

    /// The sandbox's one non-negotiable rule, and the test suite's instruction to assert on the
    /// spec rather than on a run. Everything else in the module — that a
    /// changeset is a pure function of hashed inputs, that `kiln rebuild` can
    /// audit determinism — is false the moment this stops holding.
    #[test]
    fn a_script_has_no_network_and_never_can() {
        let spec = spec(
            Path::new("/merged"),
            Path::new("/work/script"),
            "echo hi\n",
            "workstation",
            7,
        );
        assert_eq!(spec.network, kiln_sandbox::Network::Disabled);
    }

    /// *the file's shebang if it has one, otherwise `bash -euo
    /// pipefail`*. Without `-e` a script that fails on its second line exits 0
    /// and Kiln records a changeset for a job half done.
    #[test]
    fn a_script_without_a_shebang_gets_bash_with_the_strict_flags() {
        let spec = spec(
            Path::new("/merged"),
            Path::new("/work/script"),
            "locale-gen\n",
            "workstation",
            1,
        );
        assert_eq!(
            spec.command,
            vec!["bash", "-euo", "pipefail", SCRIPT_IN_SANDBOX]
        );
    }

    #[test]
    fn a_script_with_a_shebang_is_executed_by_it() {
        let spec = spec(
            Path::new("/merged"),
            Path::new("/work/script"),
            "#!/usr/bin/python3\nprint('hi')\n",
            "workstation",
            1,
        );
        assert_eq!(spec.command, vec![SCRIPT_IN_SANDBOX]);
    }

    /// The script's environment table. `SOURCE_DATE_EPOCH` comes from the sandbox's
    /// own default and is checked here too, because a script that can tell what
    /// time it is can write a timestamp into the image.
    #[test]
    fn the_environment_is_cleared_and_then_told_which_image_this_is() {
        let spec = spec(
            Path::new("/merged"),
            Path::new("/work/script"),
            "true\n",
            "workstation",
            42,
        );
        assert_eq!(
            spec.env.get("KILN_IMAGE").map(String::as_str),
            Some("workstation")
        );
        assert_eq!(
            spec.env.get("KILN_GENERATION").map(String::as_str),
            Some("42")
        );
        assert_eq!(
            spec.env.get("SOURCE_DATE_EPOCH").map(String::as_str),
            Some("0")
        );
        assert_eq!(spec.env.get("LANG").map(String::as_str), Some("C.UTF-8"));
    }

    /// The script's text reaches the sandbox as a read-only bind, not as a file
    /// written into the tree being assembled — which would have to be cleaned
    /// up again before the commit, and would be visible to the script itself as
    /// image content.
    #[test]
    fn the_script_is_bound_in_rather_than_written_into_the_image() {
        let spec = spec(
            Path::new("/merged"),
            Path::new("/work/20-locale/script"),
            "true\n",
            "workstation",
            1,
        );
        let bind = spec
            .binds
            .iter()
            .find(|b| b.target == Path::new(SCRIPT_IN_SANDBOX))
            .expect("the script is bound into the sandbox");
        assert_eq!(bind.mode, kiln_sandbox::BindMode::ReadOnly);
        assert_eq!(bind.source, Path::new("/work/20-locale/script"));
    }

    /// The digest is what `kiln rebuild` compares, so it has to move when the
    /// image would differ and stay still when it would not.
    #[test]
    fn the_changeset_digest_tracks_content_mode_and_ownership() {
        let file = |mode, uid, body: &[u8]| {
            vec![Change::File {
                rel: "usr/bin/tool".into(),
                mode,
                uid,
                gid: 0,
                // Deliberately constant while the content varies: the size is
                // implied by the digest, and encoding it as well would only
                // hide a same-length change rather than catch one.
                bytes: 3,
                digest: Hash::of(body),
            }]
        };
        let original = digest_of(&file(0o755, 0, b"abc"));

        assert_ne!(
            digest_of(&file(0o755, 0, b"abd")),
            original,
            "content must move it"
        );
        assert_ne!(
            digest_of(&file(0o4755, 0, b"abc")),
            original,
            "a setuid bit must move it"
        );
        assert_ne!(
            digest_of(&file(0o755, 1000, b"abc")),
            original,
            "ownership must move it"
        );
        assert_eq!(
            digest_of(&file(0o755, 0, b"abc")),
            original,
            "and nothing else may"
        );
    }

    /// A deletion is a change to the image, so it has to reach the digest —
    /// otherwise a script that only removes files looks like a script that did
    /// nothing, twice in a row, and the determinism audit says they agreed.
    #[test]
    fn a_deletion_reaches_the_digest() {
        let nothing = digest_of(&[]);
        let deleted = digest_of(&[Change::Deleted {
            rel: "usr/share/doc/big".into(),
        }]);
        assert_ne!(deleted, nothing);
    }

    /// An opaque directory is a deletion followed by a creation. Encoding it
    /// the same as an ordinary directory would make "removed and recreated
    /// empty" and "left alone" hash alike.
    #[test]
    fn an_opaque_directory_is_not_a_merged_one() {
        let merged = digest_of(&[Change::Dir {
            rel: "usr/share/fonts".into(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            opaque: false,
        }]);
        let opaque = digest_of(&[Change::Dir {
            rel: "usr/share/fonts".into(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            opaque: true,
        }]);
        assert_ne!(merged, opaque);
    }

    #[test]
    fn an_opaque_directory_counts_as_a_deletion_in_the_changeset() {
        let set = changeset(&[Change::Dir {
            rel: "usr/share/fonts".into(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            opaque: true,
        }]);
        assert_eq!(set.deleted, vec!["/usr/share/fonts"]);
        assert!(set.wrote.is_empty(), "a directory is not a written path");
    }

    /// Directories are not listed as writes: a script that writes one file into
    /// a new tree made several directories on the way, and `kiln build -v`
    /// saying so buries the one thing it actually did.
    #[test]
    fn the_changeset_lists_files_and_links_not_the_directories_above_them() {
        let set = changeset(&[
            Change::Dir {
                rel: "usr/lib/locale".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
                opaque: false,
            },
            Change::File {
                rel: "usr/lib/locale/locale-archive".into(),
                mode: 0o644,
                uid: 0,
                gid: 0,
                bytes: 19_293_696,
                digest: Hash::of(b""),
            },
            Change::Link {
                rel: "usr/lib/locale/C".into(),
                target: "C.utf8".into(),
            },
        ]);
        assert_eq!(
            set.wrote
                .iter()
                .map(|w| w.path.as_str())
                .collect::<Vec<_>>(),
            ["/usr/lib/locale/locale-archive", "/usr/lib/locale/C"]
        );
    }

    /// writes to `/boot` are rejected. The directory entry for `boot`
    /// itself is not one — overlayfs copies a parent up whenever anything
    /// beneath it changes, so refusing on that would refuse every script.
    #[test]
    fn writing_under_boot_is_refused_but_the_directory_itself_is_not_a_write() {
        assert!(refuse_boot(
            "20-x",
            &[Change::Dir {
                rel: "boot".into(),
                mode: 0o755,
                uid: 0,
                gid: 0,
                opaque: false,
            }]
        )
        .is_ok());

        let err = refuse_boot(
            "20-x",
            &[Change::File {
                rel: "boot/vmlinuz-linux".into(),
                mode: 0o644,
                uid: 0,
                gid: 0,
                bytes: 12,
                digest: Hash::of(b""),
            }],
        )
        .expect_err("a script must not write a kernel into /boot");
        let text = err.to_string();
        assert!(text.contains("/boot/vmlinuz-linux"), "got: {text}");
        assert!(text.contains("20-x"), "got: {text}");
    }

    /// `/var` is explicitly fine: the drain runs afterwards and turns
    /// whatever landed there into a factory default.
    #[test]
    fn writing_under_var_is_not_refused() {
        assert!(refuse_boot(
            "20-x",
            &[Change::File {
                rel: "var/lib/thing/cache".into(),
                mode: 0o644,
                uid: 0,
                gid: 0,
                bytes: 1,
                digest: Hash::of(b""),
            }]
        )
        .is_ok());
    }

    struct Owned(&'static str);
    impl Owners for Owned {
        fn owner_of(&self, path: &str) -> Option<String> {
            (path == self.0).then(|| "glibc".to_string())
        }
    }

    /// Reported with the owning package, not refused — the escape hatch's own example
    /// rewrites glibc's `locale.gen`, and a refusal would make the escape hatch
    /// useless for the case it was written for.
    #[test]
    fn overwriting_a_package_file_is_named_with_its_package() {
        let set = Changeset {
            wrote: vec![
                Written {
                    path: "/etc/locale.gen".into(),
                    bytes: 40,
                },
                Written {
                    path: "/usr/lib/locale/locale-archive".into(),
                    bytes: 1,
                },
            ],
            deleted: Vec::new(),
        };
        let found = clobbered(&set, &Owned("/etc/locale.gen"));
        assert_eq!(
            found,
            vec![Clobber {
                path: "/etc/locale.gen".into(),
                package: "glibc".into(),
            }]
        );
    }

    #[test]
    fn a_script_with_neither_source_nor_content_says_so() {
        let script = Script {
            name: "20-x".into(),
            source: None,
            content: None,
            after: ScriptPhase::Files,
        };
        let err = text_of(&script, Path::new("/nowhere")).expect_err("nothing to run");
        assert!(err.to_string().contains("20-x"));
    }

    /// Only the phase asked for. The two slots exist because a script that
    /// needs `[[file]]` content to be in place and one that must run before it
    /// are different jobs (steps 5 and 8).
    #[test]
    fn only_the_scripts_belonging_to_the_phase_are_due() {
        let mut scripts = BTreeMap::new();
        scripts.insert("10-early".to_string(), {
            let mut s = script("10-early", "true\n");
            s.after = ScriptPhase::Packages;
            s
        });
        scripts.insert("20-late".to_string(), script("20-late", "true\n"));

        let due: Vec<&str> = scripts
            .values()
            .filter(|s| s.after == ScriptPhase::Packages)
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(due, ["10-early"]);
    }
}
