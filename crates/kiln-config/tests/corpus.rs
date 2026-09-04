//! Snapshot tests over `tests/corpus/`.
//!
//! The invalid half matters more than the valid half: it snapshots the
//! *rendered diagnostics*, because "diagnostics that nobody tests rot into
//! `Error: InvalidConfig`".

use kiln_config::Options;
use kiln_manifest::Canonical;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/kiln-config has a workspace root")
        .to_path_buf()
}

fn opts() -> Options {
    Options {
        allow_external_sources: false,
        module_root: Some(repo_root().join("modules")),
    }
}

fn cases(kind: &str) -> Vec<(String, PathBuf)> {
    let dir = repo_root().join("tests/corpus").join(kind);
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    out
}

/// Redact the repository's own path *before* the diagnostic is rendered.
///
/// An absolute path reaches a diagnostic as text, and `render` wraps text at a
/// fixed width — so the length of the checkout's path decides where a help line
/// breaks. A snapshot taken in `~/Projects/kiln` then fails in
/// `~/build/kiln/src/kiln`, which is exactly where `makepkg` runs the suite.
/// Filtering the *rendered* string, as `settings()` also does, is too late: the
/// wrap has already happened.
fn redacted(errs: &kiln_diag::Errors) -> kiln_diag::Errors {
    let root = repo_root().to_string_lossy().into_owned();
    let fix = |s: &str| s.replace(&root, "[ROOT]");
    kiln_diag::Errors {
        diags: errs
            .diags
            .iter()
            .map(|d| kiln_diag::Diag {
                message: fix(&d.message),
                help: d.help.as_deref().map(&fix),
                labels: d
                    .labels
                    .iter()
                    .map(|l| kiln_diag::Label {
                        origin: l.origin.clone(),
                        text: fix(&l.text),
                    })
                    .collect(),
                ..d.clone()
            })
            .collect(),
    }
}

/// Absolute paths that reach a snapshot some other way — through the canonical
/// manifest dump — and the host architecture would otherwise make snapshots
/// machine-specific.
fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.add_filter(&regex_escape(&repo_root().to_string_lossy()), "[ROOT]");
    // Only the manifest's own arch field, not every occurrence of the string.
    s.add_filter(
        &format!("arch: \"{}\"", kiln_manifest::host_arch()),
        "arch: \"[ARCH]\"",
    );
    s.set_prepend_module_to_snapshot(false);
    s
}

fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if "\\.+*?()|[]{}^$".contains(c) {
                format!("\\{c}")
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[test]
fn valid_configs_produce_a_stable_manifest() {
    for (name, dir) in cases("valid") {
        let fe = match kiln_config::load(Some(&dir), &opts()) {
            Ok(fe) => fe,
            Err(errs) => panic!(
                "corpus/valid/{name} did not load:\n{}",
                kiln_diag::render_all(&redacted(&errs))
            ),
        };
        // Warnings are part of what a valid configuration produces, and seeded
        // targets are *only* observable through one. A note nobody
        // snapshots rots exactly the way a diagnostic nobody snapshots does.
        let warnings = if fe.warnings.diags.is_empty() {
            String::new()
        } else {
            format!("{}\n", kiln_diag::render_all(&redacted(&fe.warnings)))
        };
        let rendered = format!(
            "config_id: {}\n\n{warnings}{}\n",
            fe.manifest.config_id(),
            render_canon(&fe.manifest.canon(), 0)
        );
        settings().bind(|| insta::assert_snapshot!(format!("valid__{name}"), rendered));
    }
}

#[test]
fn invalid_configs_produce_the_diagnostics_we_promised() {
    for (name, dir) in cases("invalid") {
        let rendered = match kiln_config::load(Some(&dir), &opts()) {
            Ok(_) => panic!("corpus/invalid/{name} was accepted, but must not be"),
            Err(errs) => kiln_diag::render_all(&redacted(&errs)),
        };
        settings().bind(|| insta::assert_snapshot!(format!("invalid__{name}"), rendered));
    }
}

/// "Reordering lines in a TOML file must never change `config_id`."
#[test]
fn reordering_a_file_does_not_change_config_id() {
    let root = repo_root().join("tests/corpus/valid");
    let a = kiln_config::load(Some(&root.join("order-independence-a")), &opts()).unwrap();
    let b = kiln_config::load(Some(&root.join("order-independence-b")), &opts()).unwrap();
    assert_eq!(
        a.manifest.config_id(),
        b.manifest.config_id(),
        "two files differing only in line order hashed differently"
    );
}

/// A readable dump of the canonical value, so a snapshot diff shows *what*
/// changed rather than only that the hash moved.
fn render_canon(c: &kiln_manifest::Canon, indent: usize) -> String {
    use kiln_manifest::Canon;
    let pad = "  ".repeat(indent);
    match c {
        Canon::Str(s) => format!("{s:?}"),
        Canon::Int(i) => i.to_string(),
        Canon::Bool(b) => b.to_string(),
        Canon::List(items) if items.is_empty() => "[]".into(),
        Canon::List(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|i| format!("{pad}  - {}", render_canon(i, indent + 1)))
                .collect();
            format!("\n{}", inner.join("\n"))
        }
        Canon::Map(m) if m.is_empty() => "{}".into(),
        Canon::Map(m) => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{pad}  {k}: {}", render_canon(v, indent + 1)))
                .collect();
            format!("\n{}", inner.join("\n"))
        }
    }
}
