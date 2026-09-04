//! Reading BLS entries.
//!
//! This module exists for one reason, and it is a bug that already happened.
//!
//! libostree writes `ostree-1.conf`, `ostree-2.conf`, … into
//! `/boot/loader/entries`. The entry that *boots* is the one with the highest
//! BLS `version` field — which is the highest-numbered file. Sorting entries by
//! filename and taking the first one therefore selects the **rollback**
//! deployment. That cost one wrong boot in the phase 0 spike, and it would cost
//! more in a boot-acceptance test that silently asserted against the wrong
//! image.
//!
//! Pure, and separate from libostree, because the whole point is that this
//! decision can be checked without booting anything.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file's name, e.g. `ostree-2.conf`.
    pub filename: String,
    /// BLS `version`. **This is the sort key**, not the filename.
    pub version: i64,
    pub title: String,
    pub options: String,
    pub linux: String,
}

/// Parse one BLS entry file. The format is `key value` per line, with the value
/// running to end of line.
pub fn parse(filename: &str, text: &str) -> Entry {
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(char::is_whitespace) {
            fields.entry(key).or_insert_with(|| value.trim());
        }
    }
    Entry {
        filename: filename.to_string(),
        version: fields
            .get("version")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        title: fields.get("title").unwrap_or(&"").to_string(),
        options: fields.get("options").unwrap_or(&"").to_string(),
        linux: fields.get("linux").unwrap_or(&"").to_string(),
    }
}

/// Every entry in `<boot>/loader/entries`, **in boot order** — highest BLS
/// `version` first, which is the one the firmware will start.
pub fn read(boot: &Path) -> Vec<Entry> {
    let dir = boot.join("loader/entries");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "conf"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            Some(parse(&e.file_name().to_string_lossy(), &text))
        })
        .collect();
    entries.sort_by(|a, b| b.version.cmp(&a.version).then(a.filename.cmp(&b.filename)));
    entries
}

/// What the machine will boot next.
pub fn default(boot: &Path) -> Option<Entry> {
    read(boot).into_iter().next()
}
