//! The include graph.
//!
//! Depth-first with canonicalized paths; including the same file twice is a
//! no-op; cycles are a hard error printing the cycle; depth is capped at 32.
//! Kiln does **not** glob-load: every file that participates is reachable
//! through an explicit `include` chain from the entry point.

use crate::discover::Loader;
use crate::node::{self, Node, NodeKind};
use crate::{shorthand, structure};
use kiln_diag::{Diag, Errors, Origin, Src};
use std::collections::BTreeSet;
use std::path::PathBuf;

pub const MAX_DEPTH: usize = 32;

/// One file and everything it includes, in include order.
pub struct Unit {
    pub src: Src,
    pub doc: Node,
    pub children: Vec<Unit>,
}

impl Unit {
    /// Every file in the graph, entry point first.
    pub fn files(&self) -> Vec<Src> {
        let mut out = vec![self.src.clone()];
        for c in &self.children {
            out.extend(c.files());
        }
        out
    }
}

struct Walker<'a> {
    loader: &'a mut Loader,
    stack: Vec<PathBuf>,
    visited: BTreeSet<PathBuf>,
    errs: Errors,
}

/// Load the entry point and everything reachable from it.
pub fn load(loader: &mut Loader, entry: PathBuf) -> Result<Unit, Errors> {
    let mut w = Walker {
        loader,
        stack: Vec::new(),
        visited: BTreeSet::new(),
        errs: Errors::new(),
    };
    let unit = w.visit(entry, None, 0);
    match unit {
        Some(u) => w.errs.into_result(u),
        None => Err(w.errs.sorted()),
    }
}

impl Walker<'_> {
    fn visit(&mut self, path: PathBuf, at: Option<&Origin>, depth: usize) -> Option<Unit> {
        if depth > MAX_DEPTH {
            self.errs.push(
                Diag::error("kiln::graph", format!("include depth exceeded {MAX_DEPTH}"))
                    .maybe_help(at.map(|_| {
                        "a configuration this deep is almost always a cycle that dodged \
                         detection, or a module library that wants flattening"
                            .to_string()
                    })),
            );
            return None;
        }

        // Cycle detection must come *before* the visited check, or a cycle
        // reached through an already-included file is silently deduplicated
        // into a no-op instead of being reported.
        if let Some(i) = self.stack.iter().position(|p| *p == path) {
            self.cycle(i, &path, at);
            return None;
        }
        // including the same file twice is a no-op. Safe to deduplicate on
        // first visit because a file's contribution does not depend on who
        // included it — only on what it says.
        if !self.visited.insert(path.clone()) {
            return None;
        }

        let src = match self.loader.load(&path, at) {
            Ok(s) => s,
            Err(d) => {
                self.errs.push(d);
                return None;
            }
        };

        let mut doc = match node::parse(&src) {
            Ok(d) => d,
            Err(e) => {
                self.errs.extend(e);
                return None;
            }
        };
        shorthand::expand(&mut doc);
        self.errs.extend(structure::check(&doc));

        let includes = self.include_list(&doc);

        self.stack.push(path.clone());
        let mut children = Vec::new();
        for reference in includes {
            let target = match self
                .loader
                .resolve_include(&reference.value, &reference.origin)
            {
                Ok(t) => t,
                Err(d) => {
                    self.errs.push(d);
                    continue;
                }
            };
            if let Some(child) = self.visit(target, Some(&reference.origin), depth + 1) {
                children.push(child);
            }
        }
        self.stack.pop();

        Some(Unit { src, doc, children })
    }

    fn cycle(&mut self, start: usize, path: &std::path::Path, at: Option<&Origin>) {
        let names: Vec<String> = self.stack[start..]
            .iter()
            .chain(std::iter::once(&path.to_path_buf()))
            .map(|p| self.loader.display_name(p))
            .collect();
        let mut d = Diag::error("kiln::graph", "include cycle")
            .help(format!("the cycle is: {}", names.join(" → ")));
        if let Some(o) = at {
            d = d.label(o, "closes the cycle here");
        }
        self.errs.push(d);
    }

    /// `include` must be a top-level array of strings; anything else is reported
    /// by `structure::check` and skipped here.
    fn include_list(&mut self, doc: &Node) -> Vec<kiln_diag::Spanned<String>> {
        let Some(entry) = doc.as_table().and_then(|t| t.get("include")) else {
            return Vec::new();
        };
        let Some(items) = entry.value.as_array() else {
            self.errs.push(
                Diag::error(
                    "kiln::structure",
                    format!(
                        "`include` must be a list, found {}",
                        entry.value.type_name()
                    ),
                )
                .label(&entry.value.origin, "here")
                .help("`include = [\"hardware.toml\", \"@kiln/profiles/minimal\"]`"),
            );
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| match &item.kind {
                NodeKind::Str(s) => Some(kiln_diag::Spanned::new(s.clone(), item.origin.clone())),
                _ => {
                    self.errs.push(
                        Diag::error(
                            "kiln::structure",
                            format!("`include` takes strings, found {}", item.type_name()),
                        )
                        .label(&item.origin, "here"),
                    );
                    None
                }
            })
            .collect()
    }
}
