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
    ///
    /// `systemctl` is the one that is not uniformly hostile; see
    /// `systemctl_script`.
    pub fn hostile_to_images() -> Vec<Shim> {
        ["systemctl", "udevadm", "update-grub", "grub-mkconfig"]
            .into_iter()
            .map(Shim::new)
            .collect()
    }

    /// The wrapper's text. It records the call before exiting 0, so that
    /// `kiln build -v` can say `shimmed: systemctl daemon-reload` rather than
    /// leaving the user to wonder what a scriptlet tried to do.
    pub fn script(&self) -> String {
        match self.name.as_str() {
            "systemctl" => systemctl_script(),
            _ => neutralized_script(&self.name),
        }
    }
}

/// Record the call, do nothing, succeed.
///
/// One line per call — which is why the escaping below is worth a second
/// look. A shell `printf '%s\n'` writes a literal backslash-n, and every
/// shimmed call then lands on one unreadable line.
///
/// `2>/dev/null` comes *before* the append, not after: redirections are
/// applied left to right, and a shell reports a redirection it could not open
/// on whatever stderr it has at that moment. Written the other way round, a
/// build with no shim log — a sandbox that set none up — puts a "No such file
/// or directory" into the output of every scriptlet that touches a shim.
fn neutralized_script(name: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Written by Kiln. {name} addresses the running system, which during a\n\
         # build is the build host — not the image.\n\
         printf '%s\\n' \"{name} $*\" 2>/dev/null >> {SHIM_LOG}\n\
         exit 0\n"
    )
}

/// `systemctl`, which is hostile in every scope but one.
///
/// **`systemctl --global enable|disable|reenable` is not a request to the
/// running system at all.** It writes `/etc/systemd/user/<target>.wants/<unit>`
/// and nothing else — a plain filesystem edit that needs no bus, no running
/// systemd and no `--root`, because inside the transaction's root that path
/// *is* the image. Arch packages enable their **user** units this way and by
/// no other means: `pipewire`, `pipewire-pulse`, `wireplumber` and
/// `xdg-user-dirs` each call it from `post_install`, and none of them ships
/// the symlink in its file list. Neutralizing those calls dropped real image
/// content — an image with PipeWire installed and no PipeWire enabled, which
/// boots to silence with every card detected and every driver loaded.
///
/// `p11-kit` does the identical job with `mkdir` and `ln -sf`, is not shimmed,
/// and has always worked. That difference — which binary the scriptlet reached
/// for, not what it was trying to do — is the whole of the bug.
///
/// **System scope stays neutralized, deliberately.** Kiln realizes system unit
/// state declaratively: `20-kiln.preset` and `systemctl preset-all --root` in
/// assembly step 7, which reproduce the distribution's own defaults *and* let
/// the config override them. A scriptlet writing `.wants` symlinks behind that
/// step's back is the disagreement step 7 exists to prevent, and `kiln explain`
/// could not account for the result. Nothing is lost by refusing it: the same
/// preset pass is what puts `getty@tty1.service` in the image, which is what
/// `systemd`'s own `post_install` was asking for.
///
/// A pass-through is not written to the shim log. The log's one job is to say
/// which calls did *nothing*, and these did exactly what they said — like the
/// `ln -sf` beside them, which is not logged either.
fn systemctl_script() -> String {
    format!(
        "#!/bin/sh\n\
         # Written by Kiln. systemctl addresses the running system, which during\n\
         # a build is the build host — not the image. The exception is\n\
         # `--global enable|disable|reenable`, which only writes\n\
         # /etc/systemd/user inside this root: that is how every Arch package\n\
         # enables a user unit, and this root is the image.\n\
         scope=\n\
         verb=\n\
         for arg in \"$@\"; do\n\
         \tcase $arg in\n\
         \t\t--global) scope=global ;;\n\
         \t\t-*) ;;\n\
         \t\t*) [ -n \"$verb\" ] || verb=$arg ;;\n\
         \tesac\n\
         done\n\
         if [ \"$scope\" = global ] && [ -z \"$KILN_SHIM_REENTERED\" ]; then\n\
         \tcase $verb in\n\
         \t\tenable|disable|reenable)\n\
         \t\t\t# The real systemctl: the first one on PATH that is not this\n\
         \t\t\t# script. `$0` is the path the kernel execve'd for the\n\
         \t\t\t# shebang, so it names this shim wherever it was placed —\n\
         \t\t\t# /usr/local/bin during the alpm transaction, found through\n\
         \t\t\t# pacman's own PATH, or SHIM_DIR in a sandbox. Searching\n\
         \t\t\t# beats stripping a known directory, which silently does\n\
         \t\t\t# nothing when PATH is not the one that was expected.\n\
         \t\t\treal=\n\
         \t\t\toldifs=$IFS\n\
         \t\t\tIFS=:\n\
         \t\t\tfor dir in $PATH; do\n\
         \t\t\t\t[ -n \"$dir\" ] || dir=.\n\
         \t\t\t\tif [ -x \"$dir/systemctl\" ] && [ \"$dir/systemctl\" != \"$0\" ]; then\n\
         \t\t\t\t\treal=$dir/systemctl\n\
         \t\t\t\t\tbreak\n\
         \t\t\t\tfi\n\
         \t\t\tdone\n\
         \t\t\tIFS=$oldifs\n\
         \t\t\t# The marker is the second guard: if `$0` did not match — a\n\
         \t\t\t# second copy of the shim, a path spelled differently — the\n\
         \t\t\t# exec lands back here, sees it, and falls through to the\n\
         \t\t\t# no-op instead of looping until the build runs out of\n\
         \t\t\t# processes.\n\
         \t\t\tKILN_SHIM_REENTERED=1\n\
         \t\t\texport KILN_SHIM_REENTERED\n\
         \t\t\t[ -n \"$real\" ] && exec \"$real\" \"$@\"\n\
         \t\t\t;;\n\
         \tesac\n\
         fi\n\
         printf '%s\\n' \"systemctl $*\" 2>/dev/null >> {SHIM_LOG}\n\
         exit 0\n"
    )
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
