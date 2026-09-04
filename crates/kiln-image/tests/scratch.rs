//! Building synthetic staging roots to normalize.
//!
//! Shared by every test binary in this crate, each of which compiles its own
//! copy — so anything one binary does not call looks dead to that binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub fn root(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/test-roots")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn dir(root: &Path, rel: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let at = root.join(rel);
    std::fs::create_dir_all(&at).unwrap();
    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
}

pub fn file(root: &Path, rel: &str, body: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let at = root.join(rel);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    // Removed first: a test that replaces a file it wrote read-only — the way
    // /etc/machine-id ships — would otherwise fail on the second write.
    std::fs::remove_file(&at).ok();
    std::fs::write(&at, body).unwrap();
    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
}

pub fn link(root: &Path, rel: &str, target: &str) {
    let at = root.join(rel);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, at).unwrap();
}

/// A `passwd`/`group` pair mapping the *current* uid and gid to the names a
/// real build would see.
///
/// The drain's contract is "look the owner up in this tree's passwd", so
/// mapping the test user's id to `root` exercises exactly that lookup and keeps
/// the rendered output the same on every machine. Nothing here pretends the
/// test is running as root — the ids are real, only the names are the tree's.
pub fn account_files(root: &Path) {
    let uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(2)?
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);
    let gid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Gid:"))?
                .split_whitespace()
                .nth(2)?
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);
    file(
        root,
        "etc/passwd",
        &format!("root:x:{uid}:{gid}:root:/root:/bin/sh\n"),
        0o644,
    );
    file(root, "etc/group", &format!("root:x:{gid}:\n"), 0o644);
}
