//! The canonical merged IR and `config_id`.

pub mod canon;
pub mod manifest;

pub use canon::{Canon, Canonical, Hash};
pub use manifest::*;
