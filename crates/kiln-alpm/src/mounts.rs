//! Kernel filesystems in a root, for the duration of a transaction.
//!
//! Assembly step 4 is where
//! package hooks and scriptlets run, and libalpm runs them chrooted into the
//! staging root — where `/proc`, `/sys`, `/dev` and `/run` are empty
//! directories the skeleton made. `pacstrap` mounts all four for exactly this
//! reason.
//!
//! It lives beside the transaction rather than beside the assembler because
//! there are two roots libalpm installs into and both need it: assembly's
//! staging root, and realization's **build root**, which holds `base-devel` and
//! therefore drags in every scriptlet and hook `systemd` and `pacman` ship. A
//! build root without `/proc` fails in exactly the way described below, several
//! hundred megabytes into an install, naming a catalog file.
//!
//! The failure without them is worth writing down, because nothing about it
//! points at a mount. `journalctl --update-catalog` reports
//!
//! ```text
//! Failed to open file /usr/lib/systemd/catalog/dbus-broker-launch.catalog:
//!   No such file or directory
//! ```
//!
//! about a file that is present, readable, and opens fine with `head` from
//! inside the same chroot. systemd's path helpers reopen descriptors through
//! `/proc/self/fd`, so with no `/proc` the *reopen* fails and the error is
//! reported against the original path. Every minute spent looking at that
//! catalog file is a minute spent in the wrong place.
//!
//! The mounts are a guard: they go away when it drops, including on the error
//! path, because a build that failed halfway must not leave `/proc` bind-mounted
//! under `/var/lib/kiln`.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What to mount, in order. Unmounted in reverse.
///
/// `--rbind` for `/dev`, because `/dev/pts` and `/dev/shm` are separate mounts
/// and a plain bind would leave a scriptlet that wants a pty looking at an
/// empty directory. `--make-rslave` after it so that anything the build mounts
/// cannot propagate back to the host.
const SPECS: &[Spec] = &[
    Spec::Kind {
        at: "proc",
        fstype: "proc",
    },
    Spec::Kind {
        at: "sys",
        fstype: "sysfs",
    },
    Spec::Bind { at: "dev" },
    // A tmpfs rather than a bind of the host's `/run`: a scriptlet writing
    // there is writing *machine* state, and binding the host's would let it
    // reach the build machine's runtime directory. Kiln shadows the tmpfiles
    // hook for the same reason.
    Spec::Tmpfs { at: "run" },
];

enum Spec {
    Kind {
        at: &'static str,
        fstype: &'static str,
    },
    Bind {
        at: &'static str,
    },
    Tmpfs {
        at: &'static str,
    },
}

impl Spec {
    fn at(&self) -> &'static str {
        match self {
            Spec::Kind { at, .. } | Spec::Bind { at } | Spec::Tmpfs { at } => at,
        }
    }
}

/// Mounted for as long as this is alive.
pub struct Mounts {
    active: Vec<PathBuf>,
}

impl Mounts {
    pub fn setup(root: &Path) -> Result<Mounts> {
        let mut mounts = Mounts { active: Vec::new() };
        for spec in SPECS {
            let target = root.join(spec.at());
            std::fs::create_dir_all(&target).map_err(|e| Error::Mount {
                at: target.clone(),
                message: e.to_string(),
            })?;
            let at = target.to_string_lossy().to_string();

            let args: Vec<String> = match spec {
                Spec::Kind { fstype, .. } => {
                    vec!["-t".into(), (*fstype).into(), (*fstype).into(), at.clone()]
                }
                Spec::Bind { .. } => {
                    vec!["--rbind".into(), format!("/{}", spec.at()), at.clone()]
                }
                Spec::Tmpfs { .. } => {
                    vec!["-t".into(), "tmpfs".into(), "tmpfs".into(), at.clone()]
                }
            };
            mount(&args)?;
            // Pushed immediately: if `--make-rslave` fails, the mount is still
            // there and still has to come back off.
            mounts.active.push(target);
            if matches!(spec, Spec::Bind { .. }) {
                let _ = mount(&["--make-rslave".to_string(), at]);
            }
        }
        Ok(mounts)
    }

    /// Unmount everything, deepest first. Lazy (`-l`) so a descriptor a
    /// scriptlet left open cannot keep `/proc` mounted under the build
    /// directory forever.
    pub fn teardown(&mut self) {
        for target in self.active.drain(..).rev() {
            let _ = Command::new("umount")
                .args(["-R", "-l"])
                .arg(&target)
                .output();
        }
    }
}

impl Drop for Mounts {
    fn drop(&mut self) {
        self.teardown();
    }
}

fn mount(args: &[String]) -> Result<()> {
    let at = PathBuf::from(args.last().map_or("", |s| s.as_str()));
    let out = Command::new("mount")
        .args(args)
        .output()
        .map_err(|e| Error::Mount {
            at: at.clone(),
            message: format!("running mount: {e}"),
        })?;
    if !out.status.success() {
        return Err(Error::Mount {
            at,
            message: format!(
                "`mount {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(())
}
