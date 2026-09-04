//! Things Kiln has to canonicalize by hand.
//!
//! OSTree canonicalizes mtimes to 0 and stores only mode, ownership, xattrs and
//! content checksums, which removes a large class of reproducibility problems
//! for free. What is left is here — measured in the Phase 0 spike, where two
//! consecutive builds of the same plan initially differed in 153 files, all but
//! one of them for these reasons.

use crate::tree::{self, Result};
use std::path::Path;

/// Where the package database lives in the image. Duplicated from
/// `kiln_alpm::session::DB_PATH` on purpose — that constant configures the
/// *transaction*, this one is what the image's own `pacman.conf` must say, and
/// the whole point of is that setting one without the other leaves a
/// booted system whose `pacman -Q` reports nothing.
pub const IMAGE_DB_PATH: &str = "/usr/lib/sysimage/pacman";

/// Pin `%INSTALLDATE%` in every local database entry to the epoch.
///
/// The field is the wall clock at transaction time, so two builds of the same
/// plan differ in one `desc` file per package — about 150 in a base image.
/// Pinning it is the same move as OSTree canonicalizing mtimes.
/// The cost is that `pacman -Qi` shows a meaningless install date, which is the
/// right trade for a rebuild that produces the same tree.
///
/// Returns how many records changed.
pub fn pin_install_dates(root: &Path) -> Result<usize> {
    let local = root
        .join(IMAGE_DB_PATH.trim_start_matches('/'))
        .join("local");
    let mut changed = 0;
    for package in tree::entries(&local)? {
        let desc = package.join("desc");
        let Ok(text) = std::fs::read_to_string(&desc) else {
            continue;
        };
        let pinned = pin_field(&text, "%INSTALLDATE%", "0");
        if pinned != text {
            tree::write(&desc, &pinned)?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Replace the value on the line after `field` in pacman's `desc` format, which
/// is `%NAME%` on one line and the value on the next.
///
/// Written as a pure function over the text so the parsing has a test rather
/// than a hope. A record can end immediately after the marker, and a field's
/// value can be empty; both would be easy to mishandle in place.
pub fn pin_field(text: &str, field: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        out.push_str(line);
        out.push('\n');
        if line.trim() != field {
            continue;
        }
        // Consume the current value, if there is one. A blank line means the
        // field was empty and the record has moved on, so nothing is replaced.
        match lines.peek() {
            Some(next) if !next.trim().is_empty() => {
                lines.next();
                out.push_str(value);
                out.push('\n');
            }
            _ => {}
        }
    }
    out
}

/// Truncate `/etc/machine-id`.
///
/// systemd's documented first-boot marker: empty means "allocate one on first
/// boot". A populated one makes every machine deployed from the image share an
/// identity *and* makes the build nondeterministic. Both halves
/// matter, and the identity half is the one that would go unnoticed.
///
/// The file ships mode 0444, so it is replaced rather than written through:
/// writing in place happens to work as root — which bypasses the permission
/// check — and fails everywhere else, which is a bad thing to depend on. The
/// mode is put back, because it is the mode systemd expects to find.
pub fn reset_machine_id(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let at = root.join("etc/machine-id");
    let mode = at
        .metadata()
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o444);
    tree::remove(&at)?;
    tree::write(&at, "")?;
    tree::set_mode(&at, mode)
}

/// Point the image's own `pacman.conf` at the relocated database.
///
/// Setting `DBPath` for the transaction is only half of the relocation.
/// The image ships a `pacman.conf` that still says
/// `/var/lib/pacman`, which is now empty — so `pacman -Q` on the booted system
/// reports nothing, and `kiln why` and `kiln owns` are dead on arrival.
///
/// Must run before `/etc` moves to `/usr/etc`.
///
/// An image with no `pacman.conf` has no `pacman` package, and is explicit
/// that an empty configuration produces an empty image. There is nothing to
/// point anywhere, and nothing to complain about.
pub fn point_pacman_conf_at_the_image_database(root: &Path) -> Result<bool> {
    let conf = root.join("etc/pacman.conf");
    if !conf.is_file() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&conf).map_err(tree::io("reading", &conf))?;
    let rewritten = rewrite_dbpath(&text, IMAGE_DB_PATH).ok_or_else(|| {
        tree::shape(
            "/etc/pacman.conf has no `[options]` section, so `DBPath` could not be set; \
             the booted image's package database would appear empty",
        )
    })?;
    tree::write(&conf, &rewritten)?;
    Ok(true)
}

/// Set `DBPath` in a pacman.conf, replacing an existing setting — commented or
/// not — or inserting one under `[options]`. `None` if there is nowhere to put
/// it, which is a broken config rather than something to paper over.
pub fn rewrite_dbpath(text: &str, dbpath: &str) -> Option<String> {
    let setting = format!("DBPath     = {dbpath}\n");
    let mut out = String::with_capacity(text.len() + setting.len());
    let mut placed = false;

    for line in text.lines() {
        let bare = line.trim_start().trim_start_matches('#').trim_start();
        if bare.starts_with("DBPath") {
            // Replace the first, drop any others: two DBPath lines would leave
            // which one wins up to pacman's parser.
            if !placed {
                out.push_str(&setting);
                placed = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if !placed && bare.starts_with("[options]") {
            out.push_str(&setting);
            placed = true;
        }
    }
    placed.then_some(out)
}

/// Drop the synced repository databases from the image.
///
/// They are *resolution* state — a snapshot of what the mirrors held at build
/// time — not image content, and they are large. `local/` stays: that is the
/// installed-package database `pacman -Q`, `kiln why` and `kiln owns` read
/// offline from the booted system.
pub fn drop_sync_databases(root: &Path) -> Result<u64> {
    let sync = root
        .join(IMAGE_DB_PATH.trim_start_matches('/'))
        .join("sync");
    if !sync.exists() {
        return Ok(0);
    }
    let size = tree::tree_size(&sync);
    tree::remove(&sync)?;
    Ok(size)
}
