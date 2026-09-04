//! Rendered resolution diagnostics.
//!
//! > When you change a diagnostic, read the snapshot diff as a user would —
//! > that is the review, not a formality.
//!
//! These are snapshots of what a person actually sees, not of a `Debug` value.
//! A diagnostic that nobody tests rots into `Error: InvalidConfig`.

mod harness;

use harness::*;

fn render(name: &str, toml: &str) -> String {
    let errs = try_plan(name, toml).expect_err("this configuration must not resolve");
    kiln_diag::render_all(&errs)
}

#[test]
fn a_misspelled_package_points_at_the_word_and_suggests() {
    insta::assert_snapshot!(render(
        "diag-typo",
        &BOOTABLE.replace(r#""fixture-init"]"#, r#""fixture-init", "fixture-libfo"]"#,),
    ));
}

#[test]
fn an_unsatisfiable_dependency_names_the_package_that_wants_it() {
    insta::assert_snapshot!(render(
        "diag-unsatisfied",
        &BOOTABLE.replace(r#""fixture-init"]"#, r#""fixture-init", "fixture-broken"]"#,),
    ));
}

#[test]
fn a_conflict_points_at_both_packages() {
    insta::assert_snapshot!(render(
        "diag-conflict",
        &BOOTABLE.replace(
            r#""fixture-init"]"#,
            r#""fixture-init", "fixture-clash-a", "fixture-clash-b"]"#,
        ),
    ));
}

/// The diagnostic Kiln most needs to get right: it has to say what is excluded,
/// what requires it, and why Kiln will not simply drop the dependency.
#[test]
fn an_excluded_dependency_shows_the_exclusion_and_what_needs_it() {
    insta::assert_snapshot!(render(
        "diag-excluded",
        &format!(
            "{}\nexclude = [\"fixture-libfoo\"]\n",
            BOOTABLE.replace(r#""fixture-init"]"#, r#""fixture-init", "fixture-app"]"#,)
        ),
    ));
}

/// Kiln promises this exact failure rather than an artifact that silently does
/// not boot.
#[test]
fn an_image_with_no_kernel_says_so_and_points_at_minimal() {
    insta::assert_snapshot!(render(
        "diag-no-kernel",
        r#"
kiln = 1

[image]
name = "fixture"
arch = "x86_64"

[kernel]
package = "fixture-linux"

[packages]
repo = ["fixture-init"]
"#,
    ));
}

#[test]
fn an_image_with_no_init_says_so() {
    insta::assert_snapshot!(render(
        "diag-no-init",
        r#"
kiln = 1

[image]
name = "fixture"
arch = "x86_64"

[kernel]
package = "fixture-linux"

[packages]
repo = ["fixture-linux"]
"#,
    ));
}
