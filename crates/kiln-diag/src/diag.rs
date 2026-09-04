//! Rendered diagnostics.
//!
//! A Kiln diagnostic can point at several places in several files at once —
//! "set here" in one file and "and here" in another is the single most common
//! shape (rule 3). miette carries one `SourceCode` per diagnostic, so a
//! multi-file diagnostic is rendered as a primary plus one `related` per extra
//! file. That plumbing lives here so no other crate has to know about it.

use crate::source::{Origin, Src};
use miette::{Diagnostic, LabeledSpan, Severity, SourceCode};
use std::fmt;

#[derive(Debug, Clone)]
pub struct Label {
    pub origin: Origin,
    pub text: String,
}

impl Label {
    pub fn new(origin: &Origin, text: impl Into<String>) -> Label {
        Label {
            origin: origin.clone(),
            text: text.into(),
        }
    }
}

/// One problem, with everywhere it can be seen.
#[derive(Debug, Clone)]
pub struct Diag {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub labels: Vec<Label>,
    pub help: Option<String>,
}

impl Diag {
    pub fn error(code: &'static str, message: impl Into<String>) -> Diag {
        Diag {
            code,
            severity: Severity::Error,
            message: message.into(),
            labels: Vec::new(),
            help: None,
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Diag {
        Diag {
            severity: Severity::Warning,
            ..Diag::error(code, message)
        }
    }

    pub fn label(mut self, origin: &Origin, text: impl Into<String>) -> Diag {
        self.labels.push(Label::new(origin, text));
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Diag {
        self.help = Some(help.into());
        self
    }

    pub fn maybe_help(mut self, help: Option<String>) -> Diag {
        self.help = help;
        self
    }

    /// The file the primary labels live in, if any.
    fn primary_file(&self) -> Option<&Src> {
        self.labels.first().map(|l| &l.origin.file)
    }

    fn labels_for(&self, file: &Src) -> Vec<LabeledSpan> {
        self.labels
            .iter()
            .filter(|l| std::sync::Arc::ptr_eq(&l.origin.file, file))
            .map(|l| {
                LabeledSpan::new(
                    Some(l.text.clone()),
                    l.origin.span.start,
                    l.origin.span.len(),
                )
            })
            .collect()
    }

    /// Files after the first, each rendered as its own source block.
    fn extra_files(&self) -> Vec<Src> {
        let mut out: Vec<Src> = Vec::new();
        for l in &self.labels {
            let f = &l.origin.file;
            let seen = self
                .primary_file()
                .is_some_and(|p| std::sync::Arc::ptr_eq(p, f))
                || out.iter().any(|o| std::sync::Arc::ptr_eq(o, f));
            if !seen {
                out.push(f.clone());
            }
        }
        out
    }
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Diag {}

impl Diagnostic for Diag {
    fn code(&self) -> Option<Box<dyn fmt::Display + '_>> {
        Some(Box::new(self.code))
    }

    fn severity(&self) -> Option<Severity> {
        Some(self.severity)
    }

    fn help(&self) -> Option<Box<dyn fmt::Display + '_>> {
        self.help
            .as_ref()
            .map(|h| Box::new(h) as Box<dyn fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.primary_file().map(|f| &f.named as &dyn SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let f = self.primary_file()?;
        Some(Box::new(self.labels_for(f).into_iter()))
    }

    fn related(&self) -> Option<Box<dyn Iterator<Item = &dyn Diagnostic> + '_>> {
        // Rendered lazily would be nicer, but miette wants borrowed trait
        // objects; the parts are cheap and a diagnostic is printed once.
        None
    }
}

/// A single file's worth of a multi-file diagnostic, used when rendering.
pub struct DiagPart {
    file: Src,
    labels: Vec<LabeledSpan>,
}

impl fmt::Debug for DiagPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.file.name)
    }
}
impl fmt::Display for DiagPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "also in {}", self.file.name)
    }
}
impl std::error::Error for DiagPart {}
impl Diagnostic for DiagPart {
    fn source_code(&self) -> Option<&dyn SourceCode> {
        Some(&self.file.named as &dyn SourceCode)
    }
    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        Some(Box::new(self.labels.clone().into_iter()))
    }
}

impl Diag {
    /// The extra-file blocks, for a renderer that wants to print them after the
    /// primary. Kept separate from `related()` because miette's signature there
    /// cannot return owned values.
    pub fn parts(&self) -> Vec<DiagPart> {
        self.extra_files()
            .into_iter()
            .map(|file| {
                let labels = self.labels_for(&file);
                DiagPart { file, labels }
            })
            .collect()
    }
}
