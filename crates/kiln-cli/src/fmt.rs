//! Small shared renderers.
//!
//! Every one of these was written out once per command before it lived here,
//! and the copies had quietly drifted: the same digest printed at six, eight
//! and twelve characters depending on which command you asked. None of that
//! was a decision anybody made, so there is one of each now.

use kiln_diag::ExitCode;
use serde::Serialize;
use std::path::Path;

/// `--json` on every inspection command prints one value this way: pretty,
/// newline-terminated, and always `ExitCode::Ok` — a value that serialized is
/// a value the caller asked a real question about and got a real answer to.
/// The unformatted, human-only failure paths (a missing sysroot, an unknown
/// generation) are unchanged by `--json`; a script that wants a structured
/// error parses `kiln`'s exit code, not stderr.
pub fn json(value: &impl Serialize) -> ExitCode {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serializable by construction")
    );
    ExitCode::Ok
}

/// A digest abbreviated for reading.
///
/// A `b3:` digest keeps its prefix and eight hex characters, the same as
/// `kiln_manifest::Hash::short`; anything else — an OSTree checksum, a
/// `sha256` — gets twelve, which is the width `ostree` itself prints.
pub fn hash(s: &str) -> String {
    match s.strip_prefix("b3:") {
        Some(hex) => format!("b3:{}", &hex[..hex.len().min(8)]),
        None => s.chars().take(12).collect(),
    }
}

/// A git revision, abbreviated. Shorter than a digest on purpose: eight is
/// what a reader pastes into `git show`, and an AUR commit is the one hash in
/// Kiln's output that a person goes and looks up somewhere else.
pub fn commit(s: &str) -> String {
    s.chars().take(8).collect()
}

/// A size for the eye rather than for arithmetic: a locale archive is the
/// reason this is printed at all, and `19293696` does not read as "that is
/// most of what this script did".
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The `s` in "3 packages".
pub fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// "1 package" / "3 packages".
pub fn counted(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", plural(n))
}

/// `cp -a`, for the two commands that stage a configuration tree into a
/// scratch directory before handing it to a build.
///
/// Shelling out rather than walking the tree: `-a` is hardlinks, sparse files,
/// device nodes, xattrs and timestamps, and a hand-written copy that gets one
/// of them wrong produces a build that differs from the one the user tested.
pub fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let out = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .output()
        .map_err(|e| format!("running cp: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "copying {}: {}",
        from.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_keeps_its_prefix_and_a_checksum_does_not() {
        assert_eq!(hash("b3:7f2a3b4c5d6e7f80"), "b3:7f2a3b4c");
        assert_eq!(hash(&"a".repeat(64)), "aaaaaaaaaaaa");
    }

    /// Short inputs must not panic: `&s[..8]` on a six-character hash is the
    /// bug this replaced.
    #[test]
    fn a_short_input_is_returned_whole() {
        assert_eq!(hash("b3:abc"), "b3:abc");
        assert_eq!(hash("abc"), "abc");
        assert_eq!(commit("abc"), "abc");
    }

    #[test]
    fn sizes_read_as_sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(19_293_696), "18.4 MiB");
    }

    #[test]
    fn one_is_singular() {
        assert_eq!(counted(1, "package"), "1 package");
        assert_eq!(counted(0, "package"), "0 packages");
    }
}
