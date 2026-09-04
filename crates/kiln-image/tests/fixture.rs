//! Shared setup for the fixture-backed assembly tests. never the
//! network — `tests/repo-fixture` is a real tiny pacman repo built in-tree.
//!
//! Shared by every test binary in this crate, each of which compiles its own
//! copy — so anything one binary does not call looks dead to that binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

pub fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

pub fn repo() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = workspace().join("tests/repo-fixture");
    ONCE.call_once(|| {
        let out = std::process::Command::new(root.join("build.sh"))
            .output()
            .expect("tests/repo-fixture/build.sh must be runnable");
        assert!(
            out.status.success(),
            "the fixture repository failed to build:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    });
    root.join("repo")
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
