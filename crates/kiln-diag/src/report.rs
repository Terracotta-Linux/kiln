//! Error taxonomy, exit codes, and deterministic rendering.

use crate::diag::Diag;
use miette::{GraphicalReportHandler, GraphicalTheme};
use std::fmt::Write as _;

/// Each phase reports **every** error it finds before stopping:
/// nothing is more tiring than fixing typos one build at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Discovery,
    Syntax,
    Structure,
    Graph,
    Merge,
    Semantic,
    Resolution,
    Assembly,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Discovery => "discovery",
            Phase::Syntax => "syntax",
            Phase::Structure => "structure",
            Phase::Graph => "graph",
            Phase::Merge => "merge",
            Phase::Semantic => "semantic",
            Phase::Resolution => "resolution",
            Phase::Assembly => "assembly",
        }
    }
}

/// `kiln check` returning 10 for "there are changes" is what makes
/// `kiln check && echo current` work in a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    Config = 1,
    Resolution = 2,
    Build = 3,
    System = 4,
    ChangesFound = 10,
}

impl ExitCode {
    pub fn code(self) -> i32 {
        self as i32
    }
}

impl From<Phase> for ExitCode {
    fn from(p: Phase) -> ExitCode {
        match p {
            Phase::Resolution => ExitCode::Resolution,
            Phase::Assembly => ExitCode::Build,
            _ => ExitCode::Config,
        }
    }
}

/// Everything one phase found. Accumulated, not thrown at the first problem.
#[derive(Debug, Default)]
pub struct Errors {
    pub diags: Vec<Diag>,
}

impl Errors {
    pub fn new() -> Errors {
        Errors::default()
    }

    pub fn push(&mut self, d: Diag) {
        self.diags.push(d);
    }

    pub fn extend(&mut self, other: Errors) {
        self.diags.extend(other.diags);
    }

    pub fn has_errors(&self) -> bool {
        self.diags
            .iter()
            .any(|d| d.severity == miette::Severity::Error)
    }

    pub fn is_empty(&self) -> bool {
        self.diags.is_empty()
    }

    pub fn len(&self) -> usize {
        self.diags.len()
    }

    /// Deterministic order: by file name, then byte offset, then code. Snapshot
    /// tests depend on this, and so does anyone diffing two runs.
    pub fn sorted(mut self) -> Errors {
        self.diags.sort_by(|a, b| {
            let key = |d: &Diag| {
                d.labels
                    .first()
                    .map(|l| (l.origin.file.name.clone(), l.origin.span.start))
                    .unwrap_or_default()
            };
            key(a).cmp(&key(b)).then_with(|| a.code.cmp(b.code))
        });
        self
    }

    pub fn into_result<T>(self, ok: T) -> Result<T, Errors> {
        if self.has_errors() {
            Err(self.sorted())
        } else {
            Ok(ok)
        }
    }
}

/// Render one diagnostic to a string with no colour and a fixed width, so that
/// `insta` snapshots of *rendered* diagnostics are stable across terminals and
/// CI. diagnostics that nobody tests rot into `Error: InvalidConfig`.
pub fn render(diag: &Diag) -> String {
    let handler = GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor())
        .with_width(90)
        .with_context_lines(1);
    let mut out = String::new();
    let _ = handler.render_report(&mut out, diag);
    for part in diag.parts() {
        let _ = handler.render_report(&mut out, &part);
    }
    out
}

pub fn render_all(errors: &Errors) -> String {
    let mut out = String::new();
    for d in &errors.diags {
        out.push_str(&render(d));
    }
    if errors.diags.len() > 1 {
        // "problems" only when at least one of them is one. A set of pure
        // warnings is what a *successful* build prints — the seeded-target note,
        // say — and telling someone their working configuration has
        // three problems is a small lie that costs them a debugging session.
        let noun = if errors.has_errors() {
            "problems"
        } else {
            "warnings"
        };
        let _ = writeln!(out, "\n{} {noun} found.", errors.diags.len());
    }
    out
}
