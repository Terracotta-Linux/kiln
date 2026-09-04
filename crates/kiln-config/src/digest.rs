//! Hashing local files and trees into `local_digests`.
//!
//! "Putting `local_digests` in the Manifest is why editing `files/motd` changes
//! `config_id` even though no TOML changed. Local files are free to hash, so
//! they belong to the configuration identity rather than the resolution identity."

use kiln_manifest::Hash;
use std::path::Path;

/// Directories skipped when hashing a tree.
///
/// Kiln defines a PKGBUILD's recipe hash as "blake3 of the PKGBUILD directory,
/// **ignoring VCS metadata**". Without this, a `.git` beside a PKGBUILD makes
/// the recipe hash — and so `build_key`, and so the build cache — change on
/// every commit to the repository holding it, and the cache never hits. The
/// same reasoning applies to a `[[file]]` tree, which has no business shipping
/// a `.git` into the image either.
const SKIP: &[&str] = &[".git", ".hg", ".svn", ".bzr"];

/// Hash a file or a whole tree. A tree's digest covers every path, its mode's
/// executable bit, and its contents — sorted, so the result does not depend on
/// directory iteration order.
pub fn digest(path: &Path) -> std::io::Result<Hash> {
    let mut h = blake3::Hasher::new();
    let md = std::fs::symlink_metadata(path)?;
    if md.is_dir() {
        h.update(b"tree\0");
        let mut entries = Vec::new();
        collect(path, path, &mut entries)?;
        entries.sort();
        for rel in entries {
            let full = path.join(&rel);
            h.update(rel.as_bytes());
            h.update(b"\0");
            hash_one(&full, &mut h)?;
        }
    } else {
        h.update(b"file\0");
        hash_one(path, &mut h)?;
    }
    Ok(Hash(format!("b3:{}", h.finalize().to_hex())))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let md = e.metadata()?;
        if md.is_dir() {
            if SKIP.iter().any(|s| e.file_name() == *s) {
                continue;
            }
            collect(root, &p, out)?;
        } else {
            out.push(
                p.strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok(())
}

fn hash_one(path: &Path, h: &mut blake3::Hasher) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let md = std::fs::symlink_metadata(path)?;
    if md.file_type().is_symlink() {
        h.update(b"link\0");
        h.update(std::fs::read_link(path)?.as_os_str().as_encoded_bytes());
        h.update(b"\0");
        return Ok(());
    }
    // Only the executable bit is part of identity: the rest of the mode comes
    // from the manifest's `mode` key, not from the checkout's umask.
    h.update(if md.permissions().mode() & 0o111 != 0 {
        b"x\0"
    } else {
        b"-\0"
    });
    let bytes = std::fs::read(path)?;
    h.update(&bytes.len().to_le_bytes());
    h.update(&bytes);
    Ok(())
}
