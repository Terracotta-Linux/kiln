//! The systemd-nspawn backend. for the pacman transaction,
//! where scriptlets sometimes want a more complete environment, and for
//! cgroup-based resource limits — which bubblewrap has no way to apply.

use crate::exec;
use crate::spec::{inside, BindMode, Network, SandboxSpec, SandboxUser, SHIM_DIR, SHIM_LOG};
use crate::{Error, Outcome, Result, Sandbox};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Nspawn {
    pub scratch: PathBuf,
}

impl Nspawn {
    pub fn new(scratch: impl Into<PathBuf>) -> Nspawn {
        Nspawn {
            scratch: scratch.into(),
        }
    }

    fn shim_dir(&self) -> PathBuf {
        self.scratch.join("shims")
    }

    fn build_argv(&self, spec: &SandboxSpec, shim_log: Option<&PathBuf>) -> Result<Vec<String>> {
        if spec.command.is_empty() {
            return Err(Error::Unsupported {
                backend: "nspawn",
                what: "an empty command".into(),
            });
        }
        if matches!(spec.user, SandboxUser::Unprivileged { .. }) {
            // nspawn's `--user` resolves the name inside the container, which
            // needs an account that exists there. A staging root mid-assembly
            // may not have one yet, and silently running as root would defeat
            // the reason the caller asked.
            return Err(Error::Unsupported {
                backend: "nspawn",
                what: "an unprivileged user — use the bubblewrap backend".into(),
            });
        }

        let mut a: Vec<String> = vec!["systemd-nspawn".into()];
        let mut push = |args: &[&str]| a.extend(args.iter().map(|s| s.to_string()));

        push(&["--quiet", "--directory", &inside(&spec.root)]);
        // A build tree is not a machine: no boot, no machine registration, and
        // no leftover `/etc/machine-id` — which normalization has to truncate
        // anyway.
        push(&["--register=no", "--keep-unit", "--as-pid2"]);
        push(&["--link-journal=no"]);

        // nspawn already provides /proc, /dev, /sys, /run and /tmp. The kernel
        // filesystem entries in the spec are therefore satisfied rather than
        // translated; only real binds have anything to add.
        for bind in &spec.binds {
            match bind.mode {
                BindMode::ProcFs | BindMode::DevFs | BindMode::TmpFs | BindMode::SysFs => {}
                BindMode::ReadOnly => push(&[
                    "--bind-ro",
                    &format!("{}:{}", inside(&bind.source), inside(&bind.target)),
                ]),
                BindMode::ReadWrite => push(&[
                    "--bind",
                    &format!("{}:{}", inside(&bind.source), inside(&bind.target)),
                ]),
            }
        }

        if !spec.shims.is_empty() {
            push(&[
                "--bind-ro",
                &format!("{}:{SHIM_DIR}", inside(&self.shim_dir())),
            ]);
            if let Some(log) = shim_log {
                push(&["--bind", &format!("{}:{SHIM_LOG}", inside(log))]);
            }
        }

        match spec.network {
            Network::Disabled => push(&["--private-network"]),
            Network::Enabled => {}
        }

        if let Some(bytes) = spec.limits.memory {
            push(&["--property", &format!("MemoryMax={bytes}")]);
        }
        if let Some(pids) = spec.limits.pids {
            push(&["--property", &format!("TasksMax={pids}")]);
        }

        for (k, v) in &spec.env {
            if k != "PATH" {
                push(&["--setenv", &format!("{k}={v}")]);
            }
        }
        push(&["--setenv", &format!("PATH={}", spec.effective_path())]);

        if let Some(dir) = &spec.workdir {
            push(&["--chdir", &inside(dir)]);
        }

        a.extend(spec.command.iter().cloned());
        Ok(a)
    }
}

impl Sandbox for Nspawn {
    fn name(&self) -> &'static str {
        "nspawn"
    }

    fn argv(&self, spec: &SandboxSpec) -> Result<Vec<String>> {
        let log = (!spec.shims.is_empty()).then(|| self.shim_dir().join("calls.log"));
        self.build_argv(spec, log.as_ref())
    }

    fn run(&self, spec: &SandboxSpec) -> Result<Outcome> {
        crate::bwrap::which("systemd-nspawn").ok_or_else(|| Error::Missing {
            backend: "nspawn",
            hint: "install `systemd-container`".into(),
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
