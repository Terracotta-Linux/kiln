//! The AUR RPC v5 `info` endpoint, and the shape of what it returns.
//!
//! **batched** — one HTTP request for every AUR package in the manifest,
//! not one per package. The endpoint takes repeated `arg[]` parameters and
//! answers with everything at once, which is the difference between a
//! `kiln check` that costs one round trip and one that costs forty.

use serde::Deserialize;
use std::collections::BTreeMap;

pub const ENDPOINT: &str = "https://aur.archlinux.org/rpc/v5/info";

/// The subset of the RPC's reply Kiln uses. This names `Version`, `Depends`,
/// `MakeDepends` and `LastModified`; the rest is carried because the trust
/// summary prints a maintainer and a source count.
///
/// Not `deny_unknown_fields`: the AUR adds fields, and a Kiln that stopped
/// resolving because the RPC learned a new key would be a worse tool than one
/// that reads what it understands. The frontend denies unknown keys because a
/// typo in *your* config is a mistake worth catching; this is somebody else's
/// API, and a new field there is not a mistake at all.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct Info {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "PackageBase")]
    pub package_base: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Maintainer")]
    pub maintainer: Option<String>,
    #[serde(rename = "LastModified", default)]
    pub last_modified: i64,
    #[serde(rename = "OutOfDate")]
    pub out_of_date: Option<i64>,
    #[serde(rename = "Depends", default)]
    pub depends: Vec<String>,
    #[serde(rename = "MakeDepends", default)]
    pub make_depends: Vec<String>,
    #[serde(rename = "CheckDepends", default)]
    pub check_depends: Vec<String>,
    #[serde(rename = "Provides", default)]
    pub provides: Vec<String>,
    #[serde(rename = "Conflicts", default)]
    pub conflicts: Vec<String>,
    #[serde(rename = "License", default)]
    pub license: Vec<String>,
    #[serde(rename = "URL")]
    pub url: Option<String>,
}

impl Info {
    /// Everything this package needs in order to *build*, which is what the
    /// closure walks. This names `Depends` and `MakeDepends`; `CheckDepends`
    /// joins them because `makepkg` runs `check()` unless told not to.
    pub fn all_dependencies(&self) -> Vec<&str> {
        self.depends
            .iter()
            .chain(&self.make_depends)
            .chain(&self.check_depends)
            .map(String::as_str)
            .collect()
    }

    /// The trust summary, printed on the first build of any new AUR package.
    /// Arbitrary code from a stranger deserves one line of daylight.
    pub fn trust_summary(&self, commit: &str, sources: usize) -> String {
        format!(
            "{} {} — pkgbase {}, maintainer {}, commit {}, {} source{}",
            self.name,
            self.version,
            self.package_base,
            self.maintainer.as_deref().unwrap_or("ORPHANED"),
            &commit[..commit.len().min(7)],
            sources,
            if sources == 1 { "" } else { "s" }
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Reply {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(rename = "results", default)]
    results: Vec<Info>,
}

/// Parse an RPC reply into `name → info`.
///
/// A name the AUR does not know is simply absent from `results` — the endpoint
/// does not error for it — so "not found" is the caller's conclusion to draw,
/// with the name it asked for. Returning a map rather than a list is what makes
/// that possible.
pub fn parse(body: &str) -> Result<BTreeMap<String, Info>, Error> {
    let reply: Reply =
        serde_json::from_str(body).map_err(|e| Error::Malformed { why: e.to_string() })?;
    if reply.kind == "error" {
        return Err(Error::Rpc {
            message: reply.error.unwrap_or_else(|| "unspecified".into()),
        });
    }
    Ok(reply
        .results
        .into_iter()
        .map(|info| (info.name.clone(), info))
        .collect())
}

/// Build the batched query URL for a set of names.
pub fn url(names: &[String]) -> String {
    let mut url = String::from(ENDPOINT);
    for (index, name) in names.iter().enumerate() {
        url.push(if index == 0 { '?' } else { '&' });
        url.push_str("arg%5B%5D=");
        url.push_str(&encode(name));
    }
    url
}

/// Percent-encode a package name.
///
/// Package names are `[a-z0-9@._+-]`, so in practice nothing needs encoding —
/// which is exactly why it is done anyway. A name that arrives from a config
/// file and goes into a URL without escaping is how query injection happens,
/// and "the character set forbids it" is a property of a validator somewhere
/// else that this function should not depend on.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Malformed { why: String },
    Rpc { message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Malformed { why } => {
                write!(
                    f,
                    "the AUR returned something that is not an RPC reply: {why}"
                )
            }
            Error::Rpc { message } => write!(f, "the AUR rejected the query: {message}"),
        }
    }
}

impl std::error::Error for Error {}
