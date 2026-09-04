//! The bubblewrap backend. The default — cheap, no systemd
//! dependency, right for chrooted build steps and for PKGBUILD builds.

use crate::exec;
use crate::spec::{inside, BindMode, Network, SandboxSpec, SandboxUser, SHIM_DIR, SHIM_LOG};
use crate::{Error, Outcome, Result, Sandbox};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Bubblewrap {
    /// Where the shim wrappers and their log are written on the *host*. Under
    /// the build directory, never inside the staging root: they are facts about
    /// the build, and anything left in the root would have to be cleaned up
    /// again before the commit.
    pub scratch: PathBuf,
}

impl Bubblewrap {
    pub fn new(scratch: impl Into<PathBuf>) -> Bubblewrap {
        Bubblewrap {
            scratch: scratch.into(),
        }
    }

    fn shim_dir(&self) -> PathBuf {
        self.scratch.join("shims")
    }

    /// The argv, given where the shim log lives on the host. Split out so that
    /// `argv()` can render a faithful command line without writing anything.
    fn build_argv(&self, spec: &SandboxSpec, shim_log: Option<&PathBuf>) -> Result<Vec<String>> {
        if spec.command.is_empty() {
            return Err(Error::Unsupported {
                backend: "bubblewrap",
                what: "an empty command".into(),
            });
        }
        // Kiln puts cgroup limits on the nspawn backend. Refusing here rather
        // than ignoring them is the whole point: a caller that asked for a
        // memory cap and silently did not get one is worse off than one that
        // never asked.
        if spec.limits.needs_cgroups() {
            return Err(Error::Unsupported {
                backend: "bubblewrap",
                what: "memory or pid limits — use the nspawn backend".into(),
            });
        }

        // Decided before the binds, because it changes what a private tmpfs has
        // to be: bubblewrap makes one 0755 and root-owned, which is right for a
        // command running as root and useless to one that is not.
        let unprivileged = matches!(spec.user, SandboxUser::Unprivileged { .. });

        let mut a: Vec<String> = vec!["bwrap".into()];
        let mut push = |args: &[&str]| a.extend(args.iter().map(|s| s.to_string()));

        // The staging root becomes `/`. Read-write: this is the tree being
        // assembled, and the transaction writes to it.
        push(&["--bind", &inside(&spec.root), "/"]);

        for bind in &spec.binds {
            match bind.mode {
                BindMode::ProcFs => push(&["--proc", &inside(&bind.target)]),
                BindMode::DevFs => push(&["--dev", &inside(&bind.target)]),
                // A build runs as an unprivileged user (the sandbox's one exception)
                // and `/tmp` is where a compiler puts things. A private tmpfs
                // nothing outside the sandbox can see is exactly where the
                // sticky-bit-and-world-writable convention belongs — but only
                // where it is needed: widening a mode for a command that runs
                // as root buys nothing.
                BindMode::TmpFs if unprivileged => {
                    push(&["--perms", "1777", "--tmpfs", &inside(&bind.target)])
                }
                BindMode::TmpFs => push(&["--tmpfs", &inside(&bind.target)]),
                // bubblewrap has no `--sys`, so the host's is bound read-only.
                // `-try` so that a host without one — a container, mostly — is
                // a missing mount rather than a failed build.
                BindMode::SysFs => push(&["--ro-bind-try", "/sys", &inside(&bind.target)]),
                BindMode::ReadOnly => push(&[
                    "--ro-bind-try",
                    &inside(&bind.source),
                    &inside(&bind.target),
                ]),
                BindMode::ReadWrite => {
                    push(&["--bind", &inside(&bind.source), &inside(&bind.target)])
                }
            }
        }

        if !spec.shims.is_empty() {
            push(&["--ro-bind", &inside(&self.shim_dir()), SHIM_DIR]);
            if let Some(log) = shim_log {
                // A file bind, so what the shims append is readable from the
                // host after the run. Without it the log would land on the
                // `/run` tmpfs and vanish with the namespace.
                push(&["--bind", &inside(log), SHIM_LOG]);
            }
        }

        match spec.network {
            Network::Disabled => push(&["--unshare-net"]),
            Network::Enabled => {}
        }

        push(&["--clearenv"]);
        for (k, v) in &spec.env {
            // PATH is handled below so the shim directory can win.
            if k != "PATH" {
                push(&["--setenv", k, v]);
            }
        }
        push(&["--setenv", "PATH", &spec.effective_path()]);

        if let Some(dir) = &spec.workdir {
            push(&["--chdir", &inside(dir)]);
        }

        // Nothing sandboxed should outlive the build that started it.
        push(&["--die-with-parent", "--new-session"]);

        push(&["--"]);

        // Privileges are dropped *inside* the sandbox, not by remapping the
        // user namespace around it.
        //
        // `--unshare-user --uid 1000` reads like the obvious way to do this and
        // is wrong twice. It maps exactly one id — the caller's — so bubblewrap
        // resolves every bind source as the mapped user, and a source whose
        // *ancestor* is owned by anyone else becomes unreachable: a staging
        // root under a home directory fails with `Can't find source path …:
        // Permission denied`, naming a path that is plainly there. And it maps
        // the caller, who is root, *to* the build user — so everything
        // root owns appears to belong to the build, which in phase 1's sandbox
        // means write access to the whole host filesystem. Dropping privileges
        // for real gives up neither the mounts nor the isolation, and takes
        // away an authority the build should never have had.
        //
        // `setpriv` is `util-linux`, which every build root has: `makepkg`
        // needs `fakeroot`, and `fakeroot` depends on it.
        if let SandboxUser::Unprivileged { uid, gid } = spec.user {
            push(&[
                "setpriv",
                "--reuid",
                &uid.to_string(),
                "--regid",
                &gid.to_string(),
                "--clear-groups",
                "--",
            ]);
        }
        a.extend(spec.command.iter().cloned());
        Ok(a)
    }
}

impl Sandbox for Bubblewrap {
    fn name(&self) -> &'static str {
        "bubblewrap"
    }

    fn argv(&self, spec: &SandboxSpec) -> Result<Vec<String>> {
        let log = (!spec.shims.is_empty()).then(|| self.shim_dir().join("calls.log"));
        self.build_argv(spec, log.as_ref())
    }

    fn run(&self, spec: &SandboxSpec) -> Result<Outcome> {
        which("bwrap").ok_or_else(|| Error::Missing {
            backend: "bubblewrap",
            hint: "install `bubblewrap`".into(),
        })?;

        let shim_log = exec::materialize_shims(spec, &self.shim_dir())?;
        let argv = self.build_argv(spec, shim_log.as_ref())?;
        let outcome = exec::spawn(spec, &argv, shim_log.as_deref())?;

        if !outcome.ok() {
            return Err(Error::Failed {
                command: spec.command.join(" "),
                status: outcome.status,
                stderr: exec::tail(&outcome.stderr, 40),
            });
        }
        Ok(outcome)
    }
}

/// A `PATH` lookup, so that "is the backend installed" is answered before the
/// build gets far enough to fail confusingly.
pub(crate) fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(binary))
            .find(|p| p.is_file())
    })
}
