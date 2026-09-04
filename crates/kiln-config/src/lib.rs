//! The Kiln frontend: discovery → parse → include graph → merge → validate.
//!
//! ```text
//! system.toml ──parse──► Node (spanned)
//!      │                   │
//!   include            structure
//!      ▼                   ▼
//!   Unit tree ──merge──► Node + OriginMap ──validate──► Manifest ⇒ config_id
//! ```
//!
//! Every stage reports all of its problems before the next one starts,
//! and every value keeps the file and byte range it came
//! from all the way through.

pub mod digest;
pub mod discover;
pub mod include;
pub mod merge;
pub mod node;
pub mod schema;
pub mod shorthand;
pub mod structure;
pub mod validate;

use kiln_diag::{Diag, Errors, Src};
use kiln_manifest::Manifest;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct Options {
    /// escaping the config root requires this, and warns.
    pub allow_external_sources: bool,
    /// Overrides `/usr/share/kiln/modules`; used by tests and by an installer
    /// building against a target root.
    pub module_root: Option<PathBuf>,
}

/// Everything the frontend produced, including what it needs to explain itself.
pub struct Frontend {
    pub manifest: Manifest,
    pub merged: merge::Merged,
    /// Every file that participated, entry point first.
    pub files: Vec<Src>,
    pub config_root: PathBuf,
    /// Non-fatal problems: escaped sources, and anything else worth saying.
    pub warnings: Errors,
}

pub fn load(config: Option<&Path>, opts: &Options) -> Result<Frontend, Errors> {
    let entry = discover::entry_point(config).map_err(one)?;

    let mut loader = discover::Loader::new(entry.config_root.clone())
        .allow_external(opts.allow_external_sources);
    if let Some(m) = &opts.module_root {
        loader = loader.with_module_root(m.clone());
    }

    let unit = include::load(&mut loader, entry.path)?;
    let files = unit.files();
    let merged = merge::merge(&unit)?;
    let (manifest, notes) = validate::validate(&merged, &mut loader)?;

    let mut warnings = notes;
    for (path, at) in &loader.escapes {
        warnings.push(
            Diag::warning(
                "kiln::security",
                "a source outside the config root was used",
            )
            .label(at, format!("resolves to {}", path.display()))
            .help(
                "--allow-external-sources was given; this input is not covered by the \
                       config root's guarantee that a build is self-contained",
            ),
        );
    }

    Ok(Frontend {
        manifest,
        merged,
        files,
        config_root: entry.config_root,
        warnings,
    })
}

fn one(d: Diag) -> Errors {
    let mut e = Errors::new();
    e.push(d);
    e
}
