//! The dependency solver, against a real local repository.

mod fixture;

use kiln_alpm::{Error, Request};

#[test]
fn resolves_a_dependency_graph() {
    let mut s = fixture::session("solve-graph", ".");
    let sol = s
        .solve(&Request::new(["fixture-base".to_string()]))
        .unwrap();

    assert_eq!(
        sol.names(),
        ["fixture-base", "fixture-filesystem", "fixture-libfoo"]
    );
    // Only what the configuration named is explicit; the rest arrived because
    // something needed it, and the image's package database must say so.
    assert!(sol.get("fixture-base").unwrap().explicit);
    assert!(!sol.get("fixture-libfoo").unwrap().explicit);
}

/// filename and sha256 are recorded for every package regardless of
/// whether anything is pinned, so `kiln rebuild` can be satisfied from the
/// artifact store or the Archive after mirrors have moved on.
#[test]
fn every_package_carries_a_filename_and_a_checksum() {
    let mut s = fixture::session("solve-checksums", ".");
    let sol = s
        .solve(&Request::new(["fixture-base".to_string()]))
        .unwrap();
    for p in &sol.packages {
        assert!(!p.filename.is_empty(), "{} has no filename", p.name);
        assert!(p.sha256.is_some(), "{} has no sha256", p.name);
        assert_eq!(p.repo, "fixture");
    }
}

/// The property that keeps `plan_id` stable: the solution is a function of the
/// request's *content*, not of the order the packages were written in.
#[test]
fn solution_order_is_content_determined() {
    let forward = ["fixture-app".to_string(), "fixture-sysuser".to_string()];
    let backward = ["fixture-sysuser".to_string(), "fixture-app".to_string()];

    let mut a = fixture::session("solve-order-a", ".");
    let mut b = fixture::session("solve-order-b", ".");
    let one = a.solve(&Request::new(forward)).unwrap();
    let two = b.solve(&Request::new(backward)).unwrap();

    assert_eq!(one.packages, two.packages);
}

#[test]
fn a_virtual_name_resolves_through_provides() {
    let mut s = fixture::session("solve-provides", ".");
    // Nothing is named `fixture-editor`; only `fixture-alt` provides it.
    let sol = s
        .solve(&Request::new(["fixture-editor".to_string()]))
        .unwrap();
    assert!(sol.get("fixture-alt").unwrap().explicit);
}

#[test]
fn reports_a_missing_package_by_name() {
    let mut s = fixture::session("solve-missing", ".");
    let err = s
        .solve(&Request::new(["no-such-package".to_string()]))
        .unwrap_err();
    assert_eq!(
        err,
        Error::NotFound {
            name: "no-such-package".into()
        }
    );
}

#[test]
fn reports_an_unsatisfiable_dependency_and_who_wanted_it() {
    let mut s = fixture::session("solve-unsatisfied", ".");
    let err = s
        .solve(&Request::new(["fixture-broken".to_string()]))
        .unwrap_err();
    match err {
        Error::Unsatisfied { wanted_by, dep } => {
            assert_eq!(wanted_by.as_deref(), Some("fixture-broken"));
            assert_eq!(dep, "fixture-nonexistent");
        }
        other => panic!("expected an unsatisfied dependency, got {other:?}"),
    }
}

#[test]
fn reports_a_conflict_between_two_requested_packages() {
    let mut s = fixture::session("solve-conflict", ".");
    let err = s
        .solve(&Request::new([
            "fixture-clash-a".to_string(),
            "fixture-clash-b".to_string(),
        ]))
        .unwrap_err();
    match err {
        Error::Conflict {
            first,
            second,
            reason,
        } => {
            let mut pair = [first, second];
            pair.sort();
            assert_eq!(
                pair,
                ["fixture-clash-a".to_string(), "fixture-clash-b".to_string()]
            );
            assert!(reason.starts_with("fixture-clash-"));
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

/// libalpm distinguishes a conflict from a *replacement*: when one of two
/// conflicting packages also provides the other, it drops the provided one from
/// the target list and resolves silently. That is correct — it is how pacman
/// handles a package superseding another — but it means asking for `A` can get
/// you `B`, and `kiln check` must report what was actually resolved rather than
/// what was asked for. Pinned here so a future change to the solver wrapper
/// cannot quietly turn this into an error, or the error into this.
#[test]
fn a_package_that_provides_what_it_conflicts_with_replaces_it_silently() {
    let mut s = fixture::session("solve-replacement", ".");
    let sol = s
        .solve(&Request::new([
            "fixture-app".to_string(),
            "fixture-alt".to_string(),
        ]))
        .unwrap();
    assert!(sol.get("fixture-alt").is_some());
    assert!(
        sol.get("fixture-app").is_none(),
        "fixture-alt provides and conflicts with fixture-app, so it replaces it"
    );
}

/// `exclude` means "must not appear, even as a dependency". Refusing and
/// naming what pulled it in leaves the decision with the person who wrote the
/// config; dropping the dependency would produce a broken image nobody asked
/// for.
#[test]
fn an_excluded_dependency_is_refused_and_names_what_pulled_it_in() {
    let mut s = fixture::session("solve-excluded", ".");
    let err = s
        .solve(
            &Request::new(["fixture-base".to_string()]).excluding(["fixture-libfoo".to_string()]),
        )
        .unwrap_err();
    assert_eq!(
        err,
        Error::Excluded {
            name: "fixture-libfoo".into(),
            pulled_in_by: vec!["fixture-base".into()],
        }
    );
    assert_eq!(
        err.to_string(),
        "`fixture-libfoo` is excluded but the image would contain it; \
         required by `fixture-base`"
    );
}

#[test]
fn excluding_something_absent_is_not_an_error() {
    let mut s = fixture::session("solve-exclude-absent", ".");
    s.solve(&Request::new(["fixture-libfoo".to_string()]).excluding(["nano".to_string()]))
        .unwrap();
}

/// The data behind `kiln why`: the shortest chain from something the user
/// asked for to the package they are asking about.
#[test]
fn why_gives_the_shortest_chain_from_an_explicit_package() {
    let mut s = fixture::session("solve-why", ".");
    let sol = s.solve(&Request::new(["fixture-app".to_string()])).unwrap();
    assert_eq!(
        sol.chain_to("fixture-libfoo"),
        Some(vec![
            "fixture-app".to_string(),
            "fixture-libfoo".to_string()
        ])
    );
    // A package that is itself explicit is its own explanation.
    assert_eq!(
        sol.chain_to("fixture-app"),
        Some(vec!["fixture-app".to_string()])
    );
}

/// the same configuration against a moved repository must resolve to a
/// different version — the whole point of change detection.
#[test]
fn the_next_repository_resolves_a_newer_version() {
    let mut now = fixture::session("solve-now", ".");
    let mut next = fixture::session("solve-next", "next");
    let want = Request::new(["fixture-libfoo".to_string()]);

    assert_eq!(
        now.solve(&want)
            .unwrap()
            .get("fixture-libfoo")
            .unwrap()
            .version,
        "1.2-1"
    );
    assert_eq!(
        next.solve(&want)
            .unwrap()
            .get("fixture-libfoo")
            .unwrap()
            .version,
        "1.3-1"
    );
}
