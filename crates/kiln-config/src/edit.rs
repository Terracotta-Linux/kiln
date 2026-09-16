//! Write-back for `kiln config set/unset/add/remove`.
//!
//! Pure text-in, text-out: no include graph, no CLI concerns, no knowledge of
//! which file is "the right one" to edit — that policy lives in `kiln-cli`,
//! which has the provenance to decide it. This module only knows how to
//! change one file's TOML text without disturbing anything it did not touch.
//!
//! `toml_edit::DocumentMut` is used here rather than the read-only
//! `ImDocument` parse `node.rs` uses for the frontend: it preserves
//! formatting and comments across an edit, which is the entire point of
//! editing a config file with a tool instead of retyping it.

use crate::schema::Ty;
use kiln_diag::Diag;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

fn parse(text: &str) -> Result<DocumentMut, Diag> {
    text.parse::<DocumentMut>()
        .map_err(|e| Diag::error("kiln::config", format!("invalid TOML: {e}")))
}

/// Walk to the table holding a dotted key's last segment, creating implicit
/// tables along the way. Used by the two mutations that must be able to
/// write into a file with nothing under that path yet (`set`, `add`).
fn table_for<'a>(
    doc: &'a mut DocumentMut,
    dotted: &'a str,
) -> Result<(&'a mut Table, &'a str), Diag> {
    let mut segs: Vec<&str> = dotted.split('.').collect();
    let last = segs.pop().expect("a dotted key has at least one segment");
    let mut cur = doc.as_table_mut();
    for seg in segs {
        let entry = cur.entry(seg).or_insert_with(|| Item::Table(Table::new()));
        cur = entry.as_table_mut().ok_or_else(|| {
            Diag::error(
                "kiln::config",
                format!("`{seg}` in `{dotted}` is already a value, not a table"),
            )
        })?;
    }
    Ok((cur, last))
}

/// The same walk, but read/write without creating anything: used by `unset`
/// and `remove`, which must be a no-op — not a structural change — when the
/// path is not there at all.
fn find_table_mut<'a>(
    doc: &'a mut DocumentMut,
    dotted: &'a str,
) -> Option<(&'a mut Table, &'a str)> {
    let mut segs: Vec<&str> = dotted.split('.').collect();
    let last = segs.pop()?;
    let mut cur = doc.as_table_mut();
    for seg in segs {
        cur = cur.get_mut(seg)?.as_table_mut()?;
    }
    Some((cur, last))
}

fn scalar_value(dotted: &str, raw: &str, ty: Ty) -> Result<Value, Diag> {
    Ok(match ty {
        Ty::Str => Value::from(raw),
        Ty::Int => Value::from(raw.parse::<i64>().map_err(|_| {
            Diag::error(
                "kiln::config",
                format!("`{dotted}` takes an integer, not `{raw}`"),
            )
        })?),
        Ty::Bool => Value::from(raw.parse::<bool>().map_err(|_| {
            Diag::error(
                "kiln::config",
                format!("`{dotted}` takes true or false, not `{raw}`"),
            )
        })?),
        Ty::Table => {
            return Err(Diag::error(
                "kiln::config",
                format!("`{dotted}` is a table; `kiln config set` only writes scalars"),
            ))
        }
    })
}

/// Set a scalar key, replacing whatever was there. Errors if the key is
/// currently a table — `kiln config set` never turns a section into a value.
pub fn set_scalar(text: &str, dotted: &str, raw_value: &str, ty: Ty) -> Result<String, Diag> {
    let mut doc = parse(text)?;
    let value = scalar_value(dotted, raw_value, ty)?;
    let (table, last) = table_for(&mut doc, dotted)?;
    if let Some(existing) = table.get(last) {
        if existing.is_table() || existing.is_array_of_tables() {
            return Err(Diag::error(
                "kiln::config",
                format!("`{dotted}` is a table, not a scalar; `kiln config set` cannot write it"),
            ));
        }
    }
    table.insert(last, Item::Value(value));
    Ok(doc.to_string())
}

/// Remove a scalar key. `bool` says whether it was present; removing an
/// absent key is a successful no-op, not an error, so the caller does not
/// need to check first.
pub fn unset_scalar(text: &str, dotted: &str) -> Result<(String, bool), Diag> {
    let mut doc = parse(text)?;
    let Some((table, last)) = find_table_mut(&mut doc, dotted) else {
        return Ok((text.to_string(), false));
    };
    let was_present = table.remove(last).is_some();
    Ok((doc.to_string(), was_present))
}

/// Whether a list-valued key's array, as written in this one file, already
/// contains `item`. Used to decide whether `add` would be a no-op before
/// anyone commits to a target file.
pub fn list_contains(text: &str, dotted: &str, item: &str) -> bool {
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return false;
    };
    let mut cur = doc.as_table();
    let mut segs = dotted.split('.').peekable();
    while let Some(seg) = segs.next() {
        let Some(next) = cur.get(seg) else {
            return false;
        };
        if segs.peek().is_none() {
            return next
                .as_array()
                .map(|a| a.iter().any(|v| v.as_str() == Some(item)))
                .unwrap_or(false);
        }
        let Some(t) = next.as_table() else {
            return false;
        };
        cur = t;
    }
    false
}

/// Append to a scalar-set list, creating it (and any table above it) if
/// nothing is there yet. `bool` says whether the item was newly added; a
/// value already present in this file's own array is a no-op, since writing
/// a duplicate would only dedupe away at merge time.
pub fn add_list_item(text: &str, dotted: &str, item: &str) -> Result<(String, bool), Diag> {
    let mut doc = parse(text)?;
    let (table, last) = table_for(&mut doc, dotted)?;
    let entry = table
        .entry(last)
        .or_insert_with(|| Item::Value(Value::Array(Array::new())));
    let arr = entry
        .as_array_mut()
        .ok_or_else(|| Diag::error("kiln::config", format!("`{dotted}` is not a list")))?;
    if arr.iter().any(|v| v.as_str() == Some(item)) {
        return Ok((doc.to_string(), false));
    }
    arr.push(item);
    Ok((doc.to_string(), true))
}

/// Remove one value from a scalar-set list in this file. `bool` says whether
/// it was found and removed; an absent value, or an absent key entirely, is a
/// no-op the caller reports, not an error this function raises.
pub fn remove_list_item(text: &str, dotted: &str, item: &str) -> Result<(String, bool), Diag> {
    let mut doc = parse(text)?;
    let Some((table, last)) = find_table_mut(&mut doc, dotted) else {
        return Ok((text.to_string(), false));
    };
    let Some(entry) = table.get_mut(last) else {
        return Ok((text.to_string(), false));
    };
    let Some(arr) = entry.as_array_mut() else {
        return Err(Diag::error(
            "kiln::config",
            format!("`{dotted}` is not a list"),
        ));
    };
    let Some(idx) = arr.iter().position(|v| v.as_str() == Some(item)) else {
        return Ok((doc.to_string(), false));
    };
    arr.remove(idx);
    Ok((doc.to_string(), true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_creates_missing_tables() {
        let out = set_scalar("", "boot.timeout", "10", Ty::Int).unwrap();
        assert!(out.contains("[boot]"));
        assert!(out.contains("timeout = 10"));
    }

    #[test]
    fn set_overwrites_a_scalar_in_place() {
        let text = "[boot]\ntimeout = 5\nloader = \"grub2\"\n";
        let out = set_scalar(text, "boot.timeout", "10", Ty::Int).unwrap();
        assert!(out.contains("timeout = 10"));
        assert!(out.contains("loader = \"grub2\""));
    }

    #[test]
    fn set_rejects_a_non_integer_for_an_integer_key() {
        assert!(set_scalar("", "boot.timeout", "soon", Ty::Int).is_err());
    }

    #[test]
    fn set_rejects_writing_a_scalar_over_a_table() {
        let text = "[boot]\ntimeout = 5\n";
        assert!(set_scalar(text, "boot", "5", Ty::Int).is_err());
    }

    #[test]
    fn unset_removes_an_existing_key() {
        let text = "[boot]\ntimeout = 5\nloader = \"grub2\"\n";
        let (out, was_present) = unset_scalar(text, "boot.timeout").unwrap();
        assert!(was_present);
        assert!(!out.contains("timeout"));
        assert!(out.contains("loader"));
    }

    #[test]
    fn unset_is_a_noop_when_absent() {
        let text = "[boot]\nloader = \"grub2\"\n";
        let (out, was_present) = unset_scalar(text, "boot.timeout").unwrap();
        assert!(!was_present);
        assert_eq!(out, text);
    }

    #[test]
    fn add_creates_the_list_and_appends() {
        let (out, added) = add_list_item("", "kernel.cmdline", "quiet").unwrap();
        assert!(added);
        assert!(out.contains("quiet"));
    }

    #[test]
    fn add_dedupes_against_this_files_own_array() {
        let text = "[kernel]\ncmdline = [\"quiet\"]\n";
        let (out, added) = add_list_item(text, "kernel.cmdline", "quiet").unwrap();
        assert!(!added);
        assert_eq!(out, text);
    }

    #[test]
    fn remove_deletes_a_present_value() {
        let text = "[kernel]\ncmdline = [\"quiet\", \"splash\"]\n";
        let (out, removed) = remove_list_item(text, "kernel.cmdline", "quiet").unwrap();
        assert!(removed);
        assert!(!out.contains("quiet"));
        assert!(out.contains("splash"));
    }

    #[test]
    fn remove_is_a_noop_when_absent() {
        let text = "[kernel]\ncmdline = [\"splash\"]\n";
        let (out, removed) = remove_list_item(text, "kernel.cmdline", "quiet").unwrap();
        assert!(!removed);
        assert_eq!(out, text);
    }

    #[test]
    fn list_contains_reflects_this_files_own_array_only() {
        let text = "[kernel]\ncmdline = [\"quiet\"]\n";
        assert!(list_contains(text, "kernel.cmdline", "quiet"));
        assert!(!list_contains(text, "kernel.cmdline", "splash"));
        assert!(!list_contains(text, "kernel.dracut_modules", "quiet"));
    }

    #[test]
    fn untouched_content_survives_byte_for_byte() {
        let text = "# a comment kiln should keep\n[boot]\nloader = \"grub2\" # inline note\n";
        let (out, _) = unset_scalar(text, "boot.timeout").unwrap();
        assert_eq!(out, text);
    }
}
