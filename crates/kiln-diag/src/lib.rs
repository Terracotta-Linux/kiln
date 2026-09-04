//! Provenance, the error taxonomy, and diagnostic rendering.
//!
//! This is its own crate because error quality is a feature, and a feature with
//! no home crate decays.

pub mod diag;
pub mod report;
pub mod source;
pub mod suggest;

pub use diag::{Diag, DiagPart, Label};
pub use report::{render, render_all, Errors, ExitCode, Phase};
pub use source::{Origin, OriginMap, Provenance, SourceFile, Spanned, Src};
pub use suggest::did_you_mean;

pub type Result<T> = std::result::Result<T, Errors>;
