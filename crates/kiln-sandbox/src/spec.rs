//! What to run, where, and with what taken away.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the shim wrappers are mounted inside the sandbox. Fixed rather than
/// generated so that a `PATH` printed in a log is the same in every build.
pub const SHIM_DIR: &str = "/run/kiln/shims";

/// Where a shim records that it was called, inside the sandbox.
pub const SHIM_LOG: &str = "/run/kiln/shims.log";

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// The staging root the command sees as `/`.
    pub root: PathBuf,
    /// argv. lists no command because it describes the *isolation*; a
    /// sandbox with nothing to run is not useful, so it lives here.
    pub command: Vec<String>,
    /// Explicit, with no implicit host access.
    pub binds: Vec<Bind>,
    pub network: Network,
    pub user: SandboxUser,
    /// Cleared, then explicitly populated.
    pub env: BTreeMap<String, String>,
    /// binaries neutralized to no-ops for the duration, each call logged.
    pub shims: Vec<Shim>,
    pub limits: Limits,
    /// Inside the sandbox. `/` when unset.
    pub workdir: Option<PathBuf>,
    /// A host path to write the run's combined output to, whether it succeeded
    /// or not.
    ///
    /// The sandbox promises that *the full log is always written* and that its path is
    /// printed on failure. That cannot be done by the caller, because a failing
    /// run comes back as `Error::Failed` carrying only the last forty lines —
    /// the rest is gone by then. So the only place with the whole thing is the
    /// runner, and this is how it is asked for it.
    pub log: Option<PathBuf>,
}

impl SandboxSpec {
    /// A command run against a staging root, with the isolation the build
    /// phase requires: **no network**, and the standard kernel filesystems.
    ///
    /// The network default is not a convenience: it is the constraint
    /// the rest of the model rests on. With the network off, a command's output
    /// is a pure function of things Kiln already hashes. Defaulting the other
    /// way and asking every caller to remember would put that guarantee one
    /// forgotten line away from being false.
    pub fn in_root(root: impl Into<PathBuf>, command: impl IntoIterator<Item = String>) -> Self {
        SandboxSpec {
            root: root.into(),
            command: command.into_iter().collect(),
            binds: Bind::kernel_filesystems(),
            network: Network::Disabled,
            user: SandboxUser::Root,
            env: default_env(),
            shims: Vec::new(),
            limits: Limits::default(),
            workdir: None,
            log: None,
        }
    }

    pub fn with_network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    pub fn with_bind(mut self, bind: Bind) -> Self {
        self.binds.push(bind);
        self
    }

    pub fn with_shims(mut self, shims: impl IntoIterator<Item = Shim>) -> Self {
        self.shims = shims.into_iter().collect();
        self
    }

    pub fn with_env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.insert(key.to_string(), value.into());
        self
    }

    pub fn with_timeout(mut self, wall: Duration) -> Self {
        self.limits.wall = Some(wall);
        self
    }

    pub fn with_user(mut self, user: SandboxUser) -> Self {
        self.user = user;
        self
    }

    /// Tee this run's output to a file on the host.
    pub fn logging_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.log = Some(path.into());
        self
    }

    /// The `PATH` the command actually sees: the shim directory first, so a
    /// shimmed binary wins over the image's own.
    pub fn effective_path(&self) -> String {
        let base = self.env.get("PATH").cloned().unwrap_or_default();
        if self.shims.is_empty() {
            base
        } else {
            format!("{SHIM_DIR}:{base}")
        }
    }
}

/// cleared, then explicitly populated. `SOURCE_DATE_EPOCH` is pinned to
/// 0 for the same reason OSTree canonicalizes mtimes — a build must not
/// be able to tell what time it is.
pub fn default_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("PATH".into(), "/usr/bin:/usr/sbin:/bin:/sbin".into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("LC_ALL".into(), "C.UTF-8".into()),
        ("TZ".into(), "UTC".into()),
        ("SOURCE_DATE_EPOCH".into(), "0".into()),
        ("HOME".into(), "/root".into()),
        ("TERM".into(), "dumb".into()),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub source: PathBuf,
    pub target: PathBuf,
    pub mode: BindMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindMode {
    ReadOnly,
    ReadWrite,
    /// A fresh kernel filesystem rather than a bind of the host's.
    DevFs,
    ProcFs,
    /// `/sys`. Its own mode rather than a read-only bind of `/sys`, because the
    /// two backends disagree about it: bubblewrap has no `--sys` and needs the
    /// host's bound in, while nspawn mounts one itself and binding over it
    /// would replace a container's view with the host's.
    SysFs,
    /// A private tmpfs, for `/tmp` and `/run`.
    TmpFs,
}

impl Bind {
    pub fn ro(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Bind {
        Bind {
            source: source.into(),
            target: target.into(),
            mode: BindMode::ReadOnly,
        }
    }

    pub fn rw(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Bind {
        Bind {
            source: source.into(),
            target: target.into(),
            mode: BindMode::ReadWrite,
        }
    }

    fn special(mode: BindMode, target: &str) -> Bind {
        Bind {
            source: PathBuf::new(),
            target: PathBuf::from(target),
            mode,
        }
    }

    /// What a chrooted distribution tool needs and no more. `/dev`
    /// minimal, `/proc` from a fresh mount, and no host network namespace.
    /// `/run` and `/tmp` are private tmpfs mounts so that whatever a scriptlet
    /// leaves in them cannot reach the image — which is the same reason the
    /// tmpfiles hook has to be shadowed.
    pub fn kernel_filesystems() -> Vec<Bind> {
        vec![
            Bind::special(BindMode::ProcFs, "/proc"),
            Bind::special(BindMode::DevFs, "/dev"),
            Bind::special(BindMode::SysFs, "/sys"),
            Bind::special(BindMode::TmpFs, "/run"),
            Bind::special(BindMode::TmpFs, "/tmp"),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Enabled,
    /// `CLONE_NEWNET` with no interfaces.
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxUser {
    /// builds run as root, always, so that ownership, setuid bits and
    /// file capabilities land in the commit exactly as the packages declare.
    Root,
    /// For `makepkg`, which refuses to run as root (phase 3).
    Unprivileged { uid: u32, gid: u32 },
}

/// A binary replaced by a wrapper that records the call and exits
/// 0, because a distribution scriptlet asking the *build host* to reload
/// systemd is asking the wrong machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shim {
    pub name: String,
}

impl Shim {
    pub fn new(name: impl Into<String>) -> Shim {
        Shim { name: name.into() }
    }

    /// The binaries whose scriptlet behaviour is hostile to an image build.
    /// Each one addresses the *running* system, which during a build is the
    /// build host — a machine that has nothing to do with the image.
    pub fn hostile_to_images() -> Vec<Shim> {
        ["systemctl", "udevadm", "update-grub", "grub-mkconfig"]
            .into_iter()
            .map(Shim::new)
            .collect()
    }

    /// The wrapper's text. It records the call before exiting 0, so that
    /// `kiln build -v` can say `shimmed: systemctl daemon-reload` rather than
    /// leaving the user to wonder what a scriptlet tried to do.
    ///
    /// One line per call — which is why the escaping below is worth a second
    /// look. A shell `printf '%s\\n'` writes a literal backslash-n, and every
    /// shimmed call then lands on one unreadable line.
    pub fn script(&self) -> String {
        format!(
            "#!/bin/sh\n\
             # Written by Kiln. {} addresses the running system, which during a\n\
             # build is the build host — not the image.\n\
             printf '%s\\n' \"{} $*\" >> {SHIM_LOG} 2>/dev/null\n\
             exit 0\n",
            self.name, self.name
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    /// Wall-clock timeout. Enforced by the parent, so every backend honours it.
    pub wall: Option<Duration>,
    /// Bytes. Requires a backend with cgroup control.
    pub memory: Option<u64>,
    /// Requires a backend with cgroup control.
    pub pids: Option<u32>,
}

impl Limits {
    pub fn needs_cgroups(&self) -> bool {
        self.memory.is_some() || self.pids.is_some()
    }
}

/// A path inside the sandbox, rendered for an argv.
pub(crate) fn inside(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
