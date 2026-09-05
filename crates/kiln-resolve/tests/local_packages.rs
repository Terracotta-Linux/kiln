//! Local `.pkg.tar.zst` files.
//!
//! > `sha256` is **required**, not optional. An unhashed local blob that
//! > silently changes is precisely the class of drift `kiln check` exists to
//! > catch; making the hash optional makes the guarantee optional.
//!
//! Recording the hash without checking it makes it optional in a way that is
//! harder to notice, so resolution verifies it against the bytes on disk.

mod harness;

use harness::*;
use kiln_resolve::ResolvedInput;

/// The sha256 of the four bytes `pkg\n`, which stands in for a package archive:
/// nothing here unpacks it, and using a real 11 KB fixture would only make the
/// test slower and the intent less obvious.
const BODY: &str = "pkg\n";
const BODY_SHA256: &str = "f238df2ae16f95a3461bb262b8db52df5808bb03a6f2d85471442835bb31c65b";

fn config(sha256: &str) -> String {
    format!(
        r#"{BOOTABLE}
file = [{{ path = "packages/myapp-1.0-1-x86_64.pkg.tar.zst", sha256 = "{sha256}" }}]
"#
    )
}

#[test]
fn a_local_package_with_a_matching_checksum_enters_the_plan() {
    let plan = plan_with(
        "local-ok",
        &config(BODY_SHA256),
        &[("packages/myapp-1.0-1-x86_64.pkg.tar.zst", BODY)],
    );
    let found: Vec<&ResolvedInput> = plan
        .inputs
        .iter()
        .filter(|i| matches!(i, ResolvedInput::FilePackage { .. }))
        .collect();
    assert_eq!(found.len(), 1);
    match found[0] {
        ResolvedInput::FilePackage { path, sha256 } => {
            assert_eq!(path, "packages/myapp-1.0-1-x86_64.pkg.tar.zst");
            assert_eq!(sha256, BODY_SHA256);
        }
        _ => unreachable!(),
    }
}

/// The case the requirement exists for: the blob changed and nobody said so.
#[test]
fn a_local_package_whose_bytes_changed_is_refused() {
    let errs = try_plan_with(
        "local-drift",
        &config(BODY_SHA256),
        &[("packages/myapp-1.0-1-x86_64.pkg.tar.zst", "different\n")],
    )
    .expect_err("a checksum that does not match must fail resolution");

    insta::assert_snapshot!(kiln_diag::render_all(&errs));
}

/// Editing the blob changes `config_id` too — `local_digests` notices
/// the change, and the checksum says whether it was authorized. Two different
/// jobs, and this test is why both exist.
#[test]
fn the_blob_is_part_of_the_configuration_identity_as_well() {
    let one = plan_with(
        "local-identity-a",
        &config(BODY_SHA256),
        &[("packages/myapp-1.0-1-x86_64.pkg.tar.zst", BODY)],
    );
    // The same declaration, different bytes *and* an updated checksum — an
    // authorized change. It must still be a different image.
    let other = "different\n";
    let other_sha = "1170c8939638387ed45a0d39fa66b9cf4302208f2192e7d2ffefb1b9e2a620af";
    let two = plan_with(
        "local-identity-b",
        &config(other_sha),
        &[("packages/myapp-1.0-1-x86_64.pkg.tar.zst", other)],
    );

    assert_ne!(
        one.config_id, two.config_id,
        "the bytes are part of config_id"
    );
    assert_ne!(one.plan_id(), two.plan_id());
}

/// A `packages.file` entry may name an `http(s)://` URL instead of a path.
/// Resolution never fetches it — that happens at realization — so it is
/// trusted through on the strength of the declared `sha256` alone, the same
/// way an AUR closure carries a pinned commit through resolution untouched.
#[test]
fn a_url_package_enters_the_plan_without_being_fetched() {
    let toml = format!(
        r#"{BOOTABLE}
file = [{{ path = "https://example.com/myapp-1.0-1-x86_64.pkg.tar.zst", sha256 = "{BODY_SHA256}" }}]
"#
    );
    let plan = plan("url-file", &toml);
    let found: Vec<&ResolvedInput> = plan
        .inputs
        .iter()
        .filter(|i| matches!(i, ResolvedInput::FilePackage { .. }))
        .collect();
    assert_eq!(found.len(), 1);
    match found[0] {
        ResolvedInput::FilePackage { path, sha256 } => {
            assert_eq!(path, "https://example.com/myapp-1.0-1-x86_64.pkg.tar.zst");
            assert_eq!(sha256, BODY_SHA256);
        }
        _ => unreachable!(),
    }
}

/// `sha256` may itself be a URL to a `.sha256` file — resolution fetches it
/// (it is metadata, the same kind of network call as an AUR RPC lookup) and
/// resolves it to the concrete digest before it ever reaches the plan.
#[test]
fn a_checksum_url_is_fetched_and_resolved_to_the_digest_it_names() {
    let toml = format!(
        r#"{BOOTABLE}
file = [{{ path = "packages/myapp-1.0-1-x86_64.pkg.tar.zst", sha256 = "https://example.com/myapp.pkg.tar.zst.sha256" }}]
"#
    );
    // `sha256sum`'s own format: hex, two spaces, the filename.
    let transport = kiln_aur::Recorded {
        bodies: std::collections::BTreeMap::from([(
            "https://example.com/myapp.pkg.tar.zst.sha256".to_string(),
            format!("{BODY_SHA256}  myapp-1.0-1-x86_64.pkg.tar.zst\n"),
        )]),
        ..kiln_aur::Recorded::new()
    };

    let plan = harness::try_plan_with_transport(
        "checksum-url-ok",
        &toml,
        &[("packages/myapp-1.0-1-x86_64.pkg.tar.zst", BODY)],
        &transport,
    )
    .unwrap_or_else(|e| panic!("resolution failed:\n{}", kiln_diag::render_all(&e)));

    match plan
        .inputs
        .iter()
        .find(|i| matches!(i, ResolvedInput::FilePackage { .. }))
        .unwrap()
    {
        ResolvedInput::FilePackage { sha256, .. } => assert_eq!(sha256, BODY_SHA256),
        _ => unreachable!(),
    }
}

/// The case the checksum-URL feature exists to catch: the file it names does
/// not hold a valid sha256 line.
#[test]
fn a_checksum_url_that_is_not_a_sha256_file_is_refused() {
    let toml = format!(
        r#"{BOOTABLE}
file = [{{ path = "https://example.com/myapp.pkg.tar.zst", sha256 = "https://example.com/not-a-checksum" }}]
"#
    );
    let transport = kiln_aur::Recorded {
        bodies: std::collections::BTreeMap::from([(
            "https://example.com/not-a-checksum".to_string(),
            "<html>404</html>\n".to_string(),
        )]),
        ..kiln_aur::Recorded::new()
    };
    let errs = harness::try_plan_with_transport("checksum-url-bad", &toml, &[], &transport)
        .expect_err("a body with no plausible sha256 must fail resolution");
    insta::assert_snapshot!(kiln_diag::render_all(&errs));
}
