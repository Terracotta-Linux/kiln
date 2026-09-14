//! The merge algebra — exactly three rules:
//!
//! 1. **Lists union.** Duplicates collapse. Order is discarded.
//! 2. **The includer wins.** If A includes B and both set a scalar, A's value is used.
//! 3. **Siblings conflicting is an error** — never last-wins. Identical values are fine.
//!
//! Rule 3 is the load-bearing one: ambiguity is an error, not a coin flip. The
//! merge also records, per dotted key, what was overridden and by what, which is
//! the entirety of what `kiln explain` needs.

use crate::include::Unit;
use crate::node::{Entry, Node, NodeKind};
use crate::schema;
use kiln_diag::{Diag, Errors, Src};

pub use kiln_diag::{Origin, OriginMap as Origins, Provenance};
use std::collections::BTreeMap;

pub struct Merged {
    pub doc: Node,
    pub origins: Origins,
}

pub fn merge(root: &Unit) -> Result<Merged, Errors> {
    let mut errs = Errors::new();
    let doc = merge_unit(root, &mut errs);
    let origins = provenance(root);
    errs.into_result(Merged { doc, origins })
}

/// Provenance is computed by walking the include tree rather than threaded
/// through the merge, because the answer is a property of the tree: the
/// *shallowest* file that sets a key is the one whose value survives (rule 2),
/// and siblings that disagree are already an error (rule 3), so nothing else can
/// change the outcome. Doing it separately keeps the merge itself small enough
/// to property-test.
pub fn provenance(root: &Unit) -> Origins {
    let mut found: BTreeMap<String, Vec<(usize, usize, Origin)>> = BTreeMap::new();
    let mut seq = 0usize;
    collect(root, 0, &mut seq, &mut found);

    found
        .into_iter()
        .map(|(key, mut hits)| {
            hits.sort_by_key(|(depth, order, _)| (*depth, *order));
            let mut it = hits.into_iter().map(|(_, _, o)| o);
            let effective = it.next().expect("a key is only recorded when it is set");
            (
                key.clone(),
                Provenance {
                    effective,
                    others: it.collect(),
                    is_list: schema::is_list(&key),
                },
            )
        })
        .collect()
}

fn collect(
    unit: &Unit,
    depth: usize,
    seq: &mut usize,
    out: &mut BTreeMap<String, Vec<(usize, usize, Origin)>>,
) {
    let order = *seq;
    *seq += 1;
    let stripped = strip_control_keys(&unit.doc);
    let mut path = Vec::new();
    keys_of(&stripped, &mut path, &mut |dotted, origin| {
        out.entry(dotted.to_string())
            .or_default()
            .push((depth, order, origin));
    });
    for child in &unit.children {
        collect(child, depth + 1, seq, out);
    }
}

/// Every *leaf* key, where a list counts as a leaf: `kernel.cmdline` is one
/// thing a file sets, not four.
fn keys_of(node: &Node, path: &mut Vec<String>, f: &mut impl FnMut(&str, Origin)) {
    let NodeKind::Table(t) = &node.kind else {
        return;
    };
    for (k, e) in t {
        path.push(k.clone());
        let dotted = path.join(".");
        if schema::is_list(&dotted) || !matches!(e.value.kind, NodeKind::Table(_)) {
            f(&dotted, e.value.origin.clone());
        } else {
            keys_of(&e.value, path, f);
        }
        path.pop();
    }
}

/// A file's contribution is its includes merged as siblings, then its own
/// content laid over the top.
fn merge_unit(unit: &Unit, errs: &mut Errors) -> Node {
    let mut acc: Option<Node> = None;
    for child in &unit.children {
        let child_doc = merge_unit(child, errs);
        acc = Some(match acc {
            None => child_doc,
            Some(prev) => {
                let mut path = Vec::new();
                union_siblings(prev, child_doc, &mut path, &unit.src, errs)
            }
        });
    }

    let own = strip_control_keys(&unit.doc);
    match acc {
        None => own,
        Some(children) => overlay(children, own, &mut Vec::new(), errs),
    }
}

/// `include` and `kiln` describe the file, not the image. They must not reach
/// the Manifest, and they must not participate in merge conflicts — two files
/// both saying `kiln = 1` is not a disagreement.
fn strip_control_keys(doc: &Node) -> Node {
    let mut out = doc.clone();
    if let NodeKind::Table(t) = &mut out.kind {
        t.remove("include");
        t.remove("kiln");
    }
    out
}

/// Rule 3. Two files at the same level of the include tree.
fn union_siblings(
    a: Node,
    b: Node,
    path: &mut Vec<String>,
    includer: &Src,
    errs: &mut Errors,
) -> Node {
    match (a.kind, b.kind) {
        (NodeKind::Table(ta), NodeKind::Table(tb)) => {
            let mut out = ta;
            for (key, eb) in tb {
                path.push(key.clone());
                match out.remove(&key) {
                    None => {
                        out.insert(key.clone(), eb);
                    }
                    Some(ea) => {
                        let dotted = path.join(".");
                        let merged = if schema::is_list(&dotted) {
                            union_lists(
                                &dotted,
                                ea.value,
                                eb.value,
                                &Rule::Siblings(includer),
                                errs,
                            )
                        } else if matches!(ea.value.kind, NodeKind::Table(_)) {
                            union_siblings(ea.value, eb.value, path, includer, errs)
                        } else if ea.value.same_value(&eb.value) {
                            ea.value
                        } else {
                            conflict(&dotted, &ea, &eb, includer, errs);
                            ea.value
                        };
                        out.insert(
                            key.clone(),
                            Entry {
                                key: ea.key,
                                value: merged,
                            },
                        );
                    }
                }
                path.pop();
            }
            Node {
                kind: NodeKind::Table(out),
                origin: a.origin,
            }
        }
        (ka, _) => Node {
            kind: ka,
            origin: a.origin,
        },
    }
}

/// Rule 3's diagnostic: both origins, and the one file that can settle it.
fn conflict(dotted: &str, a: &Entry, b: &Entry, includer: &Src, errs: &mut Errors) {
    errs.push(
        Diag::error("kiln::merge", format!("conflicting values for `{dotted}`"))
            .label(&a.value.origin, format!("set to {} here", a.value.render()))
            .label(&b.value.origin, format!("and to {} here", b.value.render()))
            .help(format!(
                "`{}` and `{}` are both included by `{}`. Set `{dotted}` in `{}` to resolve it — \
                 the includer always wins.",
                a.value.origin.file.name, b.value.origin.file.name, includer.name, includer.name,
            )),
    );
}

/// Which rule two lists are being combined under. The union itself is the same
/// either way; only the *fields* of two entries sharing an identity key need to
/// know, because that is where rules 2 and 3 disagree.
enum Rule<'a> {
    /// Rule 2: the includer's fields win, silently.
    Overlay,
    /// Rule 3: two siblings setting the same field to different values conflict,
    /// exactly as they would outside a list.
    Siblings(&'a Src),
}

/// Rule 1. Union, deduplicated, canonically sorted. Identity-keyed lists merge
/// entry by entry so that two files describing the same `[[file]]` target
/// combine rather than duplicate.
fn union_lists(dotted: &str, a: Node, b: Node, rule: &Rule<'_>, errs: &mut Errors) -> Node {
    let (NodeKind::Array(items_a), NodeKind::Array(items_b)) = (a.kind, b.kind) else {
        // A type error here was already reported by `structure::check`.
        return Node {
            kind: NodeKind::Array(Vec::new()),
            origin: a.origin,
        };
    };
    let identity = schema::list_spec(dotted).and_then(|s| s.identity);

    // Indexed rather than scanned: `list_key` allocates, so re-deriving the key
    // of everything already placed for every element added is quadratic in the
    // number of packages a profile include brings in.
    let mut out: Vec<Node> = Vec::new();
    let mut at: BTreeMap<Option<String>, usize> = BTreeMap::new();
    for item in items_a.into_iter().chain(items_b) {
        let key = list_key(&item, identity);
        match at.get(&key) {
            None => {
                at.insert(key, out.len());
                out.push(item);
            }
            Some(&i) => {
                let entry = entry_path(dotted, &out[i], identity);
                merge_entry(&entry, &mut out[i], item, rule, errs);
            }
        }
    }
    sort_items(&mut out, identity);
    Node {
        kind: NodeKind::Array(out),
        origin: a.origin,
    }
}

/// Shallow-merge one array-of-tables entry into another of the same identity.
fn merge_entry(entry: &str, into: &mut Node, from: Node, rule: &Rule<'_>, errs: &mut Errors) {
    let (NodeKind::Table(kept), NodeKind::Table(adding)) = (&mut into.kind, from.kind) else {
        return;
    };
    for (key, new) in adding {
        let Some(held) = kept.get_mut(&key) else {
            kept.insert(key, new);
            continue;
        };
        match rule {
            Rule::Overlay => *held = new,
            Rule::Siblings(includer) => {
                if !held.value.same_value(&new.value) {
                    conflict(&format!("{entry}.{key}"), held, &new, includer, errs);
                }
            }
        }
    }
}

/// How an entry inside an array of tables is named in a diagnostic:
/// `file[target="/etc/motd"]`.
fn entry_path(dotted: &str, entry: &Node, identity: Option<&str>) -> String {
    let named = identity
        .and_then(|field| Some((field, entry.as_table()?.get(field)?)))
        .map(|(field, e)| format!("[{field}={}]", e.value.render()));
    format!("{dotted}{}", named.unwrap_or_default())
}

/// Iteration order is content-determined, not insertion-determined: reordering
/// lines in a TOML file must never change `config_id`.
fn sort_items(items: &mut [Node], identity: Option<&str>) {
    items.sort_by_key(|n| list_key(n, identity).unwrap_or_default());
}

fn list_key(node: &Node, identity: Option<&str>) -> Option<String> {
    match identity {
        Some(field) => node
            .as_table()
            .and_then(|t| t.get(field))
            .and_then(|e| e.value.scalar_key()),
        None => node.scalar_key(),
    }
}

/// Rule 2. `top` is the including file; it wins, and we record what it displaced.
fn overlay(base: Node, top: Node, path: &mut Vec<String>, errs: &mut Errors) -> Node {
    match (base.kind, top.kind) {
        (NodeKind::Table(tb), NodeKind::Table(tt)) => {
            let mut out = tb;
            for (key, et) in tt {
                path.push(key.clone());
                let dotted = path.join(".");
                match out.remove(&key) {
                    None => {
                        out.insert(key.clone(), et);
                    }
                    Some(eb) => {
                        let merged = if schema::is_list(&dotted) {
                            union_lists(&dotted, eb.value, et.value, &Rule::Overlay, errs)
                        } else if matches!(et.value.kind, NodeKind::Table(_)) {
                            overlay(eb.value, et.value, path, errs)
                        } else {
                            // Rule 2, the whole of it: the includer's value wins,
                            // silently and without ceremony.
                            et.value
                        };
                        out.insert(
                            key.clone(),
                            Entry {
                                key: et.key,
                                value: merged,
                            },
                        );
                    }
                }
                path.pop();
            }
            Node {
                kind: NodeKind::Table(out),
                origin: base.origin,
            }
        }
        (_, kt) => Node {
            kind: kt,
            origin: top.origin,
        },
    }
}
