//! The build cache.
//!
//! The strategy is to cache aggressively at the package level and rebuild the
//! tree from scratch every time. Being aggressive is only safe because deleting
//! the cache costs time and never correctness — so the failure modes that would
//! break *that* property are what these tests are about.

use kiln_build::cache::{Cache, Lookup};
use kiln_manifest::Hash;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/test-roots")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn artifact(dir: &Path, name: &str) -> PathBuf {
    let at = dir.join(name);
    std::fs::write(&at, "not really a package\n").unwrap();
    at
}

const KEY: &str = "b3:cc41aa";

#[test]
fn an_unbuilt_key_misses() {
    let dir = scratch("cache-miss");
    assert_eq!(Cache::new(&dir).lookup(&Hash(KEY.into())), Lookup::Miss);
}

#[test]
fn a_stored_artifact_hits_and_comes_back() {
    let dir = scratch("cache-hit");
    let cache = Cache::new(&dir);
    let built = artifact(&dir, "mytool-1.2.0-3-x86_64.pkg.tar.zst");

    let stored = cache.store(&Hash(KEY.into()), &[built]).unwrap();
    assert_eq!(stored.len(), 1);

    match cache.lookup(&Hash(KEY.into())) {
        Lookup::Hit(paths) => {
            assert_eq!(paths.len(), 1);
            assert!(paths[0]
                .to_string_lossy()
                .ends_with("mytool-1.2.0-3-x86_64.pkg.tar.zst"));
            assert!(paths[0].is_file());
        }
        Lookup::Miss => panic!("a stored artifact must hit"),
    }
}

/// A split package builds several artifacts at once, and all of them belong to
/// the one key.
#[test]
fn a_split_package_stores_every_artifact_under_one_key() {
    let dir = scratch("cache-split");
    let cache = Cache::new(&dir);
    let built = vec![
        artifact(&dir, "mytool-1.2.0-3-x86_64.pkg.tar.zst"),
        artifact(&dir, "mytool-docs-1.2.0-3-x86_64.pkg.tar.zst"),
    ];
    cache.store(&Hash(KEY.into()), &built).unwrap();

    match cache.lookup(&Hash(KEY.into())) {
        Lookup::Hit(paths) => assert_eq!(paths.len(), 2),
        Lookup::Miss => panic!("both artifacts belong to the one key"),
    }
}

/// The failure this cache would otherwise have: an interrupted build leaves the
/// entry directory behind, and a lookup that treated "the directory exists" as
/// a hit would answer "yes, zero artifacts" and silently drop a package from
/// the image.
#[test]
fn an_empty_entry_is_a_miss_not_a_hit_with_nothing_in_it() {
    let dir = scratch("cache-interrupted");
    let cache = Cache::new(&dir);
    std::fs::create_dir_all(dir.join("cache/build/cc41aa")).unwrap();

    assert_eq!(cache.lookup(&Hash(KEY.into())), Lookup::Miss);
}

/// …and the same for a directory holding something that is not a package.
#[test]
fn a_directory_of_debris_is_a_miss() {
    let dir = scratch("cache-debris");
    let cache = Cache::new(&dir);
    let entry = dir.join("cache/build/cc41aa");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("build.log"), "==> ERROR: ...\n").unwrap();

    assert_eq!(cache.lookup(&Hash(KEY.into())), Lookup::Miss);
}

/// Storing again replaces cleanly rather than accumulating: two runs of the
/// same key must not leave the artifacts of both.
#[test]
fn storing_twice_leaves_one_set_of_artifacts() {
    let dir = scratch("cache-restore");
    let cache = Cache::new(&dir);
    cache
        .store(
            &Hash(KEY.into()),
            &[artifact(&dir, "a-1-1-x86_64.pkg.tar.zst")],
        )
        .unwrap();
    cache
        .store(
            &Hash(KEY.into()),
            &[artifact(&dir, "b-1-1-x86_64.pkg.tar.zst")],
        )
        .unwrap();

    match cache.lookup(&Hash(KEY.into())) {
        Lookup::Hit(paths) => {
            assert_eq!(paths.len(), 1);
            assert!(paths[0].to_string_lossy().contains("b-1-1"));
        }
        Lookup::Miss => panic!("the second store must be a hit"),
    }
}

/// the log path is printed on failure, so it is part of the interface.
#[test]
fn the_log_path_is_named_after_the_build_key() {
    let dir = scratch("cache-log");
    let path = Cache::new(&dir).log_path(&Hash(KEY.into()));
    assert_eq!(path, dir.join("logs/cc41aa.log"));
}

/// The `b3:` prefix is how Kiln *prints* a hash. A colon in a directory name is
/// a needless surprise for anyone looking around /var/lib/kiln with a shell.
#[test]
fn cache_directories_do_not_carry_the_hash_prefix() {
    let dir = scratch("cache-naming");
    Cache::new(&dir)
        .store(
            &Hash(KEY.into()),
            &[artifact(&dir, "a-1-1-x86_64.pkg.tar.zst")],
        )
        .unwrap();
    assert!(dir.join("cache/build/cc41aa").is_dir());
}
