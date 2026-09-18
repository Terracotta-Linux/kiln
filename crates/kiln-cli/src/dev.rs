//! `kiln unlock` and `kiln live` — dev/test scratch space on the booted
//! system, never a second way to change a deployment.
//!
//! Both commands are **temporary and dev/test only**. Neither touches a
//! deployment — not the booted one, not the one that boots next — and neither
//! writes a commit, so nothing here shows up in `kiln list`, `plan_id`, or the
//! build record. Every change either lives only for the rest of this boot
//! (`kiln unlock`) or is a live-applied preview on top of that (`kiln live`),
//! and both are discarded at the next reboot — even a reboot back into the
//! very generation that was unlocked or previewed. See
//! `kiln_ostree::unlock` for why only the transient unlock state exists here.
//!
//! `kiln live <gen>` never touches the kernel, the initramfs, the kernel
//! command line, or the bootloader — those need a real reboot, and this
//! command says so rather than silently applying a partial version of them.
//! It syncs `/usr` and `/etc` only: `/usr` by making the live root match the
//! target generation exactly, `/etc` with the same 3-way merge OSTree itself
//! would do at a real deploy — a local edit on the live `/etc` is kept, not
//! clobbered, and the reason `kiln_ostree::drift` is reused here rather than
//! reimplemented.

use crate::paths;
use kiln_diag::ExitCode;
use kiln_ostree::drift::{self, Change};
use kiln_ostree::{Error as OError, Sysroot};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

fn warning() -> String {
    format!(
        "{} dev/test only — never run this on a system you depend on.\n        Temporary: \
         this touches no deployment, writes no commit, and every\n        change is \
         discarded on reboot — even a reboot back into the same\n        generation.\n",
        crate::color::warning(crate::color::Stream::Out)
    )
}

pub fn unlock(sysroot: Option<&Path>) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(exit) => return exit,
    };
    if !sysroot.is_booted() {
        return not_booted();
    }

    print!("{}", warning());
    match sysroot.unlock() {
        Ok(()) => {
            println!("unlocked  /usr is writable for the rest of this boot");
            ExitCode::Ok
        }
        Err(e) => code(&e),
    }
}

pub fn live(sysroot: Option<&Path>, generation: u64) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(exit) => return exit,
    };
    if !sysroot.is_booted() {
        return not_booted();
    }

    let generations = match sysroot.generations() {
        Ok(g) => g,
        Err(e) => return code(&e),
    };
    let Some(booted) = generations.iter().find(|g| g.booted) else {
        eprintln!(
            "{} nothing is booted from Kiln on this machine",
            crate::color::error()
        );
        return ExitCode::System;
    };
    let Some(target) = generations.iter().find(|g| g.number == generation) else {
        return code(&OError::NoSuchGeneration {
            wanted: generation,
            available: generations.iter().map(|g| g.number).collect(),
        });
    };
    if target.number == booted.number {
        println!("generation {generation} is already booted; nothing to preview.");
        return ExitCode::Ok;
    }

    let booted_root = match sysroot.deployment_root(booted.number) {
        Ok(p) => p,
        Err(e) => return code(&e),
    };
    let target_root = match sysroot.deployment_root(target.number) {
        Ok(p) => p,
        Err(e) => return code(&e),
    };

    let boot_only = reboot_only_changes(&sysroot, booted, target, &booted_root, &target_root);

    print!("{}", warning());
    println!(
        "previewing generation {} live over generation {} — /usr and /etc only\n",
        target.number, booted.number
    );

    if let Err(e) = sysroot.unlock() {
        return code(&e);
    }

    let live_root = Path::new("/");
    if let Err(e) = sync_usr(&target_root, live_root) {
        eprintln!("{} syncing /usr: {e}", crate::color::error());
        return ExitCode::System;
    }
    let etc_report = match sync_etc(&booted_root, &target_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} syncing /etc: {e}", crate::color::error());
            return ExitCode::System;
        }
    };

    println!("applied   /usr, and /etc where you have no local edits");
    if !etc_report.kept_local.is_empty() {
        println!("\nkept — a local /etc change is at or under this path:");
        for path in &etc_report.kept_local {
            println!("  {path}");
        }
    }
    if !boot_only.is_empty() {
        println!("\nskipped — needs a real reboot, not applied live:");
        for reason in &boot_only {
            println!("  {reason}");
        }
    }
    println!(
        "\nchanged services may still be running the old code; restart them by hand, or reboot \
         into generation {} for real.",
        target.number
    );
    ExitCode::Ok
}

/// What `kiln live` cannot apply without a real reboot: a different kernel, a
/// different initramfs (both named by the `/usr/lib/modules/<version>`
/// directory the image places them under — see `kiln-image/src/kernel.rs`),
/// or a different `kernel.cmdline`. All three are read from the deployments'
/// own trees and commit metadata, not from `/etc/kiln`, so this works however
/// old the target generation is.
fn reboot_only_changes(
    sysroot: &Sysroot,
    booted: &kiln_ostree::Generation,
    target: &kiln_ostree::Generation,
    booted_root: &Path,
    target_root: &Path,
) -> Vec<String> {
    let mut out = Vec::new();
    let (bv, tv) = (kernel_version(booted_root), kernel_version(target_root));
    if bv != tv {
        out.push(format!(
            "kernel {} \u{2192} {}",
            bv.as_deref().unwrap_or("(none)"),
            tv.as_deref().unwrap_or("(none)")
        ));
    }

    let bm = kiln_ostree::commit::read_metadata(&sysroot.repo(), &booted.checksum)
        .ok()
        .and_then(|m| m.manifest);
    let tm = kiln_ostree::commit::read_metadata(&sysroot.repo(), &target.checksum)
        .ok()
        .and_then(|m| m.manifest);
    if let (Some(bm), Some(tm)) = (bm, tm) {
        if bm.kernel.cmdline != tm.kernel.cmdline {
            out.push("kernel.cmdline".to_string());
        }
    }
    out
}

fn kernel_version(deployment_root: &Path) -> Option<String> {
    let dir = deployment_root.join("usr/lib/modules");
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
}

/// Make live `/usr` match `target_root/usr`, except `usr/lib/modules`: kernel
/// and module content never applies live, whether or not the version changed
/// — a running kernel does not want its own modules rewritten underneath it.
fn sync_usr(target_root: &Path, live_root: &Path) -> io::Result<()> {
    let target_usr = target_root.join("usr");
    let live_usr = live_root.join("usr");
    let changes = drift::compare(&live_usr, &target_usr, "/usr")
        .map_err(|e| io::Error::other(e.to_string()))?;
    for change in changes {
        if change.path() == "/usr/lib/modules" || change.path().starts_with("/usr/lib/modules/") {
            continue;
        }
        apply(&change, &target_usr, &live_usr)?;
    }
    Ok(())
}

/// A 3-way merge: `booted_root/usr/etc` is the base both generations agree on
/// having started from, `/etc` (via `booted_root/etc`, the same directory —
/// see the module doc) is what the machine actually has, and
/// `target_root/usr/etc` is what generation `target` would ship. A path
/// changed on the live system is left alone; everything else takes the
/// target's version, the same rule libostree's own merge applies at a real
/// deploy.
struct EtcReport {
    kept_local: Vec<String>,
}

fn sync_etc(booted_root: &Path, target_root: &Path) -> io::Result<EtcReport> {
    let base = booted_root.join("usr/etc");
    let live = booted_root.join("etc");
    let theirs = target_root.join("usr/etc");

    let local = drift::scan(booted_root).map_err(|e| io::Error::other(e.to_string()))?;
    let local_paths: Vec<&str> = local.iter().map(Change::path).collect();

    let wanted =
        drift::compare(&base, &theirs, "/etc").map_err(|e| io::Error::other(e.to_string()))?;

    let mut kept_local = Vec::new();
    for change in wanted {
        // Not just paths the user *shadowed* (modified or deleted something
        // shipped) — a target generation that drops a shipped directory the
        // user has since added local files into must not carry that removal
        // out here: `Removed("/etc/NetworkManager")` next to a local, never
        // shipped `Added("/etc/NetworkManager/system-connections/…")` would
        // otherwise delete files that were never image content in the first
        // place. Any overlap between the two trees, in either direction, is
        // reason enough to leave the path alone and say so.
        if local_paths.iter().any(|p| overlaps(change.path(), p)) {
            kept_local.push(change.path().to_string());
            continue;
        }
        apply(&change, &theirs, &live)?;
    }
    Ok(EtcReport { kept_local })
}

/// Do `a` and `b` name the same path, or is one an ancestor of the other?
fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
}

/// `change.path()` is rooted at `/usr` or `/etc`; `src_root`/`dst_root` are the
/// matching `usr` or `etc` directories the change was computed from.
fn apply(change: &Change, src_root: &Path, dst_root: &Path) -> io::Result<()> {
    let rel = change
        .path()
        .trim_start_matches('/')
        .split_once('/')
        .map_or("", |(_, rest)| rest);
    let dst = dst_root.join(rel);
    match change {
        Change::Removed { .. } => remove_any(&dst),
        // A directory that exists on both sides and differs only in
        // permissions or ownership is fixed in place. `drift::walk` always
        // recurses into a common directory regardless of whether it also
        // records a metadata difference for the directory itself, so the
        // *content* differences underneath already arrive as their own
        // `Change`s — deleting the directory here to "fix" a chmod would mean
        // `fs::remove_dir_all` on a live, possibly huge, in-use tree, with a
        // real window where every file under it is briefly gone.
        Change::Modified {
            how: drift::How::Mode | drift::How::Owner,
            ..
        } => {
            let src = src_root.join(rel);
            let meta = fs::symlink_metadata(&src).map_err(|e| at(e, "reading", &src))?;
            fs::set_permissions(&dst, meta.permissions()).map_err(|e| at(e, "chmod'ing", &dst))?;
            std::os::unix::fs::lchown(&dst, Some(meta.uid()), Some(meta.gid()))
                .map_err(|e| at(e, "chown'ing", &dst))
        }
        Change::Modified { .. } | Change::Added { .. } => {
            let src = src_root.join(rel);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent).map_err(|e| at(e, "creating", parent))?;
            }
            remove_any(&dst)?;
            copy_any(&src, &dst)
        }
    }
}

/// Attach the path an `fs` call was acting on to its error, so a failure names
/// the file rather than surfacing a bare `No such file or directory (os error
/// 2)` with no way to tell which of the many paths `apply` touches it was.
fn at(e: io::Error, doing: &str, path: &Path) -> io::Error {
    io::Error::other(format!("{doing} {}: {e}", path.display()))
}

fn copy_any(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src).map_err(|e| at(e, "reading", src))?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src).map_err(|e| at(e, "reading the symlink", src))?;
        std::os::unix::fs::symlink(target, dst).map_err(|e| at(e, "creating the symlink", dst))?;
    } else if meta.is_dir() {
        fs::create_dir_all(dst).map_err(|e| at(e, "creating", dst))?;
        for entry in fs::read_dir(src).map_err(|e| at(e, "reading", src))? {
            let entry = entry.map_err(|e| at(e, "reading an entry of", src))?;
            copy_any(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions()).map_err(|e| at(e, "chmod'ing", dst))?;
    } else {
        fs::copy(src, dst).map_err(|e| at(e, "copying", src))?;
        fs::set_permissions(dst, meta.permissions()).map_err(|e| at(e, "chmod'ing", dst))?;
    }
    // `lchown`, not `chown`: for the symlink branch above, `dst` is itself the
    // symlink, and `chown` follows it — to a target that, mid-sync, may not
    // exist on the live system yet, or may exist but belong to a file this
    // change has no business touching the ownership of.
    std::os::unix::fs::lchown(dst, Some(meta.uid()), Some(meta.gid()))
        .map_err(|e| at(e, "chown'ing", dst))
}

fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path).map_err(|e| at(e, "removing", path)),
        Ok(_) => fs::remove_file(path).map_err(|e| at(e, "removing", path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(at(e, "reading", path)),
    }
}

fn not_booted() -> ExitCode {
    eprintln!("{} {}", crate::color::error(), OError::NotBooted);
    ExitCode::System
}

fn open(root: &Path) -> Result<Sysroot, ExitCode> {
    Sysroot::open(root).map_err(|e| {
        eprintln!("{} {e}", crate::color::error());
        if !paths::is_initialized(root) {
            eprintln!(
                "\n`kiln sysroot init --sysroot {}` creates the layout this needs.",
                root.display()
            );
            if paths::repo(root).exists() {
                eprintln!(
                    "\n{} has a Kiln repository but no stateroot: it was built into before it \
                     was\ninitialized. Nothing is lost — initialize it and the generations \
                     already committed\nthere are deployable, `kiln list` will show them.",
                    root.display()
                );
            }
        }
        ExitCode::System
    })
}

fn code(e: &kiln_ostree::Error) -> ExitCode {
    eprintln!("{} {e}", crate::color::error());
    ExitCode::System
}
