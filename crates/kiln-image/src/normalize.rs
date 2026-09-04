//! Step 10: the OSTree contract.
//!
//! The order in `run` is not arbitrary and every step depends on the one before
//! it:
//!
//! 1. `pacman.conf` and `machine-id` are fixed *while they are still in
//!    `/etc`* (findings N6 and D2).
//! 2. `/root`, `/home`, `/opt` and `/srv` are moved into `/var`.
//!    They become symlinks into `/var` in step 5, so anything a package left in
//!    them is `/var` content and must go through the *same* drain rather than a
//!    second one.
//! 3. The `/var` drain runs, over everything including what step 2 just moved.
//! 4. `/etc` becomes `/usr/etc`.
//! 5. The top level is rewritten: the usr-merge symlinks that could not be laid
//!    down in step 1 of assembly, because `filesystem` owns those directories
//!    as real ones.
//!
//! Doing 2 after 3 leaves `/home` pointing at nothing. Doing 4 before 1 means
//! the booted image's `pacman -Q` reports nothing and every machine deployed
//! from it shares a machine-id.

use crate::tree::{self, Result};
use crate::{determinism, drain, verify};
use std::path::Path;

/// Where each top-level directory that becomes a symlink is moved to. The
/// destination names are `/var`'s, not `/`'s: `/root` is `/var/roothome`,
/// because `/var/root` would be a confusing name for a home directory.
pub const INTO_VAR: &[(&str, &str)] = &[
    ("root", "var/roothome"),
    ("home", "var/home"),
    ("opt", "var/opt"),
    ("srv", "var/srv"),
];

#[derive(Debug, Default)]
pub struct Report {
    /// `(from, to)` for each entry moved out of a soon-to-be-symlink.
    pub relocated: Vec<(String, String)>,
    pub drain: drain::Plan,
    pub pinned_install_dates: usize,
    pub dropped_sync_bytes: u64,
}

/// Normalize the staging root into something OSTree can commit.
pub fn run(root: &Path) -> Result<Report> {
    let mut report = Report::default();

    // Still in /etc, and only reachable there.
    determinism::point_pacman_conf_at_the_image_database(root)?;
    determinism::reset_machine_id(root)?;
    report.pinned_install_dates = determinism::pin_install_dates(root)?;
    report.dropped_sync_bytes = determinism::drop_sync_databases(root)?;

    report.relocated = relocate_into_var(root)?;

    report.drain = drain::plan(root)?;
    drain::apply(root, &report.drain)?;

    move_etc(root)?;
    rewrite_top_level(root)?;
    Ok(report)
}

/// Move the contents of the directories that become symlinks into
/// the `/var` paths they will point at, so there is exactly one code path
/// deciding the fate of persistent state.
///
/// A base image already has `/srv/ftp` and `/srv/http`; without this they are
/// silently deleted along with the directory.
pub fn relocate_into_var(root: &Path) -> Result<Vec<(String, String)>> {
    let mut moved = Vec::new();
    for (from, to) in INTO_VAR {
        let source = root.join(from);
        match source.symlink_metadata() {
            // Already a symlink: a previous normalization, or a package that
            // shipped it that way. Nothing to relocate, and following it would
            // walk into /var and move its contents onto themselves.
            Ok(md) if !md.file_type().is_dir() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let dest = root.join(to);
        tree::mkdir(&dest)?;
        for entry in tree::entries(&source)? {
            let name = entry.file_name().expect("a listed entry has a name");
            let target = dest.join(name);
            if target.symlink_metadata().is_ok() {
                // The package that owns `/var/opt` and the package that owns
                // `/opt` are usually the same one and usually agree, but a
                // collision here would be one directory silently swallowing
                // another's contents.
                return Err(tree::shape(format!(
                    "/{from}/{} and /{to}/{} both exist; Kiln will not merge them",
                    name.to_string_lossy(),
                    name.to_string_lossy()
                )));
            }
            std::fs::rename(&entry, &target).map_err(tree::io("relocating", &entry))?;
            moved.push((
                format!("/{from}/{}", name.to_string_lossy()),
                format!("/{to}/{}", name.to_string_lossy()),
            ));
        }
        tree::remove(&source)?;
    }
    Ok(moved)
}

/// Move the whole directory: everything Kiln or a package put in `/etc`
/// becomes the shipped default, and libostree 3-way-merges it against the
/// machine's own `/etc` at deploy time.
pub fn move_etc(root: &Path) -> Result<()> {
    let etc = root.join("etc");
    if !etc.is_dir() {
        // An image with no /etc at all is not impossible — it is an image with
        // no packages — and it is not this function's business to object.
        return Ok(());
    }
    let dest = root.join("usr/etc");
    if dest.symlink_metadata().is_ok() {
        return Err(tree::shape(
            "/usr/etc already exists before /etc was moved: something wrote there directly, \
             and the move would have to merge two sets of shipped defaults"
                .to_string(),
        ));
    }
    tree::mkdir(&root.join("usr"))?;
    std::fs::rename(&etc, &dest).map_err(tree::io("moving /etc to /usr/etc at", &etc))
}

/// Normalization work deferred from assembly step 1. Remove the four directories whose
/// contents `relocate_into_var` already moved, and lay down the top level the
/// image actually ships.
pub fn rewrite_top_level(root: &Path) -> Result<()> {
    for (link, target) in verify::TOP_LEVEL_LINKS {
        // Relative targets, the way Arch's own usr-merge links are written: a
        // symlink read from *outside* the deployment — which is exactly how the
        // installer and libostree see it — must not resolve to the builder's
        // `/usr/bin`.
        //
        // `bin`, `sbin`, `lib` and `lib64` are already symlinks on a usr-merged
        // Arch; `home`, `root`, `opt` and `srv` are real directories that
        // `relocate_into_var` has emptied. `tree::symlink` replaces either.
        tree::symlink(target, &root.join(link))?;
    }
    for dir in verify::TOP_LEVEL_DIRS {
        tree::mkdir(&root.join(dir))?;
    }
    tree::set_mode(&root.join("tmp"), 0o1777)
}
