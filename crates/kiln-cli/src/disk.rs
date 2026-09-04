//! One machine, one disk, so the budget is not academic.
//!
//! Three rules, all of them here so that the numbers are in one place rather
//! than spread across the commands that enforce them:
//!
//! - a build needs roughly twice the image size free, checked **before**
//!   starting rather than discovered halfway through a pacman transaction,
//! - the artifact cache budget is `min(20 GiB, 10% of the filesystem)`, not a
//!   flat number — 20 GiB is most of a small SSD and nothing on a large one,
//! - staging roots are removed on success; `--keep-failed` opts out for one
//!   build (enforced in `build.rs`).

use std::path::Path;

/// The ceiling on the artifact cache, whatever the filesystem's size.
pub const CACHE_CEILING: u64 = 20 * GIB;

/// The share of the filesystem the cache may take when the ceiling is not the
/// binding constraint.
pub const CACHE_SHARE: f64 = 0.10;

/// "a build needs roughly twice the image size free". Twice, because the
/// staging root is a full second copy of the tree and the commit writes a third
/// partial one into the repository before the staging root goes away.
pub const BUILD_HEADROOM: f64 = 2.0;

/// What a fresh Arch image with a kernel and a desktop costs, used as the
/// estimate when there is no previous generation to measure. Deliberately
/// generous: refusing a build that would have fit wastes a command, and
/// starting one that does not wastes an hour and leaves a half-written
/// transaction.
pub const ASSUMED_IMAGE: u64 = 6 * GIB;

const GIB: u64 = 1024 * 1024 * 1024;

/// Free bytes on the filesystem holding `path`, and its total size.
///
/// `None` when the path does not exist yet or the syscall fails. A disk check
/// that cannot answer must not block a build: is a courtesy that turns a
/// mid-transaction failure into a message, not a gate on whether Kiln runs.
pub fn space(path: &Path) -> Option<Space> {
    // The nearest existing ancestor: `/var/lib/kiln/build/<plan>` is asked about
    // before it is created, and `statvfs` on a path that is not there fails.
    let mut at = path;
    while !at.exists() {
        at = at.parent()?;
    }
    let c = std::ffi::CString::new(at.to_str()?).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `stat` is written only by
    // the call, which reports failure through its return value.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let unit = stat.f_frsize as u64;
    Some(Space {
        free: stat.f_bavail as u64 * unit,
        total: stat.f_blocks as u64 * unit,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// Available to an unprivileged writer — `f_bavail`, not `f_bfree`. The
    /// difference is the reserved blocks, and counting them as free is how a
    /// check passes and the write still fails.
    pub free: u64,
    pub total: u64,
}

/// The cache budget for a filesystem of this size.
pub fn cache_budget(total: u64) -> u64 {
    let share = (total as f64 * CACHE_SHARE) as u64;
    share.min(CACHE_CEILING)
}

/// What a build of roughly `image` bytes needs free.
pub fn build_needs(image: u64) -> u64 {
    (image as f64 * BUILD_HEADROOM) as u64
}

/// Human-readable, for the eye rather than for arithmetic.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One cached artifact, for the eviction order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub path: std::path::PathBuf,
    pub bytes: u64,
    /// Seconds since the epoch. Modification time rather than access time:
    /// `relatime` makes atime nearly useless for this, and a package's mtime is
    /// when it entered the cache, which is the age that matters.
    pub age: u64,
}

/// Everything in the artifact cache, oldest last — the order eviction walks.
pub fn cached(dir: &Path) -> Vec<Cached> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Cached> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(Cached {
                path: e.path(),
                bytes: meta.len(),
                age: meta
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs(),
            })
        })
        .collect();
    out.sort_by(|a, b| b.age.cmp(&a.age).then(a.path.cmp(&b.path)));
    out
}

/// Which cached artifacts to evict to get under `budget`, newest kept.
///
/// Pure, and separate from the deleting, because "which files would go" is the
/// question `--dry-run` asks and the one a test can answer without a disk.
pub fn evict(cached: &[Cached], budget: u64) -> Vec<&Cached> {
    let mut total: u64 = cached.iter().map(|c| c.bytes).sum();
    let mut out = Vec::new();
    // Oldest first, which is the end of the list.
    for entry in cached.iter().rev() {
        if total <= budget {
            break;
        }
        total -= entry.bytes;
        out.push(entry);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `min(20 GiB, 10% of the filesystem)`. The point of the pair is
    /// that neither number is right alone — 20 GiB is most of a 32 GiB disk,
    /// and 10% of a 4 TiB one is 400 GiB of packages nobody wants.
    #[test]
    fn the_cache_budget_is_the_smaller_of_the_two_rules() {
        assert_eq!(cache_budget(32 * GIB), 3 * GIB + GIB / 5);
        assert_eq!(cache_budget(4096 * GIB), CACHE_CEILING);
    }

    /// Eviction takes the oldest first and stops the moment it is under budget:
    /// a cache trim that cleared everything would make the next build download
    /// packages it had a minute ago.
    #[test]
    fn eviction_is_oldest_first_and_stops_at_the_budget() {
        let cached = vec![
            Cached {
                path: "new".into(),
                bytes: 100,
                age: 300,
            },
            Cached {
                path: "mid".into(),
                bytes: 100,
                age: 200,
            },
            Cached {
                path: "old".into(),
                bytes: 100,
                age: 100,
            },
        ];
        let evicted: Vec<&str> = evict(&cached, 150)
            .iter()
            .map(|c| c.path.to_str().unwrap())
            .collect();
        assert_eq!(evicted, ["old", "mid"]);
    }

    #[test]
    fn a_cache_already_under_budget_loses_nothing() {
        let cached = vec![Cached {
            path: "a".into(),
            bytes: 10,
            age: 1,
        }];
        assert!(evict(&cached, 100).is_empty());
    }
}
