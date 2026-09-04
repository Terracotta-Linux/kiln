//! Per-file structural checks: schema version, `include` placement, unknown
//! keys, and shape — the "structure" phase.
//!
//! Every problem in the file is reported, not just the first.

use crate::node::{Entry, Node, NodeKind};
use crate::schema;
use kiln_diag::{did_you_mean, Diag, Errors};

pub const SCHEMA_VERSION: i64 = 1;

pub fn check(doc: &Node) -> Errors {
    let mut errs = Errors::new();
    check_version(doc, &mut errs);
    check_keys(doc, &mut Vec::new(), &mut errs);
    errs
}

/// `kiln = 1` is required and must be the first key.
fn check_version(doc: &Node, errs: &mut Errors) {
    let Some(table) = doc.as_table() else { return };
    let Some(entry) = table.get("kiln") else {
        errs.push(
            Diag::error("kiln::structure", "missing `kiln` schema version")
                .label(&doc.origin, "this file has no `kiln` key")
                .help("every Kiln file starts with `kiln = 1` as its first key"),
        );
        return;
    };

    match &entry.value.kind {
        NodeKind::Int(v) if *v == SCHEMA_VERSION => {}
        NodeKind::Int(v) => errs.push(
            Diag::error("kiln::structure", format!("unsupported schema version {v}"))
                .label(&entry.value.origin, "here")
                .help(format!(
                    "this build of Kiln understands `kiln = {SCHEMA_VERSION}`"
                )),
        ),
        other => errs.push(
            Diag::error(
                "kiln::structure",
                format!("`kiln` must be an integer, found {}", type_name(other)),
            )
            .label(&entry.value.origin, "here")
            .help("write `kiln = 1`"),
        ),
    }

    // "First key" is checked by position, because a reader scanning the top of
    // the file is the reason the rule exists.
    if let Some((name, first)) = table
        .iter()
        .filter(|(k, _)| k.as_str() != "kiln")
        .min_by_key(|(_, e)| e.key.span.start)
    {
        if first.key.span.start < entry.key.span.start {
            errs.push(
                Diag::error(
                    "kiln::structure",
                    "`kiln` must be the first key in the file",
                )
                .label(&first.key, format!("`{name}` comes before it"))
                .label(&entry.key, "`kiln` is here")
                .help("move `kiln = 1` to the top of the file"),
            );
        }
    }
}

fn type_name(k: &NodeKind) -> &'static str {
    match k {
        NodeKind::Str(_) => "string",
        NodeKind::Int(_) => "integer",
        NodeKind::Bool(_) => "boolean",
        NodeKind::Array(_) => "array",
        NodeKind::Table(_) => "table",
    }
}

fn check_keys(node: &Node, path: &mut Vec<String>, errs: &mut Errors) {
    let Some(table) = node.as_table() else { return };
    for (key, entry) in table {
        path.push(key.clone());
        let dotted = path.join(".");

        if key == "include" && path.len() > 1 {
            misplaced_include(entry, &dotted, errs);
        } else if !schema::KEYS.contains(&dotted.as_str()) {
            unknown_key(key, &dotted, entry, path, errs);
        } else if schema::is_list(&dotted) {
            check_list(&dotted, entry, errs);
        } else if check_type(&dotted, entry, errs) && !schema::is_open_map(&dotted) {
            check_keys(&entry.value, path, errs);
        }

        path.pop();
    }
}

/// "TOML's most common footgun is that a bare key written after
/// `[packages]` silently becomes `packages.include`."
fn misplaced_include(entry: &Entry, dotted: &str, errs: &mut Errors) {
    errs.push(
        Diag::error("kiln::structure", "`include` is in the wrong place")
            .label(&entry.key, format!("this became `{dotted}`"))
            .help(
                "`include` must be a top-level key, before any table header. Written after a \
                 `[section]` header, TOML makes it a key *of that section* instead.",
            ),
    );
}

fn unknown_key(key: &str, dotted: &str, entry: &Entry, path: &[String], errs: &mut Errors) {
    let parent = path[..path.len() - 1].join(".");
    let siblings: Vec<&str> = schema::KEYS
        .iter()
        .copied()
        .filter(|k| match k.rsplit_once('.') {
            Some((p, _)) => p == parent,
            None => parent.is_empty(),
        })
        .map(|k| k.rsplit_once('.').map_or(k, |(_, last)| last))
        .collect();

    let mut siblings = siblings;
    siblings.sort_unstable();
    let where_ = if parent.is_empty() {
        "at the top level".to_string()
    } else {
        format!("in `{parent}`")
    };
    errs.push(
        Diag::error("kiln::structure", format!("unknown key `{key}` {where_}"))
            .label(&entry.key, "not part of the schema")
            .maybe_help(did_you_mean(key, siblings.iter().copied()).or_else(|| {
                Some(if siblings.is_empty() {
                    format!("`{dotted}` is not a Kiln key")
                } else {
                    format!("{where_} Kiln knows: {}", siblings.join(", "))
                })
            })),
    );
}

/// Returns whether the value is the right shape to keep descending into.
fn check_type(dotted: &str, entry: &Entry, errs: &mut Errors) -> bool {
    let Some(want) = schema::scalar_type(dotted) else {
        return true;
    };
    let got = &entry.value.kind;
    let ok = matches!(
        (want, got),
        (schema::Ty::Str, NodeKind::Str(_))
            | (schema::Ty::Int, NodeKind::Int(_))
            | (schema::Ty::Bool, NodeKind::Bool(_))
            | (schema::Ty::Table, NodeKind::Table(_))
    );
    if ok {
        return true;
    }
    let want_name = match want {
        schema::Ty::Str => "a string",
        schema::Ty::Int => "an integer",
        schema::Ty::Bool => "a boolean",
        schema::Ty::Table => "a table",
    };
    let help = match (want, got) {
        (schema::Ty::Int, NodeKind::Str(s)) if s.parse::<i64>().is_ok() => {
            Some(format!("drop the quotes: `{dotted} = {s}`"))
        }
        (schema::Ty::Bool, NodeKind::Str(s)) => match s.as_str() {
            "yes" | "true" | "on" => Some(format!("write `{dotted} = true`")),
            "no" | "false" | "off" => Some(format!("write `{dotted} = false`")),
            _ => None,
        },
        _ => None,
    };
    errs.push(
        Diag::error(
            "kiln::structure",
            format!(
                "`{dotted}` must be {want_name}, found {}",
                entry.value.type_name()
            ),
        )
        .label(&entry.value.origin, "here")
        .maybe_help(help),
    );
    false
}

fn check_list(dotted: &str, entry: &Entry, errs: &mut Errors) {
    let Some(items) = entry.value.as_array() else {
        errs.push(
            Diag::error(
                "kiln::structure",
                format!(
                    "`{dotted}` must be a list, found {}",
                    entry.value.type_name()
                ),
            )
            .label(&entry.value.origin, "here"),
        );
        return;
    };

    let Some(allowed) = schema::entry_keys(dotted) else {
        // A plain set of scalars.
        for item in items {
            if item.as_str().is_none() {
                errs.push(
                    Diag::error(
                        "kiln::structure",
                        format!("`{dotted}` takes strings, found {}", item.type_name()),
                    )
                    .label(&item.origin, "here"),
                );
            }
        }
        return;
    };

    for item in items {
        let Some(t) = item.as_table() else {
            errs.push(
                Diag::error(
                    "kiln::structure",
                    format!(
                        "entries of `{dotted}` must be tables, found {}",
                        item.type_name()
                    ),
                )
                .label(&item.origin, "here"),
            );
            continue;
        };
        for (k, e) in t {
            if !allowed.contains(&k.as_str()) {
                errs.push(
                    Diag::error(
                        "kiln::structure",
                        format!("unknown key `{k}` in a `{dotted}` entry"),
                    )
                    .label(&e.key, "not part of the schema")
                    .maybe_help(
                        did_you_mean(k, allowed.iter().copied()).or_else(|| {
                            Some(format!("a `{dotted}` entry takes: {}", allowed.join(", ")))
                        }),
                    ),
                );
            }
        }
    }
}
