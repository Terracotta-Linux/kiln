//! `kiln init` — scaffold a configuration.
//!
//! Deliberately four lines plus a comment: the whole point is that a
//! complete, bootable configuration is short, and a scaffold that opens with
//! thirty commented-out keys teaches the opposite.

use crate::color;
use kiln_diag::ExitCode;
use std::path::{Path, PathBuf};

const TEMPLATE: &str = "\
# Kiln — what is inside this system's image.
# Everything here needs a new image and a reboot; nothing else belongs.

kiln = 1

include = [\"@kiln/profiles/minimal\"]

[packages]
repo = []
";

pub fn run(config: Option<&Path>) -> ExitCode {
    let dir: PathBuf = config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(kiln_config::discover::DEFAULT_CONFIG_DIR));
    // The same rule `discover::entry_point` uses, so `kiln init --config X`
    // scaffolds exactly what `kiln check --config X` would then read.
    let entry = if kiln_config::discover::names_a_file(&dir) {
        dir.clone()
    } else {
        dir.join(kiln_config::discover::ENTRY_FILE)
    };

    if entry.exists() {
        eprintln!(
            "{} {} already exists — kiln init will not overwrite it",
            color::error(),
            entry.display()
        );
        return ExitCode::Config;
    }
    if let Some(parent) = entry.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("{} cannot create {}: {e}", color::error(), parent.display());
            return ExitCode::System;
        }
    }
    if let Err(e) = std::fs::write(&entry, TEMPLATE) {
        eprintln!("{} cannot write {}: {e}", color::error(), entry.display());
        return ExitCode::System;
    }
    println!("{} {}", color::success("wrote"), entry.display());
    println!("next:  edit it, then `kiln check --offline`");
    ExitCode::Ok
}
