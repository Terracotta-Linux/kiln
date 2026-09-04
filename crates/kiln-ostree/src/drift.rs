//! `/etc` drift: what the live `/etc` has that the image did not ship.
//! and the hazard the OSTree merge raises it from.
//!
//! > A file Kiln ships to `/etc` becomes part of `/usr/etc`. If a user
//! > hand-edits it on the live system, OSTree's 3-way merge treats it as a
//! > local modification and that user's version wins *forever*, silently
//! > ignoring future Kiln changes.
//!
//! The comparison is the same one libostree's merge makes, run early: a
//! deployment's `/usr/etc` is the default the image shipped, its `/etc` is what
//! is actually there, and the difference between them is precisely the set of
//! changes the *next* deploy will carry forward onto a new commit. Nothing here
//! calls libostree — the merge's inputs are two directories in the deployment,
//! both readable with `std::fs` — which is what lets this be tested against two
//! temporary directories rather than a booted machine.
//!
//! Three kinds of difference, and they are not equally interesting:
//!
//! - **Modified** is the hazard itself. Kiln ships a value, the machine has
//!   another, and every future generation's version of that file loses.
//! - **Removed** is the same hazard mirrored. The deletion is carried forward
//!   too, so a future generation that ships the file still will not have it.
//! - **Added** is not the hazard at all. A locally created file shadows
//!   nothing; it is a file the image never had an opinion about. It becomes
//!   interesting only if a future generation ships that path, which is a
//!   question about a plan and not about a deployment — so it is counted here
//!   and reported quietly.

use crate::{Error, Result};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// One path where the live `/etc` and the shipped `/usr/etc` disagree. `path`
/// is always spelled the way the user sees it — `/etc/pacman.conf`, not
/// `usr/etc/pacman.conf` (the user should never have to type `ostree`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// Shipped and present, and not the same. The merge keeps the live one.
    Modified { path: String, how: How },
    /// Shipped and deleted. The merge keeps the deletion.
    Removed { path: String },
    /// Never shipped. The merge keeps it, and nothing of Kiln's is shadowed.
    Added { path: String },
}

/// What differs, in the order a person cares: content first, then the metadata
/// OSTree also tracks and also carries forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum How {
    /// A regular file whose bytes differ, or a symlink pointing elsewhere.
    Contents,
    /// Same bytes, different permission bits. A `chmod` is a local
    /// modification like any other and survives exactly as long.
    Mode,
    /// Same bytes, different uid or gid.
    Owner,
    /// A file where a directory was shipped, or a symlink where a file was.
    Kind,
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::Modified { path, .. } | Change::Removed { path } | Change::Added { path } => {
                path
            }
        }
    }

    /// Whether this change shadows something the image shipped. `Added` is the
    /// only one that does not, and the distinction is the whole reason the
    /// three are separate variants.
    pub fn shadows_the_image(&self) -> bool {
        !matches!(self, Change::Added { .. })
    }
}

/// Compare a deployment's live `/etc` against the `/usr/etc` it shipped.
///
/// `deployment` is a deployment directory — `Sysroot::deployment_root`. A
/// deployment with no `/usr/etc` is not a Kiln image and yields nothing rather
/// than an error: `kiln status` must still print the rest of what it knows.
pub fn scan(deployment: &Path) -> Result<Vec<Change>> {
    let shipped = deployment.join("usr/etc");
    let live = deployment.join("etc");
    if !shipped.is_dir() || !live.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    walk(&shipped, &live, "/etc", &mut out)?;
    out.sort();
    Ok(out)
}

/// Paths whose difference is not drift, with the reason each one is here.
///
/// Every entry is a file that a correct, untouched Arch system changes by
/// itself — because the image deliberately ships it blank, because
/// something generates it at first boot, or because it belongs to a machine
/// rather than to an image. Reporting them would mean every
/// `kiln status` on every machine opens with a dozen lines of drift that
/// nobody can act on, and a warning nobody can act on is one people learn to
/// scroll past — including on the day it says something real.
///
/// The list is deliberately short and deliberately not configurable. Anything
/// added to it is a file Kiln stops being able to warn about, forever.
const NOT_DRIFT: &[&str] = &[
    // shipped truncated on purpose, so systemd populates it at first
    // boot. It differs on every machine that has ever booted.
    "/etc/machine-id",
    // Kiln pins system UIDs and does not manage login accounts. The
    // difference here is the machine's own users, which is not image content.
    "/etc/passwd",
    "/etc/passwd-",
    "/etc/group",
    "/etc/group-",
    "/etc/shadow",
    "/etc/shadow-",
    "/etc/gshadow",
    "/etc/gshadow-",
    "/etc/subuid",
    "/etc/subuid-",
    "/etc/subgid",
    "/etc/subgid-",
    // Kiln does not install anything, so the storage layout is the
    // installer's and differs from the image by construction.
    "/etc/fstab",
    "/etc/crypttab",
    // Generated at first boot, and a machine identity rather than a
    // configuration.
    "/etc/ssh/ssh_host_dsa_key",
    "/etc/ssh/ssh_host_dsa_key.pub",
    "/etc/ssh/ssh_host_ecdsa_key",
    "/etc/ssh/ssh_host_ecdsa_key.pub",
    "/etc/ssh/ssh_host_ed25519_key",
    "/etc/ssh/ssh_host_ed25519_key.pub",
    "/etc/ssh/ssh_host_rsa_key",
    "/etc/ssh/ssh_host_rsa_key.pub",
    // Caches and runtime markers, not configuration. `ld.so.cache` is
    // regenerated by ldconfig, `.updated` and `.pwd.lock` are written by
    // systemd and shadow as they run.
    "/etc/ld.so.cache",
    "/etc/.updated",
    "/etc/.pwd.lock",
    "/etc/.updated~",
    // Resolver state, owned by whatever manages the network. On a machine
    // running NetworkManager or systemd-resolved it is replaced every boot.
    "/etc/resolv.conf",
    // The configuration Kiln is reading. It is not image content, so on the
    // usual machine it is an addition against every generation.
    "/etc/kiln",
];

/// Exact match, or anything below it: `/etc/kiln` covers `/etc/kiln/system.toml`.
fn excluded(path: &str) -> bool {
    NOT_DRIFT.iter().any(|e| match path.strip_prefix(e) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    })
}

fn walk(shipped: &Path, live: &Path, at: &str, out: &mut Vec<Change>) -> Result<()> {
    let mut names: BTreeSet<std::ffi::OsString> = BTreeSet::new();
    names.extend(entries(shipped)?);
    names.extend(entries(live)?);

    for name in names {
        let Some(name) = name.to_str() else {
            // A name that is not UTF-8 cannot be printed as a path a user
            // could act on, and nothing Kiln ships has one. Skipping is
            // better than rendering it lossily and naming a file that does
            // not exist.
            continue;
        };
        let path = format!("{}/{name}", at.trim_end_matches('/'));
        if excluded(&path) {
            continue;
        }
        let (s, l) = (shipped.join(name), live.join(name));
        let (sm, lm) = (meta(&s)?, meta(&l)?);

        match (sm, lm) {
            (None, None) => {}
            (Some(_), None) => out.push(Change::Removed { path }),
            // Not recursed into: a whole directory nobody shipped is one
            // addition, not one per file inside it.
            // `/etc/NetworkManager/system-connections` holding nine saved
            // networks is one thing the user did.
            (None, Some(_)) => out.push(Change::Added { path }),
            (Some(sm), Some(lm)) => {
                if sm.is_dir() && lm.is_dir() {
                    if let Some(how) = metadata_change(&sm, &lm) {
                        out.push(Change::Modified {
                            path: path.clone(),
                            how,
                        });
                    }
                    walk(&s, &l, &path, out)?;
                } else if sm.is_dir() != lm.is_dir() || sm.is_symlink() != lm.is_symlink() {
                    out.push(Change::Modified {
                        path,
                        how: How::Kind,
                    });
                } else if let Some(how) = differs(&s, &l, &sm, &lm)? {
                    out.push(Change::Modified { path, how });
                }
            }
        }
    }
    Ok(())
}

/// Content first, then the metadata OSTree stores beside it. Content is what a
/// person means by "I edited that file", so a file that differs in both should
/// say so rather than reporting a mode change.
fn differs(s: &Path, l: &Path, sm: &fs::Metadata, lm: &fs::Metadata) -> Result<Option<How>> {
    if sm.is_symlink() {
        return Ok((read_link(s)? != read_link(l)?).then_some(How::Contents));
    }
    // Same inode on the same device is the same bytes, for free. libostree
    // hardlinks an unmodified `/etc` file to the object it checked out, so on
    // a real deployment this is the case that most files take.
    if sm.dev() == lm.dev() && sm.ino() == lm.ino() {
        return Ok(metadata_change(sm, lm));
    }
    if sm.len() != lm.len() {
        return Ok(Some(How::Contents));
    }
    // Same size, different objects: the bytes have to be read. A file that
    // cannot be read on one side and can on the other is a difference; one
    // that cannot be read on either — `kiln status` run without root over a
    // mode 0600 file — is a question this cannot answer, and answering "the
    // same" would be the one wrong thing to do with it. Neither is reported
    // as a content change, and the metadata comparison below still applies.
    match (fs::read(s), fs::read(l)) {
        (Ok(a), Ok(b)) if a != b => Ok(Some(How::Contents)),
        (Ok(_), Err(_)) | (Err(_), Ok(_)) => Ok(Some(How::Contents)),
        _ => Ok(metadata_change(sm, lm)),
    }
}

fn metadata_change(sm: &fs::Metadata, lm: &fs::Metadata) -> Option<How> {
    if sm.mode() != lm.mode() {
        return Some(How::Mode);
    }
    if sm.uid() != lm.uid() || sm.gid() != lm.gid() {
        return Some(How::Owner);
    }
    None
}

fn entries(dir: &Path) -> Result<Vec<std::ffi::OsString>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        // One side of the comparison not existing is the ordinary case — it is
        // what "added" and "removed" mean — and is answered by the other
        // side's listing, not by an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                doing: "reading",
                path: dir.to_path_buf(),
                source,
            })
        }
    };
    Ok(rd.flatten().map(|e| e.file_name()).collect())
}

/// `symlink_metadata`, never `metadata`: a symlink in `/etc` pointing at a
/// file that only exists on the live system would otherwise be read as a
/// missing file on one side and a present one on the other, and reported as a
/// removal that never happened.
fn meta(p: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(p) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            doing: "reading",
            path: p.to_path_buf(),
            source,
        }),
    }
}

fn read_link(p: &Path) -> Result<std::path::PathBuf> {
    fs::read_link(p).map_err(|source| Error::Io {
        doing: "reading the symlink",
        path: p.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deployment-shaped pair of directories: `usr/etc` is what the image
    /// shipped, `etc` is what the machine has.
    struct Pair {
        dir: std::path::PathBuf,
    }

    impl Pair {
        fn new(name: &str) -> Pair {
            let dir =
                std::env::temp_dir().join(format!("kiln-drift-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("usr/etc")).unwrap();
            fs::create_dir_all(dir.join("etc")).unwrap();
            Pair { dir }
        }

        fn shipped(&self, rel: &str, content: &str) -> &Pair {
            self.write(&self.dir.join("usr/etc").join(rel), content)
        }

        fn live(&self, rel: &str, content: &str) -> &Pair {
            self.write(&self.dir.join("etc").join(rel), content)
        }

        /// The usual case: a file the image shipped and the machine still has
        /// unchanged.
        fn both(&self, rel: &str, content: &str) -> &Pair {
            self.shipped(rel, content).live(rel, content)
        }

        fn write(&self, p: &Path, content: &str) -> &Pair {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
            self
        }

        fn scan(&self) -> Vec<Change> {
            super::scan(&self.dir).unwrap()
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn an_untouched_deployment_has_no_drift() {
        let p = Pair::new("clean");
        p.both("pacman.conf", "[options]\n")
            .both("hostname", "forge\n")
            .both("systemd/logind.conf", "[Login]\n");
        assert_eq!(p.scan(), Vec::new());
    }

    #[test]
    fn a_hand_edited_file_is_the_hazard_section_2_names() {
        let p = Pair::new("edited");
        p.shipped("pacman.conf", "[options]\nParallelDownloads = 5\n")
            .live("pacman.conf", "[options]\nParallelDownloads = 20\n");
        assert_eq!(
            p.scan(),
            vec![Change::Modified {
                path: "/etc/pacman.conf".into(),
                how: How::Contents
            }]
        );
        assert!(p.scan()[0].shadows_the_image());
    }

    #[test]
    fn a_deletion_is_carried_forward_too() {
        let p = Pair::new("deleted");
        p.shipped("motd", "welcome\n");
        assert_eq!(
            p.scan(),
            vec![Change::Removed {
                path: "/etc/motd".into()
            }]
        );
    }

    #[test]
    fn a_local_file_shadows_nothing() {
        let p = Pair::new("added");
        p.live("my-notes.conf", "mine\n");
        let changes = p.scan();
        assert_eq!(
            changes,
            vec![Change::Added {
                path: "/etc/my-notes.conf".into()
            }]
        );
        assert!(!changes[0].shadows_the_image());
    }

    #[test]
    fn a_chmod_is_a_local_modification_like_any_other() {
        use std::os::unix::fs::PermissionsExt;
        let p = Pair::new("chmod");
        p.both("secret.conf", "x\n");
        fs::set_permissions(
            p.dir.join("etc/secret.conf"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_eq!(
            p.scan(),
            vec![Change::Modified {
                path: "/etc/secret.conf".into(),
                how: How::Mode
            }]
        );
    }

    #[test]
    fn a_symlink_is_compared_by_target_and_never_followed() {
        let p = Pair::new("symlink");
        // A symlink into /run: following it would read a file that does not
        // exist on either side and report a difference that is not there.
        p.shipped("localtime", "");
        fs::remove_file(p.dir.join("usr/etc/localtime")).unwrap();
        std::os::unix::fs::symlink("/usr/share/zoneinfo/UTC", p.dir.join("usr/etc/localtime"))
            .unwrap();
        std::os::unix::fs::symlink(
            "/usr/share/zoneinfo/Europe/Berlin",
            p.dir.join("etc/localtime"),
        )
        .unwrap();
        assert_eq!(
            p.scan(),
            vec![Change::Modified {
                path: "/etc/localtime".into(),
                how: How::Contents
            }]
        );
    }

    #[test]
    fn systemctl_enable_at_runtime_is_drift_and_is_reported() {
        // Kiln makes unit state image content, so a `systemctl enable` on the
        // live system is exactly the class this exists to catch: the symlink
        // wins over every future generation's preset.
        let p = Pair::new("enable");
        fs::create_dir_all(p.dir.join("usr/etc/systemd/system/multi-user.target.wants")).unwrap();
        fs::create_dir_all(p.dir.join("etc/systemd/system/multi-user.target.wants")).unwrap();
        std::os::unix::fs::symlink(
            "/usr/lib/systemd/system/sshd.service",
            p.dir
                .join("etc/systemd/system/multi-user.target.wants/sshd.service"),
        )
        .unwrap();
        assert_eq!(
            p.scan(),
            vec![Change::Added {
                path: "/etc/systemd/system/multi-user.target.wants/sshd.service".into()
            }]
        );
    }

    #[test]
    fn what_a_machine_changes_by_itself_is_not_drift() {
        let p = Pair::new("expected");
        // Every one of these differs on a correct machine that has booted once.
        p.shipped("machine-id", "").live("machine-id", "de1a…\n");
        p.shipped("passwd", "root:x:0:0::/root:/bin/bash\n").live(
            "passwd",
            "root:x:0:0::/root:/bin/bash\nyou:x:1000:1000::/home/you:/bin/fish\n",
        );
        p.live("fstab", "UUID=… / ext4 rw 0 1\n");
        p.live("kiln/system.toml", "kiln = 1\n");
        assert_eq!(p.scan(), Vec::new());
    }

    #[test]
    fn a_deployment_that_ships_no_usr_etc_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("kiln-drift-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc")).unwrap();
        assert_eq!(super::scan(&dir).unwrap(), Vec::new());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_the_image_never_had_is_one_change_not_nine() {
        let p = Pair::new("tree");
        for i in 0..9 {
            p.live(
                &format!("NetworkManager/system-connections/wifi-{i}"),
                "…\n",
            );
        }
        assert_eq!(
            p.scan(),
            vec![Change::Added {
                path: "/etc/NetworkManager".into()
            }]
        );
    }
}
