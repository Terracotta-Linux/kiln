//! AUR resolution. (*recorded HTTP fixtures, never the
//! network*).

use kiln_aur::closure::{self, Request, MAX_DEPTH};
use kiln_aur::{resolve, Recorded};

/// One RPC reply. Written as a helper rather than a file per case because the
/// interesting variable is the *dependency shape*, and putting that in JSON
/// files would hide it from the test that depends on it.
fn reply(packages: &[(&str, &str, &[&str])]) -> String {
    let results: Vec<String> = packages
        .iter()
        .map(|(name, version, deps)| {
            let deps: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
            format!(
                r#"{{"Name":"{name}","PackageBase":"{name}","Version":"{version}",
                    "Maintainer":"someone","LastModified":1756000000,
                    "Depends":[{}],"MakeDepends":[],"License":["MIT"]}}"#,
                deps.join(",")
            )
        })
        .collect();
    format!(
        r#"{{"resultcount":{},"results":[{}],"type":"multiinfo","version":5}}"#,
        results.len(),
        results.join(",")
    )
}

fn nothing_official(_: &str) -> bool {
    false
}

#[test]
fn a_package_resolves_to_its_version_and_its_commit() {
    let transport = Recorded::new()
        .with_rpc(reply(&[("zen-browser-bin", "1.16.3-1", &[])]))
        .with_head(
            "zen-browser-bin",
            "3f1a9c8e2b4d6f8a0c1e3f5a7b9d1e3f5a7b9d1e",
        );

    let closure = resolve(
        &Request::new([("zen-browser-bin".to_string(), None)]),
        &transport,
        &nothing_official,
    )
    .unwrap();

    let pkg = closure.get("zen-browser-bin").unwrap();
    assert_eq!(pkg.version, "1.16.3-1");
    assert_eq!(pkg.commit, "3f1a9c8e2b4d6f8a0c1e3f5a7b9d1e3f5a7b9d1e");
    assert_eq!(pkg.pulled_in_by, None, "the configuration named it");
}

/// *"Identity is the AUR git commit, not the version string. This makes
/// 'the maintainer force-pushed a different PKGBUILD with the same pkgver' a
/// detected change."*
#[test]
fn a_force_push_at_an_unchanged_version_is_a_different_resolution() {
    let at = |oid: &str| {
        let transport = Recorded::new()
            .with_rpc(reply(&[("thing", "1.0-1", &[])]))
            .with_head("thing", oid);
        resolve(
            &Request::new([("thing".to_string(), None)]),
            &transport,
            &nothing_official,
        )
        .unwrap()
        .get("thing")
        .unwrap()
        .clone()
    };

    let before = at("1111111111111111111111111111111111111111");
    let after = at("2222222222222222222222222222222222222222");
    assert_eq!(before.version, after.version, "the version did not move");
    assert_ne!(before.commit, after.commit, "but the recipe did");
}

/// a pin is a statement about what to build, so HEAD moving does not
/// matter — which is the entire point of pinning.
#[test]
fn a_pinned_commit_is_used_without_consulting_the_remote() {
    // No recorded HEAD at all: reaching for one would fail the test.
    let transport = Recorded::new().with_rpc(reply(&[("foo-git", "r120.abc-1", &[])]));

    let closure = resolve(
        &Request::new([("foo-git".to_string(), Some("a81fc2e".to_string()))]),
        &transport,
        &nothing_official,
    )
    .unwrap();
    assert_eq!(closure.get("foo-git").unwrap().commit, "a81fc2e");
}

/// *"Nothing enters the image anonymously."*
#[test]
fn a_transitive_package_records_what_pulled_it_in() {
    let transport = Recorded::new()
        .with_rpc(reply(&[
            ("top", "1.0-1", &["middle", "glibc"]),
            ("middle", "2.0-1", &["bottom"]),
            ("bottom", "3.0-1", &[]),
        ]))
        .with_head("top", &"a".repeat(40))
        .with_head("middle", &"b".repeat(40))
        .with_head("bottom", &"c".repeat(40));

    let closure = resolve(
        &Request::new([("top".to_string(), None)]),
        &transport,
        // `glibc` is in the official repositories, so it is libalpm's problem.
        &|name| name == "glibc",
    )
    .unwrap();

    assert_eq!(
        closure.packages.len(),
        3,
        "glibc must not enter the closure"
    );
    assert_eq!(
        closure.get("middle").unwrap().pulled_in_by.as_deref(),
        Some("top")
    );
    assert_eq!(
        closure.get("bottom").unwrap().pulled_in_by.as_deref(),
        Some("middle")
    );
    // `kiln check` prints the chain, not a bare list.
    assert_eq!(closure.chain_to("bottom"), ["top", "middle", "bottom"]);
}

/// *"batched — one HTTP request for every AUR package in the manifest"*.
/// A closure costs one request per level, not one per package.
#[test]
fn the_rpc_is_batched_rather_than_one_request_per_package() {
    let transport = Recorded::new()
        .with_rpc(reply(&[
            ("a", "1-1", &[]),
            ("b", "1-1", &[]),
            ("c", "1-1", &[]),
        ]))
        .with_head("a", &"a".repeat(40))
        .with_head("b", &"b".repeat(40))
        .with_head("c", &"c".repeat(40));

    resolve(
        &Request::new([
            ("a".to_string(), None),
            ("b".to_string(), None),
            ("c".to_string(), None),
        ]),
        &transport,
        &nothing_official,
    )
    .unwrap();

    assert_eq!(
        transport.request_count(),
        1,
        "three packages at one depth is one request"
    );
    let asked = transport.requests.borrow()[0].clone();
    for name in ["a", "b", "c"] {
        assert!(asked.contains(&format!("arg%5B%5D={name}")), "{asked}");
    }
}

/// A dependency cycle in the AUR is not hypothetical, and it must terminate
/// rather than issue requests until something gives out.
#[test]
fn a_dependency_cycle_terminates() {
    let transport = Recorded::new()
        .with_rpc(reply(&[
            ("loop-a", "1-1", &["loop-b"]),
            ("loop-b", "1-1", &["loop-a"]),
        ]))
        .with_head("loop-a", &"a".repeat(40))
        .with_head("loop-b", &"b".repeat(40));

    let closure = resolve(
        &Request::new([("loop-a".to_string(), None)]),
        &transport,
        &nothing_official,
    )
    .unwrap();
    assert_eq!(closure.packages.len(), 2);
    assert_eq!(
        closure.get("loop-b").unwrap().pulled_in_by.as_deref(),
        Some("loop-a")
    );
}

/// *"Kiln does not pretend otherwise."* A VCS package's version comes
/// from running upstream code, so it is reported as unresolvable rather than
/// guessed.
#[test]
fn a_vcs_package_is_marked_volatile_rather_than_trusted() {
    let transport = Recorded::new()
        .with_rpc(reply(&[
            ("foo-git", "r120.abc1234-1", &[]),
            ("plain", "1.0-1", &[]),
        ]))
        .with_head("foo-git", &"a".repeat(40))
        .with_head("plain", &"b".repeat(40));

    let closure = resolve(
        &Request::new([("foo-git".to_string(), None), ("plain".to_string(), None)]),
        &transport,
        &nothing_official,
    )
    .unwrap();

    assert_eq!(closure.volatile.len(), 1);
    assert_eq!(closure.volatile[0].0, "foo-git");
    assert!(closure.volatile[0].1.contains("only known after fetching"));
    // It is still resolved — the *commit* is knowable even when the version is
    // not, and that is what identity is anyway.
    assert_eq!(closure.get("foo-git").unwrap().commit, "a".repeat(40));
}

#[test]
fn the_vcs_heuristic_errs_toward_marking_volatile() {
    for name in ["foo-git", "bar-svn", "baz-hg", "thing-bzr", "x-nightly"] {
        assert!(closure::is_vcs(name), "{name}");
    }
    for name in ["digikam", "git", "gitui", "legit"] {
        assert!(!closure::is_vcs(name), "{name}");
    }
}

#[test]
fn a_dependency_specification_resolves_to_a_bare_name() {
    assert_eq!(closure::bare_name("foo>=1.2"), "foo");
    assert_eq!(closure::bare_name("foo=1.2-1"), "foo");
    assert_eq!(closure::bare_name("foo<2"), "foo");
    assert_eq!(closure::bare_name("foo: needed for bar"), "foo");
    assert_eq!(closure::bare_name("foo"), "foo");
}

#[test]
fn a_missing_package_names_what_wanted_it() {
    let transport = Recorded::new()
        .with_rpc(reply(&[("top", "1.0-1", &["gone"])]))
        .with_head("top", &"a".repeat(40));

    let err = resolve(
        &Request::new([("top".to_string(), None)]),
        &transport,
        &nothing_official,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the AUR has no `gone`, which `top` requires"
    );
}

/// The cycle check makes the cap unnecessary for correctness, so what it
/// actually guards against is a chain — legitimate but absurd, or hostile.
#[test]
fn an_absurd_dependency_chain_is_refused() {
    let chain: Vec<(String, String, Vec<String>)> = (0..=MAX_DEPTH + 2)
        .map(|i| {
            (
                format!("link{i}"),
                "1-1".to_string(),
                vec![format!("link{}", i + 1)],
            )
        })
        .collect();
    let as_refs: Vec<(&str, &str, Vec<&str>)> = chain
        .iter()
        .map(|(n, v, d)| {
            (
                n.as_str(),
                v.as_str(),
                d.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let body = reply(
        &as_refs
            .iter()
            .map(|(n, v, d)| (*n, *v, d.as_slice()))
            .collect::<Vec<_>>(),
    );

    let mut transport = Recorded::new().with_rpc(body);
    for i in 0..=MAX_DEPTH + 2 {
        transport = transport.with_head(&format!("link{i}"), &"a".repeat(40));
    }

    let err = resolve(
        &Request::new([("link0".to_string(), None)]),
        &transport,
        &nothing_official,
    )
    .unwrap_err();
    assert!(err.to_string().contains("more than 10 deep"), "{err}");
}
