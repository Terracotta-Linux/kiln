//! Source provenance.
//!
//! Every leaf value carries the file and byte range it came from, for the entire
//! life of the frontend. That is the whole reason error messages can be good; it
//! is not free, and it is worth it.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A file that took part in the configuration. Refcounted, and holds its own
/// text so a diagnostic can render the offending line without re-reading disk
/// (the file may have changed, or may not be readable any more).
#[derive(Debug)]
pub struct SourceFile {
    /// Absolute, symlink-resolved path.
    pub path: PathBuf,
    /// What the user should see: config-root-relative, or `@kiln/...` for a
    /// shipped module. Never an absolute path if we can avoid it.
    pub name: String,
    pub text: String,
    /// The same text, named, so a rendered diagnostic says
    /// `╭─[desktop.toml:7:11]` rather than `╭─[7:11]`. With several files in one
    /// diagnostic — the common case for a sibling conflict — the name is the
    /// whole point.
    pub named: miette::NamedSource<String>,
}

pub type Src = Arc<SourceFile>;

impl SourceFile {
    pub fn new(path: impl Into<PathBuf>, name: impl Into<String>, text: impl Into<String>) -> Src {
        let name = name.into();
        let text = text.into();
        let named = miette::NamedSource::new(&name, text.clone()).with_language("TOML");
        Arc::new(SourceFile {
            path: path.into(),
            name,
            text,
            named,
        })
    }

    /// 1-based line and column for a byte offset, for `file:line:col` rendering.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let upto = &self.text[..offset.min(self.text.len())];
        let line = upto.matches('\n').count() + 1;
        let col = upto.rsplit('\n').next().map_or(0, str::chars_count_) + 1;
        (line, col)
    }
}

// Tiny extension so `line_col` reads cleanly; `str::chars().count()` inline is noisier.
trait CharsCount {
    fn chars_count_(&self) -> usize;
}
impl CharsCount for str {
    fn chars_count_(&self) -> usize {
        self.chars().count()
    }
}

/// Where a value came from.
#[derive(Debug, Clone)]
pub struct Origin {
    pub file: Src,
    pub span: Range<usize>,
}

impl Origin {
    pub fn new(file: Src, span: Range<usize>) -> Origin {
        Origin { file, span }
    }

    /// `hardware.toml:14` — the form `kiln explain` prints.
    pub fn short(&self) -> String {
        let (line, _) = self.file.line_col(self.span.start);
        format!("{}:{}", self.file.name, line)
    }

    pub fn path(&self) -> &Path {
        &self.file.path
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (line, col) = self.file.line_col(self.span.start);
        write!(f, "{}:{}:{}", self.file.name, line, col)
    }
}

/// A value plus where it came from.
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    pub value: T,
    pub origin: Origin,
}

impl<T> Spanned<T> {
    pub fn new(value: T, origin: Origin) -> Self {
        Spanned { value, origin }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Spanned<U> {
        Spanned {
            value: f(self.value),
            origin: self.origin,
        }
    }

    pub fn as_ref(&self) -> Spanned<&T> {
        Spanned {
            value: &self.value,
            origin: self.origin.clone(),
        }
    }
}

impl<T> std::ops::Deref for Spanned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

/// Equality and ordering ignore provenance: two values are the same value
/// wherever they were written. Without this, `config_id` would depend on which
/// file a package name came from.
impl<T: PartialEq> PartialEq for Spanned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl<T: Eq> Eq for Spanned<T> {}
impl<T: PartialOrd> PartialOrd for Spanned<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.value.partial_cmp(&other.value)
    }
}
impl<T: Ord> Ord for Spanned<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

/// Where a key's effective value came from, and what else had a say.
/// Produced by merge, carried by the Manifest for diagnostics only, and
/// excluded from hashing.
///
/// The two cases are genuinely different and `kiln explain` must not blur them:
/// a scalar has one winner and some losers, a list has several contributors and
/// no winner at all.
#[derive(Debug, Clone)]
pub struct Provenance {
    /// For a scalar, the file whose value is in the Manifest. For a list, the
    /// nearest file that contributes to it.
    pub effective: Origin,
    /// Nearest first. Values the effective one displaced (scalar), or the rest
    /// of the contributors (list).
    pub others: Vec<Origin>,
    /// Whether this key unions (rule 1) rather than overriding (rule 2).
    pub is_list: bool,
}

/// Dotted key → provenance. This is what `kiln explain` reads.
pub type OriginMap = std::collections::BTreeMap<String, Provenance>;
