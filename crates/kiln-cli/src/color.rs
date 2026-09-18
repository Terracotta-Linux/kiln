//! Terminal color for CLI output, in one place.
//!
//! `kiln_diag::report::render`'s nocolor choice is permanent — insta
//! snapshots of rendered diagnostics have to be stable across terminals and
//! CI — and this module does not touch it. It governs everything printed
//! *around* those diagnostics instead: the `error`/`warning`/`note` tags,
//! headings, and success confirmations. `NO_COLOR` and a non-terminal stream
//! both turn color off, checked separately for stdout and stderr since a
//! script can redirect one and leave the other a terminal.

use std::io::IsTerminal;

#[derive(Clone, Copy)]
pub enum Stream {
    Out,
    Err,
}

impl Stream {
    pub fn colored(self) -> bool {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        self.is_tty()
    }

    /// Whether this stream is attached to a terminal, ignoring `NO_COLOR` —
    /// `NO_COLOR` says "no escape codes", not "no redrawn progress line", so
    /// a redraw-vs-plain-lines decision checks this instead of `colored()`.
    pub fn is_tty(self) -> bool {
        match self {
            Stream::Out => std::io::stdout().is_terminal(),
            Stream::Err => std::io::stderr().is_terminal(),
        }
    }
}

/// The escape codes below, exposed for arbitrary text — not just the fixed
/// `error`/`warning`/`note` words — so a whole clause of a message (not only
/// its tag) can be colored, like `/etc` drift's changed-file count.
fn paint(stream: Stream, code: &str, text: &str) -> String {
    if stream.colored() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Bold red, for arbitrary text.
pub fn red(stream: Stream, text: &str) -> String {
    paint(stream, "1;31", text)
}

/// Bold yellow, for arbitrary text.
pub fn yellow(stream: Stream, text: &str) -> String {
    paint(stream, "1;33", text)
}

/// Bold green, for arbitrary text.
pub fn green(stream: Stream, text: &str) -> String {
    paint(stream, "1;32", text)
}

/// Bold cyan, for arbitrary text — a change that is neither an addition, a
/// removal nor a version move (`kiln check`'s "rebuild" category: the input
/// itself is unchanged, but something upstream of its build key moved).
pub fn cyan(stream: Stream, text: &str) -> String {
    paint(stream, "1;36", text)
}

/// Bold red `error`. Always stderr — every error in the CLI is printed with
/// `eprint!`/`eprintln!`.
pub fn error() -> String {
    red(Stream::Err, "error")
}

/// Bold yellow `warning`. Takes a stream because most warnings go to
/// stderr, but a few (`kiln rebuild`'s drift/non-determinism reports) are
/// part of the command's normal stdout output.
pub fn warning(stream: Stream) -> String {
    yellow(stream, "warning")
}

/// Bold yellow `note`, the same color as `warning` — a note is a warning
/// that isn't the user's fault to fix.
pub fn note(stream: Stream) -> String {
    yellow(stream, "note")
}

/// Bold green, for a short phrase confirming something worked
/// (`kiln config set` writing a file, a generation staged for boot, ...).
pub fn success(text: &str) -> String {
    green(Stream::Out, text)
}

/// Bold, undecorated — headings and labels that aren't errors or warnings.
pub fn bold(stream: Stream, text: &str) -> String {
    paint(stream, "1", text)
}

/// Faint — secondary information that matters less than what is next to it,
/// like a `file:line` origin next to the value it explains.
pub fn dim(stream: Stream, text: &str) -> String {
    paint(stream, "2", text)
}
