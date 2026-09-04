//! `kiln-build` — turning recipes into packages.
//!
//! ```text
//! recipe directory ──srcinfo──► metadata ──┐
//!                                          ├──► build_key ──► cache hit?
//! resolved makedeps ───────────────────────┘                    │ no
//!                                                               ▼
//!   phase 1  fetch    network ON,  no build code beyond pkgver()
//!   phase 2  build    network OFF, sources bind-mounted from the cache
//!                                                               │
//!                                                               ▼
//!                                                    .pkg.tar.zst
//! ```
//!
//! The two-phase split exists because `makepkg` needs the network to fetch
//! sources, and giving arbitrary build scripts the network makes builds
//! unreproducible and hard to audit. A PKGBUILD that reaches for the network in
//! `build()` fails loudly, and that is a feature.

pub mod build;
pub mod cache;
pub mod key;
pub mod module;
pub mod recipe;
pub mod root;
pub mod srcinfo;

pub use build::{Builder, Realized};
pub use cache::Cache;
pub use key::{Ingredients, BUILDER_VERSION};
pub use recipe::Recipe;
pub use root::{BuildRoot, Sources};
pub use srcinfo::{Source, Srcinfo};

use kiln_manifest::{Canon, Canonical};

/// one `source=()` entry, pinned to the bytes that were fetched.
///
/// Defined here rather than beside the rest of the plan because it
/// describes an input to a *build*, and `kiln-build` needs it to compute a
/// build key. `kiln-resolve` re-exports it, so the plan still reads as one
/// vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourcePin {
    pub url: String,
    pub sha256: String,
}

impl Canonical for SourcePin {
    fn canon(&self) -> Canon {
        Canon::map([
            ("url", Canon::str(&self.url)),
            ("sha256", Canon::str(&self.sha256)),
        ])
    }
}
