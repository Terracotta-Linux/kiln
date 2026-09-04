//! Step 1 of assembly: the empty staging root. step 1.
//!
//! Almost nothing happens here, and *what does not happen* is the interesting
//! part. The obvious reading of "lay out the tree" is to create the usr-merge
//! top-level symlinks first and install into them. That does not work: Arch's
//! `filesystem` package owns `/home`, `/opt`, `/srv` and `/root` as real
//! directories, and the transaction aborts on the conflict. The
//! symlinks are created in step 10 instead, after every package has had its
//! say — see `crate::toplevel`.

use crate::tree::{self, Result};
use std::path::Path;

/// Directories that are mountpoints or kernel filesystems in the finished
/// image, and stay real directories through the top-level rewrite.
///
/// `sysroot` is the one a reader will not recognize: libostree bind-mounts the
/// physical root there, and `/ostree → sysroot/ostree` dangles
/// without it.
pub const MOUNTPOINTS: &[&str] = &["boot", "dev", "mnt", "proc", "run", "sys", "sysroot", "tmp"];

/// Create the staging root: the package database directory, the mountpoints,
/// and nothing else.
///
/// `/var` is deliberately absent. The `filesystem` package creates it in step
/// 2, and creating it here first would mean the drain had to tell
/// Kiln's own empty directories apart from a package's content.
pub fn create(root: &Path) -> Result<()> {
    if root.symlink_metadata().is_ok() && !tree::entries(root)?.is_empty() {
        return Err(tree::shape(format!(
            "the staging root {} already has content; assembly builds from nothing",
            root.display()
        )));
    }
    tree::mkdir(root)?;
    tree::mkdir(&root.join(kiln_alpm::session::DB_PATH))?;
    for dir in MOUNTPOINTS {
        tree::mkdir(&root.join(dir))?;
    }
    // `create_dir_all` makes 0755 directories, which is the wrong answer for
    // exactly one of these. OSTree stores the mode, so a 0755 /tmp would be
    // committed and shipped; at boot systemd's `tmp.mount` covers it with a
    // tmpfs, which means nobody would ever notice it was wrong.
    tree::set_mode(&root.join("tmp"), 0o1777)?;
    Ok(())
}
