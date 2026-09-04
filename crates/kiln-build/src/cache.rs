//! The build cache.
//!
//! Content-addressed by `build_key`, so an artifact shared between generations
//! costs disk once, and a rebuild of an unchanged recipe costs nothing at all.
//! The strategy: cache aggressively at the package level, rebuild the tree
//! from scratch every time.
//!
//! Deleting the cache costs time, never correctness — that is the property that
//! makes it safe to be aggressive here.

use kiln_manifest::Hash;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

/// What a cache lookup found, kept distinct from `Option` so that callers — and
/// `kiln build -v` — can say which happened rather than inferring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The artifacts are already built, and here they are.
    Hit(Vec<PathBuf>),
    Miss,
}

impl Cache {
    /// `<state_dir>/cache/build`.
    pub fn new(state_dir: impl AsRef<Path>) -> Cache {
        Cache {
            root: state_dir.as_ref().join("cache/build"),
        }
    }

    fn entry(&self, key: &Hash) -> PathBuf {
        // The `b3:` prefix is stripped: it is how Kiln *prints* a hash, and a
        // colon in a path name is a needless surprise for anyone poking around
        // in /var/lib/kiln with a shell.
        self.root.join(key.0.strip_prefix("b3:").unwrap_or(&key.0))
    }

    /// Has this exact build already happened?
    ///
    /// A directory with no packages in it counts as a **miss**, not a hit: an
    /// interrupted build can leave the directory behind, and returning "yes,
    /// zero artifacts" would silently drop a package from the image.
    pub fn lookup(&self, key: &Hash) -> Lookup {
        let dir = self.entry(key);
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".pkg.tar.zst"))
            .collect();
        found.sort();
        if found.is_empty() {
            Lookup::Miss
        } else {
            Lookup::Hit(found)
        }
    }

    /// Move finished artifacts into the cache under `key`.
    ///
    /// Assembled in a temporary directory and renamed into place, so a crash
    /// mid-store leaves a miss rather than a half-populated hit — which the
    /// lookup above would otherwise happily serve.
    pub fn store(&self, key: &Hash, artifacts: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
        let final_dir = self.entry(key);
        if artifacts.is_empty() {
            return Ok(Vec::new());
        }
        let staging = final_dir.with_extension("incoming");
        std::fs::remove_dir_all(&staging).ok();
        std::fs::create_dir_all(&staging)?;

        for artifact in artifacts {
            let name = artifact
                .file_name()
                .ok_or_else(|| std::io::Error::other(format!("{artifact:?} has no file name")))?;
            std::fs::copy(artifact, staging.join(name))?;
        }

        std::fs::remove_dir_all(&final_dir).ok();
        std::fs::rename(&staging, &final_dir)?;
        Ok(self.expect_hit(key))
    }

    fn expect_hit(&self, key: &Hash) -> Vec<PathBuf> {
        match self.lookup(key) {
            Lookup::Hit(paths) => paths,
            Lookup::Miss => Vec::new(),
        }
    }

    /// Where a build's log goes, whether or not it succeeded. The path
    /// is printed on failure, so it is part of the interface rather than an
    /// implementation detail.
    pub fn log_path(&self, key: &Hash) -> PathBuf {
        self.root
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&self.root)
            .join("logs")
            .join(format!(
                "{}.log",
                key.0.strip_prefix("b3:").unwrap_or(&key.0)
            ))
    }
}
