//! The AUR RPC, against a **recorded** reply.
//!
//! `fixtures/info-yay-paru.json` is a real response from
//! `aur.archlinux.org/rpc/v5/info`, saved rather than invented — which is the
//! point of a recorded fixture. It asked for three packages, one of which does
//! not exist, because how the endpoint reports that is a thing Kiln has to know
//! and not a thing worth guessing.

use kiln_aur::rpc;
use kiln_aur::transport::parse_ls_remote;

const RECORDED: &str = include_str!("fixtures/info-yay-paru.json");

#[test]
fn a_recorded_reply_parses_into_what_kiln_uses() {
    let infos = rpc::parse(RECORDED).unwrap();
    let yay = &infos["yay"];

    assert_eq!(yay.package_base, "yay");
    assert!(yay.version.contains('-'), "an evr: {}", yay.version);
    assert_eq!(yay.maintainer.as_deref(), Some("jguer"));
    assert!(yay.depends.iter().any(|d| d.starts_with("pacman")));
    assert!(yay.make_depends.iter().any(|d| d.starts_with("go")));
    assert!(yay.last_modified > 0);
}

/// The AUR does not error for a name it does not know — it simply leaves it out
/// of `results`. So "not found" is Kiln's conclusion to draw, with the name it
/// asked for, which is why `parse` returns a map rather than a list.
#[test]
fn a_name_the_aur_does_not_know_is_absent_rather_than_an_error() {
    let infos = rpc::parse(RECORDED).unwrap();
    assert_eq!(infos.len(), 2, "three were asked for");
    assert!(!infos.contains_key("this-package-does-not-exist-kiln"));
}

/// The AUR adds fields. A Kiln that stopped resolving because the RPC learned a
/// new key would be a worse tool than one that reads what it understands — the
/// opposite of the frontend's stance, where an unknown key is *your* typo.
#[test]
fn fields_kiln_does_not_model_are_ignored() {
    // The recorded reply carries ID, NumVotes, Keywords, OptDepends,
    // Popularity, Submitter, FirstSubmitted and Description, none of which
    // appear in `Info`.
    assert!(RECORDED.contains("\"NumVotes\""));
    assert!(RECORDED.contains("\"Popularity\""));
    assert!(rpc::parse(RECORDED).is_ok());
}

/// The trust seam: arbitrary code from a stranger gets one line of daylight
/// before it is built.
#[test]
fn the_trust_summary_names_the_maintainer_and_the_commit() {
    let infos = rpc::parse(RECORDED).unwrap();
    let line = infos["yay"].trust_summary("cb43f84828ab4f9700f7c6f9c6d7a923d4cfaff0", 1);
    assert!(line.starts_with("yay "), "{line}");
    assert!(line.contains("maintainer jguer"), "{line}");
    assert!(line.contains("commit cb43f84"), "{line}");
    assert!(line.contains("1 source"), "{line}");
    assert!(
        !line.contains("1 sources"),
        "one source, not sources: {line}"
    );
}

/// An orphaned package is exactly the case the summary exists for.
#[test]
fn an_orphaned_package_says_so_rather_than_saying_nothing() {
    let orphan = rpc::Info {
        name: "abandoned".into(),
        package_base: "abandoned".into(),
        version: "1.0-1".into(),
        maintainer: None,
        ..rpc::Info::default()
    };
    assert!(orphan
        .trust_summary("abc1234", 3)
        .contains("maintainer ORPHANED"));
}

#[test]
fn a_batched_query_url_carries_every_name() {
    let url = rpc::url(&["yay".into(), "paru-bin".into()]);
    assert!(url.starts_with(rpc::ENDPOINT));
    assert!(url.contains("?arg%5B%5D=yay"));
    assert!(url.contains("&arg%5B%5D=paru-bin"));
}

/// Package names cannot contain anything needing an escape, which is exactly
/// why the escaping is done: "the character set forbids it" is a property of a
/// validator somewhere else that this function must not depend on.
#[test]
fn a_name_from_a_config_file_cannot_forge_a_query() {
    let url = rpc::url(&["evil&arg[]=other".into()]);
    assert_eq!(
        url.matches("arg%5B%5D=").count(),
        1,
        "one argument, however the name was spelled: {url}"
    );
    assert!(!url.contains("&arg[]="), "{url}");
}

#[test]
fn a_reply_that_is_not_a_reply_is_rejected() {
    let err = rpc::parse("<html>502 Bad Gateway</html>").unwrap_err();
    assert!(err.to_string().contains("not an RPC reply"), "{err}");

    let err = rpc::parse(r#"{"type":"error","error":"Too many package results.","results":[]}"#)
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the AUR rejected the query: Too many package results."
    );
}

/// `git ls-remote` output, recorded from the real thing.
#[test]
fn a_recorded_ls_remote_yields_the_object_id() {
    let recorded = include_str!("fixtures/ls-remote-yay.txt");
    assert_eq!(
        parse_ls_remote(recorded).as_deref(),
        Some("cb43f84828ab4f9700f7c6f9c6d7a923d4cfaff0")
    );
}

/// A proxy's error page must not become a commit id in a build record.
#[test]
fn something_that_is_not_an_object_id_is_refused() {
    assert_eq!(parse_ls_remote("<html>Access denied</html>\n"), None);
    assert_eq!(parse_ls_remote(""), None);
    assert_eq!(
        parse_ls_remote("not-hex-not-hex-not-hex-not-hex-not-hex1\tHEAD\n"),
        None
    );
    // A sha256 repository is legitimate.
    let sha256 = "a".repeat(64);
    assert_eq!(
        parse_ls_remote(&format!("{sha256}\tHEAD\n")).as_deref(),
        Some(sha256.as_str())
    );
}
