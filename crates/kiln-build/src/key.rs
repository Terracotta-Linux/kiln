//! `build_key`.
//!
//! ```text
//! build_key = blake3(
//!     recipe_tree_hash          # blake3 of the PKGBUILD directory, ignoring VCS metadata
//!   ⧺ sorted(source_pins)       # sha256 of every fetched source
//!   ⧺ sorted(makedep_evrs)      # exact versions of the build-time dependency closure
//!   ⧺ target_arch
//!   ⧺ kiln_builder_version      # bumped when sandbox/toolchain semantics change
//! )
//! ```
//!
//! A hit returns the cached `.pkg.tar.zst` and skips the build entirely — the
//! single largest speed win in the system, because rebuilding an image after
//! changing one line of `system.toml` must not rebuild an out-of-tree NVIDIA
//! module.
//!
//! `makedep_evrs` is what makes the cache **correct** rather than merely fast.
//! A package built against `gcc 15.1` is not the same artifact as one built
//! against `gcc 15.2`, and a cache that pretends otherwise produces the worst
//! class of bug this project can have: an artifact that is silently wrong,
//! reproducibly, on one machine only.

use crate::SourcePin;
use kiln_manifest::{Canon, Canonical, Hash};

/// Bumped when the sandbox or toolchain semantics change in a way that makes
/// previously-cached artifacts untrustworthy — a different build user, a
/// different set of bind mounts, a different `makepkg` invocation.
///
/// Separate from `HASH_EPOCH`, and deliberately so: that one invalidates every
/// *identity*, this one invalidates every cached *artifact*. Changing how a
/// build runs should not force every image to rebuild, and changing what a
/// plan means should not throw away hours of compilation.
pub const BUILDER_VERSION: u32 = 1;

/// Everything that decides whether two builds produce the same artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingredients {
    /// blake3 of the recipe directory, VCS metadata excluded. This is
    /// exactly the digest the frontend already put in `local_digests`.
    pub recipe: Hash,
    pub sources: Vec<SourcePin>,
    /// `name-evr` for the whole build-time dependency closure, as resolved.
    pub makedeps: Vec<String>,
    pub arch: String,
    /// `None` for an ordinary package; the resolved kernel EVR for an
    /// out-of-tree module, which is what makes "rebuild modules when the kernel
    /// changes" fall out of the cache rather than being a special case.
    pub kernel_evr: Option<String>,
}

impl Ingredients {
    pub fn new(recipe: Hash, arch: impl Into<String>) -> Ingredients {
        Ingredients {
            recipe,
            sources: Vec::new(),
            makedeps: Vec::new(),
            arch: arch.into(),
            kernel_evr: None,
        }
    }

    pub fn with_sources(mut self, sources: Vec<SourcePin>) -> Ingredients {
        self.sources = sources;
        self
    }

    pub fn with_makedeps(mut self, makedeps: Vec<String>) -> Ingredients {
        self.makedeps = makedeps;
        self
    }

    pub fn against_kernel(mut self, evr: impl Into<String>) -> Ingredients {
        self.kernel_evr = Some(evr.into());
        self
    }

    /// The key. Sorted before hashing, so the order the resolver happened to
    /// walk a dependency closure in cannot change an artifact's identity.
    pub fn build_key(&self) -> Hash {
        let mut sources = self.sources.clone();
        sources.sort();
        let mut makedeps = self.makedeps.clone();
        makedeps.sort();
        makedeps.dedup();

        Hash::of(
            &Canon::map([
                ("builder", Canon::Int(BUILDER_VERSION as i64)),
                ("recipe", self.recipe.canon()),
                ("sources", sources.canon()),
                ("makedeps", makedeps.canon()),
                ("arch", Canon::str(&self.arch)),
                (
                    "kernel_evr",
                    Canon::opt(self.kernel_evr.as_ref().map(Canon::str)),
                ),
            ])
            .to_bytes(),
        )
    }
}
