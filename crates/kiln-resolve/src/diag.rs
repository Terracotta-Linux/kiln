//! Resolution failures, pointed at the line that caused them.
//!
//! `kiln-alpm` reports structured values naming packages and dependencies. This
//! is where they become diagnostics, because this is the first layer that holds
//! the Manifest and therefore knows that `nvidai` was written at
//! `hardware.toml:7:12`. Underlining the word rather than the array is the
//! whole reason `item_origins` exists.

use kiln_alpm::Error as AlpmError;
use kiln_diag::{did_you_mean, Diag, Errors, Origin};
use kiln_manifest::Manifest;

/// Where a package name was written, if the configuration wrote it. A package
/// pulled in as a dependency has no origin, which is not a failure — it is the
/// difference between "you asked for this" and "something you asked for did".
pub fn origin_of<'m>(manifest: &'m Manifest, list: &str, item: &str) -> Option<&'m Origin> {
    manifest.item_origins.get(&format!("{list}/{item}"))
}

/// Every package name the configuration wrote, for suggestions.
fn requested(manifest: &Manifest) -> Vec<&str> {
    manifest.packages.repo.iter().map(String::as_str).collect()
}

pub fn to_diag(manifest: &Manifest, err: &AlpmError, known: &[String]) -> Diag {
    match err {
        AlpmError::NotFound { name } => {
            let mut d = Diag::error("kiln::resolution", format!("no package named `{name}`"));
            if let Some(o) = origin_of(manifest, "packages.repo", name) {
                d = d.label(o, "not in any configured repository");
            }
            // A typo in a package name is the single most common resolution
            // failure, and the repository has the whole namespace to suggest
            // from — so suggest from what the repositories actually hold, not
            // from what the config already says.
            d.maybe_help(
                did_you_mean(name, known.iter().map(String::as_str))
                    .or_else(|| did_you_mean(name, requested(manifest))),
            )
        }

        AlpmError::Unsatisfied { wanted_by, dep } => {
            let mut d = Diag::error(
                "kiln::resolution",
                match wanted_by {
                    Some(w) => format!("`{w}` requires `{dep}`, which nothing provides"),
                    None => format!("nothing provides `{dep}`"),
                },
            );
            if let Some(o) = wanted_by
                .as_deref()
                .and_then(|w| origin_of(manifest, "packages.repo", w))
            {
                d = d.label(o, "requested here");
            }
            d.help(
                "the package exists but its dependency does not resolve — usually a \
                 repository that is not enabled, or a partial mirror",
            )
        }

        AlpmError::Conflict {
            first,
            second,
            reason,
        } => {
            let mut d = Diag::error(
                "kiln::resolution",
                format!("`{first}` and `{second}` cannot both be in one image"),
            );
            for name in [first, second] {
                if let Some(o) = origin_of(manifest, "packages.repo", name) {
                    d = d.label(o, format!("`{name}` requested here"));
                }
            }
            // When the conflict is declared on one of the two names, saying so
            // adds nothing the labels have not already said. It is worth a
            // sentence only when the conflict runs through a third name — a
            // virtual package — because then neither label explains itself.
            d.help(if reason == first || reason == second {
                "remove one of them, or exclude one with `packages.exclude`".to_string()
            } else {
                format!(
                    "they both lay claim to `{reason}`; remove one, or exclude one with \
                     `packages.exclude`"
                )
            })
        }

        AlpmError::Excluded { name, pulled_in_by } => {
            let mut d = Diag::error(
                "kiln::resolution",
                format!("`{name}` is excluded, but the image would contain it"),
            );
            if let Some(o) = origin_of(manifest, "packages.exclude", name) {
                d = d.label(o, "excluded here");
            }
            for by in pulled_in_by {
                if let Some(o) = origin_of(manifest, "packages.repo", by) {
                    d = d.label(o, format!("`{by}` requires it"));
                }
            }
            // The labels already say what requires it, so the help says only
            // the thing they cannot: why Kiln refuses instead of just dropping
            // the dependency.
            d.help(match pulled_in_by.as_slice() {
                [] => format!(
                    "`{name}` is named directly in `packages.repo` as well — remove it \
                     from one list or the other"
                ),
                _ => "Kiln will not drop a dependency to satisfy an exclusion: that \
                      produces an image missing something a program links against. \
                      Remove the exclusion, or remove what needs it."
                    .to_string(),
            })
        }

        AlpmError::WrongArch { name, arch } => Diag::error(
            "kiln::resolution",
            format!("`{name}` is built for {arch}, not {}", manifest.image.arch),
        )
        .maybe_help(
            origin_of(manifest, "packages.repo", name)
                .map(|_| format!("`image.arch` is {}", manifest.image.arch)),
        ),

        AlpmError::Refresh { repo, message } => Diag::error(
            "kiln::resolution",
            format!("could not refresh `{repo}`: {message}"),
        )
        .help("check the network, or resolve against cached metadata with `--offline`"),

        AlpmError::Alpm { doing, message } => {
            Diag::error("kiln::resolution", format!("{doing}: {message}"))
        }

        // Assembly-phase failures. They cannot arise from `solve`, which
        // installs nothing — but they are matched explicitly rather than swept
        // into a `_` arm, so that adding a variant to the taxonomy makes the
        // compiler ask where it belongs instead of letting it fall silently
        // into a generic message.
        AlpmError::FileConflict { .. }
        | AlpmError::PackageInvalid { .. }
        | AlpmError::UnreadablePackage { .. }
        | AlpmError::Mount { .. }
        | AlpmError::NotRoot
        | AlpmError::TransactionErrors { .. } => Diag::error("kiln::assembly", err.to_string()),
    }
}

pub fn one(d: Diag) -> Errors {
    let mut e = Errors::new();
    e.push(d);
    e
}
