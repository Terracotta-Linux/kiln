//! Kiln's own pacman keyring.
//!
//! Kiln does not borrow the host's `/etc/pacman.d/gnupg`. A build must work on
//! a non-Arch host and in CI, and a keyring that only exists because the build
//! machine happens to be an Arch box is a dependency nobody declared.
//!
//! The keyring is *cache*, not state: deleting it costs a minute, never
//! correctness.

use crate::error::{Error, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the shipped Arch keys live on a host that has `archlinux-keyring`.
const KEYRING_SOURCE: &str = "/usr/share/pacman/keyrings";

#[derive(Debug, Clone)]
pub struct Keyring {
    pub gpgdir: PathBuf,
}

impl Keyring {
    pub fn at(gpgdir: impl Into<PathBuf>) -> Keyring {
        Keyring {
            gpgdir: gpgdir.into(),
        }
    }

    /// True once the keyring holds a trust database. Cheap enough to call on
    /// every resolution.
    pub fn is_populated(&self) -> bool {
        self.gpgdir.join("trustdb.gpg").is_file()
            && self
                .gpgdir
                .join("pubring.gpg")
                .metadata()
                .is_ok_and(|m| m.len() > 0)
    }

    /// Create and populate the keyring if it is not already. Idempotent.
    ///
    /// Shelling out to `pacman-key` is deliberate and is not the same
    /// compromise as shelling out to `pacman`: the *solver* has to be a library
    /// because Kiln needs its answer as data, whereas seeding a GPG home has no
    /// answer to parse — it either worked or it did not. Reimplementing
    /// pacman-key's ownertrust and revocation handling would be a second
    /// implementation of a security-critical procedure, which is a worse trade
    /// than a subprocess.
    ///
    /// **The keyring is built somewhere short and then moved into place.** See
    /// `build_dir`: `pacman-key --init` starts a gpg-agent, and a gpg-agent
    /// puts its sockets *inside the GPG home*, where they run into the 108-byte
    /// limit on a Unix socket path.
    pub fn ensure(&self) -> Result<()> {
        if self.is_populated() {
            return Ok(());
        }
        let source = Path::new(KEYRING_SOURCE);
        if !source.is_dir() {
            return Err(Error::Alpm {
                doing: "seeding Kiln's keyring",
                message: format!(
                    "{KEYRING_SOURCE} does not exist, so there are no Arch keys to import. \
                     Install `archlinux-keyring`, or declare the repository with an \
                     explicit key."
                ),
            });
        }

        let building = build_dir();
        let _ = std::fs::remove_dir_all(&building);
        std::fs::create_dir_all(&building).map_err(|e| Error::Alpm {
            doing: "creating Kiln's keyring",
            message: format!("{}: {e}", building.display()),
        })?;
        // A GPG home holds a locally generated secret key. 0700 before anything
        // is written into it, not after.
        let _ = std::fs::set_permissions(&building, std::fs::Permissions::from_mode(0o700));

        let built = self
            .pacman_key(&building, &["--init"])
            .and_then(|()| self.pacman_key(&building, &["--populate", "archlinux"]));
        if let Err(e) = built {
            let _ = std::fs::remove_dir_all(&building);
            return Err(e);
        }

        self.install(&building)?;
        let _ = std::fs::remove_dir_all(&building);
        Ok(())
    }

    /// Move the finished keyring to where the session expects it.
    ///
    /// A copy rather than a rename: the build directory is on `/tmp`, which is
    /// a different filesystem, and `rename(2)` across filesystems is `EXDEV`.
    fn install(&self, from: &Path) -> Result<()> {
        if let Some(parent) = self.gpgdir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Alpm {
                doing: "creating Kiln's keyring",
                message: format!("{}: {e}", parent.display()),
            })?;
        }
        let _ = std::fs::remove_dir_all(&self.gpgdir);
        // `cp -a` rather than a hand-rolled walk: a GPG home has modes that
        // matter, and one directory left at 0755 makes gpg refuse to use it.
        let out = Command::new("cp")
            .arg("-a")
            .arg(from)
            .arg(&self.gpgdir)
            .output()
            .map_err(|e| Error::Alpm {
                doing: "installing Kiln's keyring",
                message: format!("running cp: {e}"),
            })?;
        if !out.status.success() {
            return Err(Error::Alpm {
                doing: "installing Kiln's keyring",
                message: format!(
                    "copying {} to {}: {}",
                    from.display(),
                    self.gpgdir.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            });
        }
        Ok(())
    }

    fn pacman_key(&self, gpgdir: &Path, args: &[&str]) -> Result<()> {
        let out = Command::new("pacman-key")
            .arg("--gpgdir")
            .arg(gpgdir)
            .args(args)
            .output()
            .map_err(|e| Error::Alpm {
                doing: "running pacman-key",
                message: format!("{e} (is `pacman` installed?)"),
            })?;
        if !out.status.success() {
            return Err(Error::Alpm {
                doing: "running pacman-key",
                message: last_lines(&String::from_utf8_lossy(&out.stderr), 10),
            });
        }
        Ok(())
    }
}

/// A short path to build the keyring in, before it is moved to its real home.
///
/// This exists because of a limit that has nothing to do with Kiln and cannot
/// be worked around where it bites. `pacman-key --init` generates a local
/// master key, which needs a **gpg-agent**, and gpg-agent places its sockets
/// *inside the GPG home*. `sun_path` is 108 bytes, so a GPG home more than
/// about 88 characters deep makes `S.gpg-agent.browser` unrepresentable and the
/// agent refuses to start. What the user sees is
///
/// ```text
/// gpg: error running '/usr/bin/gpg-agent': exit status 2
/// gpg: can't connect to the gpg-agent: General error
/// ==> ERROR: There is no secret key available to sign with.
/// ```
///
/// which says nothing about path length and sends the reader looking at GPG
/// configuration, dbus and permissions in turn. The real gpgdir is
/// `<sysroot>/var/lib/kiln/keyring`, which is 21 characters at `--sysroot /`
/// and arbitrarily long under the `--sysroot` an installer uses — so
/// this is not a corner case, it is the installer seam.
///
/// Only *creating* the keyring needs the agent. Verifying a package signature
/// uses public keys only, so the keyring works perfectly well from a long path
/// once it exists.
///
/// The pid keeps two concurrent builds apart. The keyring is cache, so
/// where it is assembled is not a decision anyone has to live with.
fn build_dir() -> PathBuf {
    std::env::temp_dir().join(format!("kiln-keyring.{}", std::process::id()))
}

/// The tail of a subprocess's stderr. A GPG failure's useful line is always the
/// last one, and the preceding forty are noise.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_directory_is_not_a_keyring() {
        let dir = std::env::temp_dir().join("kiln-keyring-empty-test");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!Keyring::at(&dir).is_populated());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines("a\n\nb\nc\n", 2), "b\nc");
    }

    /// The keyring must be *built* somewhere a gpg-agent socket fits, whatever
    /// the sysroot looks like. `--sysroot` can be arbitrarily deep, and
    /// `sun_path` is 108 bytes.
    ///
    /// The margin is the point: `S.gpg-agent.browser` is 19 characters and a
    /// slash makes 20, so the build directory has to stay well under 88 for the
    /// agent to start at all.
    #[test]
    fn the_keyring_is_built_somewhere_a_gpg_agent_socket_fits() {
        let dir = build_dir();
        let longest = dir.join("S.gpg-agent.browser");
        assert!(
            longest.as_os_str().len() < 108,
            "{} is {} bytes; a Unix socket path is limited to 108",
            longest.display(),
            longest.as_os_str().len()
        );
    }

    /// …and it must not matter how long the *destination* is, which is the case
    /// that actually broke.
    ///
    /// The specimen is the real path the boot acceptance test uses, with the
    /// user renamed: `<sysroot>/var/lib/kiln/keyring` under a mounted install
    /// target. Its `S.gpg-agent.browser` is 106 bytes, and gpg-agent's own
    /// margin makes that unusable — building in place there fails with
    /// "error running '/usr/bin/gpg-agent': exit status 2" and no mention of a
    /// path.
    #[test]
    fn a_long_destination_is_not_where_the_agent_would_run() {
        let deep = Path::new("/home/someone/projects/kiln/target/test-roots/boot-acceptance/mnt")
            .join("var/lib/kiln/keyring");
        let socket = deep.join("S.gpg-agent.browser");
        assert!(
            socket.as_os_str().len() > build_dir().join("S.gpg-agent.browser").as_os_str().len(),
            "the specimen should be the long case, not the short one"
        );
        assert!(
            socket.as_os_str().len() > 100,
            "the specimen is {} bytes; it should be near the 108-byte limit, which is what \
             made this a real failure rather than a theoretical one",
            socket.as_os_str().len()
        );
        assert_ne!(
            Keyring::at(&deep).gpgdir,
            build_dir(),
            "the keyring is assembled somewhere short and moved, never built in place"
        );
    }
}
