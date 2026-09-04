//! The OSTree contract, checked.
//!
//! Defence in depth: fail the build rather than commit a tree that libostree
//! will refuse to manage, or — worse — will manage badly. Every assertion here
//! corresponds to something that went wrong at least once in the Phase 0 spike,
//! and several of them fail *after a successful boot*, which is the most
//! expensive place to find a problem.

use crate::tree;
use std::path::Path;

/// One thing wrong with the tree. Collected rather than thrown, because a tree
/// with three contract violations should report three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub what: String,
    /// What it would break, in terms of something the user cares about.
    pub consequence: &'static str,
}

/// The top-level symlinks that must exist, and where they must point.
pub const TOP_LEVEL_LINKS: &[(&str, &str)] = &[
    ("bin", "usr/bin"),
    ("sbin", "usr/bin"),
    ("lib", "usr/lib"),
    ("lib64", "usr/lib"),
    ("home", "var/home"),
    ("root", "var/roothome"),
    ("opt", "var/opt"),
    ("srv", "var/srv"),
    ("media", "run/media"),
    // Nothing in Arch creates this symlink, and it went unmentioned until it broke.
    // Without it libostree resolves its repo relative to the deployment root
    // and `ostree admin status` fails from *inside* the booted system.
    ("ostree", "sysroot/ostree"),
];

/// Real directories the commit must contain.
///
/// `etc` is deliberately **not** here and must not exist: the shipped defaults
/// live at `/usr/etc`, and libostree creates the deployment's `/etc` by
/// 3-way-merging that against the machine's own at deploy time.
///
/// `var` is here, and must be *empty* rather than absent — libostree creates
/// the real `/var` in the stateroot, but the commit carries the mountpoint.
pub const TOP_LEVEL_DIRS: &[&str] = &[
    "mnt", "proc", "sys", "dev", "run", "tmp", "sysroot", "boot", "usr", "var",
];

pub fn check(root: &Path) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut bad =
        |what: String, consequence: &'static str| problems.push(Problem { what, consequence });

    // The commit filter rejects /var too, but a message naming the paths
    // beats one from libostree naming a checksum.
    let left = tree::entries(&root.join("var")).unwrap_or_default();
    if !left.is_empty() {
        let names: Vec<String> = left
            .iter()
            .take(5)
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        bad(
            format!(
                "/var is not empty ({} entries: {})",
                left.len(),
                names.join(", ")
            ),
            "OSTree requires /var to be absent from the commit; it belongs to the machine",
        );
    }

    if root.join("etc").symlink_metadata().is_ok() {
        bad(
            "/etc still exists".into(),
            "the shipped defaults must be at /usr/etc so OSTree can 3-way-merge them \
             against the machine's own /etc at deploy time",
        );
    }
    if !root.join("usr/etc").is_dir() {
        bad(
            "/usr/etc is missing".into(),
            "a deployed system would boot with no /etc at all",
        );
    }

    // Both halves — the database has to be in /usr
    // and the directory the `pacman` package owns must not have been recreated.
    if !root.join("usr/lib/sysimage/pacman/local").is_dir() {
        bad(
            "the package database is not at /usr/lib/sysimage/pacman/local".into(),
            "`pacman -Q`, `kiln why` and `kiln owns` would find nothing on the booted image",
        );
    }
    if root.join("var/lib/pacman").exists() {
        bad(
            "/var/lib/pacman still exists".into(),
            "the drain would recreate it on every boot, and the relocation would be \
             true at build time and false at runtime",
        );
    }

    for (name, want) in TOP_LEVEL_LINKS {
        match std::fs::read_link(root.join(name)) {
            Ok(target) if target == Path::new(want) => {}
            Ok(target) => bad(
                format!("/{name} points at {} rather than {want}", target.display()),
                consequence_of(name),
            ),
            Err(_) => bad(
                format!("/{name} is not a symlink to {want}"),
                consequence_of(name),
            ),
        }
    }
    for name in TOP_LEVEL_DIRS {
        let at = root.join(name);
        if !at.is_dir()
            || at
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
        {
            bad(
                format!("/{name} is not a real directory"),
                "OSTree expects it as a mountpoint",
            );
        }
    }

    // Assembly step 5.
    if !tree::entries(&root.join("boot"))
        .unwrap_or_default()
        .is_empty()
    {
        bad(
            "/boot is not empty".into(),
            "OSTree owns /boot; the kernel and boot entries are Kiln's to place",
        );
    }

    // The half that is not about determinism at all.
    match std::fs::metadata(root.join("usr/etc/machine-id")) {
        Ok(md) if md.len() > 0 => bad(
            "/usr/etc/machine-id is not empty".into(),
            "every machine deployed from this image would share one identity",
        ),
        _ => {}
    }

    // libostree titles the boot entry from `ID` or `PRETTY_NAME` and refuses to
    // deploy a tree that has neither — "Installing kernel: No PRETTY_NAME or ID
    // in /etc/os-release". A tree without one commits perfectly happily, so the
    // failure lands at deploy time, after a build that looked like it worked.
    if !has_os_identity(root) {
        bad(
            "/usr/lib/os-release has no ID or PRETTY_NAME".into(),
            "libostree titles the boot entry from one of them and refuses to deploy              without either, so this image would commit and then fail to deploy",
        );
    }

    problems
}

/// Arch's `filesystem` package ships `/usr/lib/os-release` and symlinks
/// `/etc/os-release` at it, so both places are worth looking — the second
/// because normalization has already moved `/etc` to `/usr/etc` by the time
/// this runs.
fn has_os_identity(root: &Path) -> bool {
    ["usr/lib/os-release", "usr/etc/os-release"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(root.join(p)).ok())
        .any(|text| {
            text.lines()
                .any(|l| l.starts_with("ID=") || l.starts_with("PRETTY_NAME="))
        })
}

fn consequence_of(name: &str) -> &'static str {
    match name {
        "ostree" => {
            "libostree would resolve its repo relative to the deployment root, and \
                     `ostree admin status` — and so `kiln list`, `kiln status` and \
                     `kiln rollback` — would fail from inside the booted system"
        }
        "home" | "root" | "opt" | "srv" => {
            "it must point into /var, which is the only writable persistent storage"
        }
        _ => {
            "the image would not be usr-merged, and paths compiled into binaries would not \
              resolve"
        }
    }
}

/// Render the problems as something a person can act on.
pub fn describe(problems: &[Problem]) -> String {
    let mut out = format!(
        "the assembled tree does not satisfy the OSTree filesystem contract \
         ({} problem{}):\n",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    );
    for p in problems {
        out.push_str(&format!("\n  {}\n    {}\n", p.what, p.consequence));
    }
    out
}
