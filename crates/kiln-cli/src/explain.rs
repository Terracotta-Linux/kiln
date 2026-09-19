//! `kiln explain <key>` — an alias for `kiln config get <key>`. `explain`
//! reads better in prose; `config get` exists because `get`/`list`/`set`/
//! `unset`/`add`/`remove` belong under one verb. Both call the same
//! implementation in `config.rs` and produce identical output.

pub fn run(fe: &kiln_config::Frontend, key: &str) -> kiln_diag::ExitCode {
    crate::config::get(fe, key, false)
}
