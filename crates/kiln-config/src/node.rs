//! A TOML document as a spanned generic tree.
//!
//! Merge operates on *this*, not on typed structs. Two reasons: the merge
//! algebra is then genuinely generic and can be property-tested on its own, and
//! every key keeps its provenance through the merge, which is what
//! `kiln explain` needs.
//!
//! Typed extraction happens exactly once, afterwards, in `validate`.

use kiln_diag::{Diag, Errors, Origin, Src};
use std::collections::BTreeMap;
use toml_edit::{ImDocument, Item, Value};

#[derive(Debug, Clone)]
pub enum NodeKind {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<Node>),
    Table(Table),
}

pub type Table = BTreeMap<String, Entry>;

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub origin: Origin,
}

/// A key/value pair. The key's own span is kept separately so a diagnostic can
/// underline `enabeld` rather than `true`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: Origin,
    pub value: Node,
}

impl Node {
    pub fn type_name(&self) -> &'static str {
        match self.kind {
            NodeKind::Str(_) => "string",
            NodeKind::Int(_) => "integer",
            NodeKind::Bool(_) => "boolean",
            NodeKind::Array(_) => "array",
            NodeKind::Table(_) => "table",
        }
    }

    pub fn as_table(&self) -> Option<&Table> {
        match &self.kind {
            NodeKind::Table(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Node]> {
        match &self.kind {
            NodeKind::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Scalar identity, used to deduplicate unioned lists and to compare values
    /// across files. Provenance deliberately plays no part.
    pub fn scalar_key(&self) -> Option<String> {
        match &self.kind {
            NodeKind::Str(s) => Some(format!("s:{s}")),
            NodeKind::Int(i) => Some(format!("i:{i}")),
            NodeKind::Bool(b) => Some(format!("b:{b}")),
            _ => None,
        }
    }

    /// Structural equality, ignoring provenance. This is what decides whether
    /// two files "agree" (rule 3: identical values are fine).
    pub fn same_value(&self, other: &Node) -> bool {
        match (&self.kind, &other.kind) {
            (NodeKind::Str(a), NodeKind::Str(b)) => a == b,
            (NodeKind::Int(a), NodeKind::Int(b)) => a == b,
            (NodeKind::Bool(a), NodeKind::Bool(b)) => a == b,
            (NodeKind::Array(a), NodeKind::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same_value(y))
            }
            (NodeKind::Table(a), NodeKind::Table(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, v)| b.get(k).is_some_and(|w| v.value.same_value(&w.value)))
            }
            _ => false,
        }
    }

    /// A short rendering for "…both set it, to `5` and `0`" messages.
    pub fn render(&self) -> String {
        match &self.kind {
            NodeKind::Str(s) => format!("\"{s}\""),
            NodeKind::Int(i) => i.to_string(),
            NodeKind::Bool(b) => b.to_string(),
            NodeKind::Array(a) => format!("[{} items]", a.len()),
            NodeKind::Table(t) => format!("{{{} keys}}", t.len()),
        }
    }
}

/// Parse one file. Reports every syntax and structure problem it can before
/// giving up.
pub fn parse(src: &Src) -> Result<Node, Errors> {
    let doc = match ImDocument::parse(src.text.clone()) {
        Ok(d) => d,
        Err(e) => {
            let span = e.span().unwrap_or(0..src.text.len().min(1));
            let mut errs = Errors::new();
            errs.push(
                Diag::error("kiln::syntax", "invalid TOML")
                    .label(&Origin::new(src.clone(), span), e.message().to_string()),
            );
            return Err(errs);
        }
    };

    let mut errs = Errors::new();
    let root_span = 0..src.text.len();
    let table = convert_table(doc.as_table(), src, &mut errs);
    let node = Node {
        kind: NodeKind::Table(table),
        origin: Origin::new(src.clone(), root_span),
    };
    errs.into_result(node)
}

fn origin(src: &Src, span: Option<std::ops::Range<usize>>) -> Origin {
    Origin::new(src.clone(), span.unwrap_or(0..0))
}

fn convert_table(t: &toml_edit::Table, src: &Src, errs: &mut Errors) -> Table {
    let mut out = Table::new();
    for (key, item) in t.iter() {
        let key_span = t.key(key).and_then(|k| k.span());
        let key_origin = origin(src, key_span);
        if let Some(value) = convert_item(item, src, &key_origin, errs) {
            out.insert(
                key.to_string(),
                Entry {
                    key: key_origin,
                    value,
                },
            );
        }
    }
    out
}

fn convert_item(item: &Item, src: &Src, key: &Origin, errs: &mut Errors) -> Option<Node> {
    match item {
        Item::Value(v) => convert_value(v, src, key, errs),
        Item::Table(t) => Some(Node {
            kind: NodeKind::Table(convert_table(t, src, errs)),
            origin: origin(src, t.span()).or_key(key),
        }),
        Item::ArrayOfTables(aot) => {
            let items: Vec<Node> = aot
                .iter()
                .map(|t| Node {
                    kind: NodeKind::Table(convert_table(t, src, errs)),
                    origin: origin(src, t.span()).or_key(key),
                })
                .collect();
            Some(Node {
                kind: NodeKind::Array(items),
                origin: key.clone(),
            })
        }
        Item::None => None,
    }
}

fn convert_value(v: &Value, src: &Src, key: &Origin, errs: &mut Errors) -> Option<Node> {
    let o = origin(src, v.span()).or_key(key);
    let kind = match v {
        Value::String(s) => NodeKind::Str(s.value().clone()),
        Value::Integer(i) => NodeKind::Int(*i.value()),
        Value::Boolean(b) => NodeKind::Bool(*b.value()),
        Value::Array(a) => NodeKind::Array(
            a.iter()
                .filter_map(|e| convert_value(e, src, key, errs))
                .collect(),
        ),
        Value::InlineTable(t) => {
            let mut out = Table::new();
            for (k, val) in t.iter() {
                let key_origin = origin(src, t.key(k).and_then(|kk| kk.span()));
                if let Some(node) = convert_value(val, src, &key_origin, errs) {
                    out.insert(
                        k.to_string(),
                        Entry {
                            key: key_origin,
                            value: node,
                        },
                    );
                }
            }
            NodeKind::Table(out)
        }
        Value::Float(_) => {
            errs.push(
                Diag::error(
                    "kiln::structure",
                    "floating-point values are not used anywhere in Kiln",
                )
                .label(&o, "here")
                .help("every numeric key in the schema is an integer"),
            );
            return None;
        }
        // `snapshot = 2026-08-24` is a TOML *date*, not a string, and is the
        // single most likely place a user forgets the quotes (`repos.snapshot`).
        Value::Datetime(_) => {
            errs.push(
                Diag::error("kiln::structure", "dates are not a Kiln value type")
                    .label(&o, "unquoted date")
                    .help("quote it: `snapshot = \"2026-08-24\"`"),
            );
            return None;
        }
    };
    Some(Node { kind, origin: o })
}

trait OrKey {
    fn or_key(self, key: &Origin) -> Origin;
}

impl OrKey for Origin {
    /// Implicit tables (`[a.b]` creating `a`) have no span of their own; fall
    /// back to the key so a diagnostic still points somewhere useful.
    fn or_key(self, key: &Origin) -> Origin {
        if self.span.is_empty() {
            key.clone()
        } else {
            self
        }
    }
}

/// Walk every leaf, deepest-last, calling `f` with the dotted path.
pub fn walk<'a>(node: &'a Node, path: &mut Vec<String>, f: &mut impl FnMut(&str, &'a Entry)) {
    if let NodeKind::Table(t) = &node.kind {
        for (k, e) in t {
            path.push(k.clone());
            let dotted = path.join(".");
            f(&dotted, e);
            walk(&e.value, path, f);
            path.pop();
        }
    }
}

/// Look up a dotted path such as `boot.timeout`.
pub fn get<'a>(node: &'a Node, dotted: &str) -> Option<&'a Entry> {
    let mut cur = node;
    let mut found = None;
    for part in dotted.split('.') {
        let t = cur.as_table()?;
        let e = t.get(part)?;
        found = Some(e);
        cur = &e.value;
    }
    found
}
