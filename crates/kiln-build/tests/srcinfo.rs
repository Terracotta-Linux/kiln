//! Reading what a PKGBUILD declares, without running it.

use kiln_build::srcinfo::{self, Source};

/// A split package with architecture-suffixed sources — the shape that makes
/// the positional checksum correspondence non-obvious.
const SPLIT: &str = "\
pkgbase = mytool
\tpkgdesc = A tool
\tpkgver = 1.2.0
\tpkgrel = 3
\tepoch = 1
\tarch = x86_64
\tarch = aarch64
\tmakedepends = gcc
\tmakedepends = cmake
\tdepends = glibc
\tsource = https://example.invalid/mytool-1.2.0.tar.gz
\tsha256sums = aaaa
\tsource = mytool.patch
\tsha256sums = bbbb
\tsource_x86_64 = https://example.invalid/blob-x86_64.bin
\tsha256sums_x86_64 = cccc
\tsource_aarch64 = https://example.invalid/blob-aarch64.bin
\tsha256sums_aarch64 = dddd

pkgname = mytool
\tdepends = glibc

pkgname = mytool-docs
\tdepends =
";

#[test]
fn a_split_package_reports_every_name_and_one_version() {
    let s = srcinfo::parse(SPLIT, "x86_64").unwrap();
    assert_eq!(s.pkgbase, "mytool");
    assert_eq!(s.pkgnames, ["mytool", "mytool-docs"]);
    assert_eq!(s.evr(), "1:1.2.0-3");
    assert_eq!(s.makedepends, ["gcc", "cmake"]);
}

/// The trap this parser exists for: `sha256sums` is a **parallel list**, not a
/// map, and the arch-suffixed lists are separate parallel lists. Zipping the
/// wrong pair pins the wrong bytes and says nothing about it.
#[test]
fn checksums_line_up_with_the_sources_of_their_own_suffix() {
    let s = srcinfo::parse(SPLIT, "x86_64").unwrap();
    assert_eq!(
        s.sources,
        [
            Source {
                spec: "https://example.invalid/mytool-1.2.0.tar.gz".into(),
                sha256: Some("aaaa".into())
            },
            Source {
                spec: "mytool.patch".into(),
                sha256: Some("bbbb".into())
            },
            Source {
                spec: "https://example.invalid/blob-x86_64.bin".into(),
                sha256: Some("cccc".into())
            },
        ]
    );
}

/// …and another architecture's sources are not this image's business. Picking
/// them up would pin bytes that will never be fetched.
#[test]
fn another_architectures_sources_are_ignored() {
    let s = srcinfo::parse(SPLIT, "aarch64").unwrap();
    let specs: Vec<&str> = s.sources.iter().map(|x| x.spec.as_str()).collect();
    assert!(specs.contains(&"https://example.invalid/blob-aarch64.bin"));
    assert!(!specs.iter().any(|x| x.contains("x86_64")));
    assert_eq!(
        s.sources.last().unwrap().sha256.as_deref(),
        Some("dddd"),
        "the aarch64 checksum, not the x86_64 one"
    );
}

/// `SKIP` says the source cannot be verified. That is a different fact
/// from "no checksum was given" and has to survive to `kiln check`.
#[test]
fn skip_makes_a_recipe_volatile_rather_than_merely_unpinned() {
    let text = "\
pkgbase = foo-git
\tpkgver = r120.abc1234
\tpkgrel = 1
\tsource = foo::git+https://example.invalid/foo.git
\tsha256sums = SKIP
";
    let s = srcinfo::parse(text, "x86_64").unwrap();
    assert_eq!(s.sources[0].sha256, None);
    assert!(s.is_volatile());
    assert_eq!(s.volatile_sources().len(), 1);
}

/// A VCS source is volatile even when someone writes a checksum beside it:
/// `pkgver()` still runs upstream code to produce a version.
#[test]
fn a_vcs_source_is_volatile_even_with_a_checksum() {
    let text = "\
pkgbase = foo-git
\tpkgver = r1.0000000
\tpkgrel = 1
\tsource = git+https://example.invalid/foo.git
\tsha256sums = aaaa
";
    assert!(srcinfo::parse(text, "x86_64").unwrap().is_volatile());
}

#[test]
fn an_ordinary_recipe_is_not_volatile() {
    assert!(!srcinfo::parse(SPLIT, "x86_64").unwrap().is_volatile());
}

#[test]
fn a_source_knows_the_filename_it_will_be_saved_as() {
    let named = |spec: &str| Source {
        spec: spec.into(),
        sha256: None,
    };
    assert_eq!(
        named("https://e.invalid/a/foo-1.0.tar.gz").filename(),
        "foo-1.0.tar.gz"
    );
    assert_eq!(named("mytool.patch").filename(), "mytool.patch");
    // `name::url` renames on download, and the rename is the filename that
    // ends up beside the PKGBUILD.
    assert_eq!(
        named("foo::git+https://e.invalid/bar.git").filename(),
        "foo"
    );
    // A query string is not part of the name makepkg saves.
    assert_eq!(
        named("https://e.invalid/foo.tar.gz?rev=2").filename(),
        "foo.tar.gz"
    );
}

#[test]
fn a_local_source_is_distinguished_from_one_to_fetch() {
    let s = srcinfo::parse(SPLIT, "x86_64").unwrap();
    let local: Vec<&str> = s
        .sources
        .iter()
        .filter(|x| x.is_local())
        .map(|x| x.spec.as_str())
        .collect();
    assert_eq!(local, ["mytool.patch"]);
}

/// `.SRCINFO` gains fields as pacman grows. A Kiln that refused to read a
/// recipe because it met an unfamiliar array would be a worse tool than one
/// that reads what it understands.
#[test]
fn unknown_keys_are_ignored_rather_than_rejected() {
    let text = "\
pkgbase = foo
\tpkgver = 1
\tpkgrel = 1
\tsomething_new = whatever
\tb2sums = abcd
";
    assert_eq!(srcinfo::parse(text, "x86_64").unwrap().pkgver, "1");
}

#[test]
fn a_file_that_is_not_a_srcinfo_says_so() {
    let err = srcinfo::parse("hello\n", "x86_64").unwrap_err();
    assert_eq!(err.why, "expected `key = value`");
    assert_eq!(err.line, 1);

    let err = srcinfo::parse("pkgver = 1\n", "x86_64").unwrap_err();
    assert!(err.to_string().contains("not a .SRCINFO"), "{err}");
}

/// A recipe with no explicit `pkgname` produces one package named after the
/// base — makepkg's own rule.
#[test]
fn a_recipe_without_a_split_produces_one_package() {
    let text = "pkgbase = solo\n\tpkgver = 1\n\tpkgrel = 1\n";
    assert_eq!(srcinfo::parse(text, "x86_64").unwrap().pkgnames, ["solo"]);
}
