//! "Everywhere a table is accepted, a bare string is too,
//! meaning the table with only its primary key set."
//!
//! Expanding shorthand *before* merge is what lets merge stay generic: by the
//! time the three rules run, `"firefox"` and `{ name = "firefox" }` are the same
//! shape and dedupe against each other.

use crate::node::{Entry, Node, NodeKind};
use crate::schema;

pub fn expand(doc: &mut Node) {
    expand_at(doc, &mut Vec::new());
}

fn expand_at(node: &mut Node, path: &mut Vec<String>) {
    let NodeKind::Table(table) = &mut node.kind else {
        return;
    };
    for (key, entry) in table.iter_mut() {
        path.push(key.clone());
        let dotted = path.join(".");
        if let Some(spec) = schema::list_spec(&dotted) {
            if let NodeKind::Array(items) = &mut entry.value.kind {
                for item in items.iter_mut() {
                    if let Some(primary) = spec.shorthand {
                        expand_item(item, primary);
                    }
                }
            }
            derive_names(&dotted, &mut entry.value);
        } else if !schema::is_open_map(&dotted) {
            expand_at(&mut entry.value, path);
        }
        path.pop();
    }
}

fn expand_item(item: &mut Node, primary: &str) {
    if !matches!(item.kind, NodeKind::Str(_)) {
        return;
    }
    let origin = item.origin.clone();
    let inner = Node {
        kind: item.kind.clone(),
        origin: origin.clone(),
    };
    let mut t = crate::node::Table::new();
    t.insert(
        primary.to_string(),
        Entry {
            key: origin.clone(),
            value: inner,
        },
    );
    item.kind = NodeKind::Table(t);
}

/// `[[script]]` identifies by `name`, but shorthand only writes `source`.
/// Derive the name from the source file's stem so the identity key always
/// exists — otherwise ordering would become file-order-determined, which
/// the merge algebra explicitly forbids.
fn derive_names(dotted: &str, list: &mut Node) {
    if dotted != "script" {
        return;
    }
    let NodeKind::Array(items) = &mut list.kind else {
        return;
    };
    for item in items {
        let NodeKind::Table(t) = &mut item.kind else {
            continue;
        };
        if t.contains_key("name") {
            continue;
        }
        let Some(stem) = t.get("source").and_then(|e| e.value.as_str()).map(|s| {
            std::path::Path::new(s)
                .file_stem()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_else(|| s.to_string())
        }) else {
            continue;
        };
        let origin = t["source"].key.clone();
        t.insert(
            "name".into(),
            Entry {
                key: origin.clone(),
                value: Node {
                    kind: NodeKind::Str(stem),
                    origin,
                },
            },
        );
    }
}
