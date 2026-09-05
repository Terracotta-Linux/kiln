//! Where things live.

use std::path::{Path, PathBuf};

/// `/var/lib/kiln` — the state directory. All of it is cache and
/// history: deleting it costs time, never correctness.
pub const STATE: &str = "/var/lib/kiln";

/// The sysroot to operate on. `/` unless `--sysroot` says otherwise, which is
/// what exposes so an installer can be written against Kiln without
/// Kiln having an installer.
pub fn sysroot(flag: Option<&Path>) -> PathBuf {
    flag.map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// State lives *under the sysroot*, not at an absolute path. Building into
/// `--sysroot /mnt` while writing the artifact store to the host's
/// `/var/lib/kiln` would put half the operation on the wrong machine.
pub fn state(sysroot: &Path) -> PathBuf {
    sysroot.join(STATE.trim_start_matches('/'))
}

pub fn repo(sysroot: &Path) -> PathBuf {
    sysroot.join("ostree/repo")
}

/// `<sysroot>/ostree/deploy/<stateroot>` — what `kiln sysroot init` creates and
/// what every deployment lives under.
///
/// This, not the repository, is what says a sysroot is usable. `kiln build`
/// creates `ostree/repo` by itself if it is missing, so a target that has been
/// built into but never initialized has a repository full of perfectly good
/// commits and nowhere to deploy them — and asking about the repository would
/// call that initialized.
pub fn stateroot(sysroot: &Path) -> PathBuf {
    sysroot
        .join("ostree/deploy")
        .join(kiln_ostree::deploy::STATEROOT)
}

/// Has `kiln sysroot init` been run here?
pub fn is_initialized(sysroot: &Path) -> bool {
    stateroot(sysroot).exists()
}

/// `/var/lib/kiln/build/<plan_id>`. Keyed by plan so a retried build
/// reuses nothing from a different one.
pub fn build_dir(state: &Path, plan_id: &str) -> PathBuf {
    state.join("build").join(plan_id.replace(':', "-"))
}

pub fn cache(state: &Path) -> PathBuf {
    state.join("cache/pkg")
}

/// Where a `packages.file` URL's download lands, keyed by its declared
/// `sha256` — content-addressed like `cache`, so a plan shared across
/// generations downloads it once.
pub fn file_packages(state: &Path) -> PathBuf {
    state.join("cache/file-packages")
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
