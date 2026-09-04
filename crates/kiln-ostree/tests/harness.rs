//! Shared setup. Every test binary compiles its own copy, so anything one
//! binary does not call looks dead to that binary.
#![allow(dead_code)]

use kiln_manifest::Manifest;
use kiln_resolve::{BuildPlan, ImageRef, Provenance, ResolvedInput, UidMap};
use ostree::gio;
use ostree::prelude::*;
use std::path::{Path, PathBuf};

pub fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

/// A scratch sysroot, wiped first.
///
/// The wipe is not `remove_dir_all().ok()`. libostree sets the **immutable**
/// attribute on deployment roots, so the removal fails, and swallowing that
/// leaves the previous run's deployments in place — which showed up as
/// generation numbers that kept climbing and assertions that were right about a
/// tree nobody had built. Clearing the attribute first, and then insisting the
/// directory is really gone, is the difference between a test that is
/// reproducible and one that passes on a clean checkout only.
pub fn scratch(name: &str) -> PathBuf {
    let dir = workspace().join("target/test-roots").join(name);
    if dir.exists() {
        // Dangling symlinks in the tree make chattr complain; the flags that
        // matter are on the deployment directories.
        let _ = std::process::Command::new("chattr")
            .args(["-R", "-i"])
            .arg(&dir)
            .stderr(std::process::Stdio::null())
            .status();
        std::fs::remove_dir_all(&dir)
            .unwrap_or_else(|e| panic!("could not wipe {}: {e}", dir.display()));
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn effective_uid() -> u32 {
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
        .unwrap_or(u32::MAX)
}

pub fn require_root(what: &str) -> bool {
    let root = effective_uid() == 0;
    if !root {
        eprintln!("skipped: {what} needs root");
    }
    root
}

/// A tree shaped like a normalized image: `/usr` with content, `/var` and
/// `/boot` present and empty, the top-level symlinks.
pub fn image_tree(at: &Path) -> PathBuf {
    std::fs::create_dir_all(at.join("usr/lib")).unwrap();
    std::fs::create_dir_all(at.join("usr/etc")).unwrap();
    std::fs::create_dir_all(at.join("var")).unwrap();
    std::fs::create_dir_all(at.join("boot")).unwrap();
    std::fs::create_dir_all(at.join("sysroot")).unwrap();
    // `ID` and `PRETTY_NAME` are not decoration: libostree titles the BLS entry
    // from them and refuses to deploy without one — "Installing kernel: No
    // PRETTY_NAME or ID in /etc/os-release".
    std::fs::write(
        at.join("usr/lib/os-release"),
        "NAME=Kiln\nID=kiln\nPRETTY_NAME=\"Kiln fixture\"\n",
    )
    .unwrap();

    // libostree refuses to deploy a tree with no kernel — "Failed to find
    // kernel in /usr/lib/modules, /usr/lib/ostree-boot or /boot" — and it looks
    // in exactly the place puts one. The two halves of the design agree,
    // and this is where that stops being a coincidence.
    let moddir = at.join("usr/lib/modules/6.19.0-fixture");
    std::fs::create_dir_all(&moddir).unwrap();
    std::fs::write(moddir.join("vmlinuz"), "not really a kernel\n").unwrap();
    std::fs::write(moddir.join("initramfs.img"), "not really an initramfs\n").unwrap();
    std::fs::write(at.join("usr/etc/passwd"), "root:x:0:0::/root:/bin/sh\n").unwrap();
    for (link, target) in [("bin", "usr/bin"), ("ostree", "sysroot/ostree")] {
        let _ = std::os::unix::fs::symlink(target, at.join(link));
    }
    at.to_path_buf()
}

pub fn plan() -> BuildPlan {
    let mut plan = BuildPlan {
        config_id: kiln_manifest::Hash("b3:fixture".into()),
        image: ImageRef {
            name: "fixture".into(),
            arch: "x86_64".into(),
        },
        inputs: vec![ResolvedInput::RepoPackage {
            name: "fixture-base".into(),
            evr: "1.0-1".into(),
            filename: "fixture-base-1.0-1-any.pkg.tar.zst".into(),
            sha256: "abcd".into(),
            repo: "fixture".into(),
            explicit: true,
        }],
        volatile: Vec::new(),
        uid_map: UidMap::new(),
        provenance: Provenance {
            resolved_at: "2026-09-01T00:00:00Z".into(),
            snapshot: "2026-09-01".into(),
            repos: vec![("fixture".into(), vec!["file:///fixture".into()])],
            libalpm: "16.0.1".into(),
        },
    };
    plan.canonicalize();
    plan
}

/// Every path in a commit, absolute, sorted.
pub fn list_commit(repo: &ostree::Repo, checksum: &str) -> Vec<String> {
    let (root, _) = repo.read_commit(checksum, gio::Cancellable::NONE).unwrap();
    let mut out = Vec::new();
    walk(&root, "", &mut out);
    out.sort();
    out
}

fn walk(dir: &gio::File, prefix: &str, out: &mut Vec<String>) {
    let Ok(enumerator) = dir.enumerate_children(
        "standard::name,standard::type",
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        gio::Cancellable::NONE,
    ) else {
        return;
    };
    for info in enumerator.flatten() {
        let name = info.name();
        let path = format!("{prefix}/{}", name.to_string_lossy());
        let child = dir.child(name);
        if info.file_type() == gio::FileType::Directory {
            out.push(path.clone());
            walk(&child, &path, out);
        } else {
            out.push(path);
        }
    }
}

/// The manifest the fixture plan was built from. Shared, because every commit
/// now carries one (step 11) and a commit test that invented its own would
/// be asserting against a manifest no plan here describes.
pub fn manifest() -> Manifest {
    let mut manifest = Manifest::default();
    manifest.image.name = "fixture".into();
    manifest.image.arch = "x86_64".into();
    manifest.kernel.cmdline.insert("quiet".into());
    manifest.kernel.cmdline.insert("rw".into());
    manifest
}
