//! Repositories, mirrors, and how much a signature is trusted.

use std::fmt;

/// One registered sync database and where to fetch it from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSpec {
    pub name: String,
    /// Already expanded: no `$repo` or `$arch` survives into a `RepoSpec`, so
    /// what is registered with libalpm is what a human can paste into a browser.
    pub servers: Vec<String>,
    pub trust: Trust,
}

impl RepoSpec {
    pub fn new(name: impl Into<String>, servers: Vec<String>, trust: Trust) -> RepoSpec {
        RepoSpec {
            name: name.into(),
            servers,
            trust,
        }
    }
}

/// databases **and** packages required and trusted is the default, and
/// there is deliberately no `TRUST_ALL`.
///
/// `Unsigned` exists for two honest cases — the in-tree test fixture, and a
/// local repository the user has declared without a key — and is named rather
/// than expressed as a bare `SigLevel` so that grepping for it finds every
/// place Kiln accepts unsigned input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Trust {
    #[default]
    Required,
    Unsigned,
}

impl Trust {
    pub(crate) fn siglevel(self) -> alpm::SigLevel {
        use alpm::SigLevel;
        match self {
            // Every *package* signature is required, and the database
            // signature is optional — which is pacman's own default for the
            // Arch repositories, and not laxity. **Arch does not sign its
            // repository databases**: `core.db.sig` is a 404 on every mirror,
            // and requiring one does not make Kiln stricter, it makes every
            // refresh fail with libalpm's "failed to retrieve some files",
            // which says nothing about signatures at all.
            //
            // What that costs is bounded, because the database is not the
            // thing being trusted. It carries filenames and checksums; every
            // package it names is verified against a required signature before
            // it is installed. A hostile mirror can offer an old or truncated
            // database — which is a downgrade, worth defending against
            // separately and not by a signature Arch does not publish — but it
            // cannot get unsigned code into the image.
            //
            // Neither `*_MARGINAL_OK` nor `*_UNKNOWN_OK` is set: a key Kiln
            // cannot fully validate is not a key it trusts.
            Trust::Required => SigLevel::PACKAGE | SigLevel::DATABASE_OPTIONAL,
            Trust::Unsigned => SigLevel::empty(),
        }
    }
}

impl fmt::Display for Trust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Trust::Required => "signed",
            Trust::Unsigned => "unsigned",
        })
    }
}

/// Mirror URL construction. pacman's `$repo` / `$arch` convention, plus the two
/// server families Kiln knows how to build for itself.
pub mod mirrors {
    /// the default when `repos.mirrors` is empty. The geo mirror is
    /// deterministic and works everywhere, including non-Arch hosts and CI,
    /// which is exactly what a build tool needs and what a country-specific
    /// mirrorlist is not.
    pub const GEO: &str = "https://geo.mirror.pkgbuild.com/$repo/os/$arch";

    /// `repos.snapshot = "2026-08-24"` resolves from the Archive. The date
    /// has already been validated as `YYYY-MM-DD` by the frontend.
    pub fn archive(date: &str) -> Option<String> {
        let (y, rest) = date.split_once('-')?;
        let (m, d) = rest.split_once('-')?;
        Some(format!(
            "https://archive.archlinux.org/repos/{y}/{m}/{d}/$repo/os/$arch"
        ))
    }

    /// Expand pacman's two variables. Nothing else is substituted: this is a
    /// URL template, not a language.
    pub fn expand(template: &str, repo: &str, arch: &str) -> String {
        template.replace("$repo", repo).replace("$arch", arch)
    }

    /// A local directory as a server URL, for the test fixture and for a
    /// user's own `repos.extra` pointing at a path.
    pub fn file(path: &std::path::Path) -> String {
        format!("file://{}", path.display())
    }

    /// The repositories every Arch system has, in the order pacman lists them —
    /// which is also priority order, so it is not incidental.
    pub const OFFICIAL: [&str; 2] = ["core", "extra"];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_both_variables() {
        assert_eq!(
            mirrors::expand(mirrors::GEO, "core", "x86_64"),
            "https://geo.mirror.pkgbuild.com/core/os/x86_64"
        );
    }

    #[test]
    fn archive_url_splits_the_date() {
        assert_eq!(
            mirrors::archive("2026-08-24").as_deref(),
            Some("https://archive.archlinux.org/repos/2026/08/24/$repo/os/$arch")
        );
        assert_eq!(mirrors::archive("latest"), None);
    }

    /// Signature enforcement is a security property, so it gets a test rather
    /// than a comment.
    ///
    /// Every *package* signature is required and no weakening flag is set. The
    /// database signature is optional, because **Arch does not publish one** —
    /// `core.db.sig` is a 404 on every mirror — so requiring it does not make
    /// Kiln stricter, it makes every refresh fail. What that costs is bounded:
    /// the database carries filenames and checksums, and every package it names
    /// is verified against a required signature before it is installed.
    #[test]
    fn required_trust_demands_every_package_signature() {
        use alpm::SigLevel;
        let s = Trust::Required.siglevel();
        assert!(s.contains(SigLevel::PACKAGE));
        for weakening in [
            SigLevel::PACKAGE_OPTIONAL,
            SigLevel::PACKAGE_MARGINAL_OK,
            SigLevel::PACKAGE_UNKNOWN_OK,
            SigLevel::DATABASE_MARGINAL_OK,
            SigLevel::DATABASE_UNKNOWN_OK,
            SigLevel::USE_DEFAULT,
        ] {
            assert!(
                !s.contains(weakening),
                "Required must not set {weakening:?}"
            );
        }
    }

    /// The one deliberate concession, stated as a test so it cannot be
    /// tightened back by someone reading `DATABASE_OPTIONAL` as a mistake.
    /// Setting `DATABASE` instead makes `kiln check` fail on every machine with
    /// "failed to retrieve some files", which names neither the database nor
    /// the signature.
    #[test]
    fn the_database_signature_is_optional_because_arch_publishes_none() {
        use alpm::SigLevel;
        let s = Trust::Required.siglevel();
        assert!(s.contains(SigLevel::DATABASE_OPTIONAL));
        assert!(!s.contains(SigLevel::DATABASE));
    }

    /// `Unsigned` must be exactly "no verification", not "verify and believe
    /// anything" — the two look alike in a config file and are not alike at all.
    #[test]
    fn unsigned_trust_asks_for_nothing_rather_than_trusting_everything() {
        let s = Trust::Unsigned.siglevel();
        assert!(!s.contains(alpm::SigLevel::PACKAGE));
        assert!(!s.contains(alpm::SigLevel::DATABASE));
        assert!(!s.contains(alpm::SigLevel::PACKAGE_UNKNOWN_OK));
        assert!(!s.contains(alpm::SigLevel::DATABASE_UNKNOWN_OK));
    }
}
