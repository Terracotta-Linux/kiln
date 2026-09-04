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
