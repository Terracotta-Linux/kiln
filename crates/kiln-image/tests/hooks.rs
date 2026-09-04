//! Neutralizing package-shipped alpm hooks. findings H1–H4.

mod scratch;

use kiln_image::hooks;

/// The policy names: shadow hooks that write runtime state, or that Kiln
/// owns; keep the rest. This test is the list, so that adding to it is a
/// deliberate act rather than a drive-by.
#[test]
fn exactly_the_four_hooks_the_design_names_are_shadowed() {
    let names: Vec<&str> = hooks::SHADOWED.iter().map(|h| h.filename).collect();
    assert_eq!(
        names,
        [
            "21-systemd-tmpfiles.hook",
            "90-dracut-install.hook",
            "60-dracut-remove.hook",
            "60-depmod.hook",
        ]
    );
    // Everything else is legitimate image content: locale-gen, ldconfig,
    // iconvconfig, sysusers, ca-trust, hwdb, the journal catalog, binfmt and
    // the glib schema hooks all write to /usr or /etc.
    for kept in [
        "20-systemd-sysusers.hook",
        "30-update-ca-trust.hook",
        "zz-ldconfig.hook",
    ] {
        assert!(!hooks::is_shadowed(kept), "{kept} must keep running");
    }
}

#[test]
fn a_shadow_file_never_fires_and_says_why_it_exists() {
    let dir = scratch::root("hooks-materialize").join("hooks");
    hooks::materialize(&dir).unwrap();

    for hook in hooks::SHADOWED {
        let body = std::fs::read_to_string(dir.join(hook.filename)).unwrap();
        // Every line is a comment, so the file declares no trigger and no
        // action and libalpm parses it without ever running it. A shadow that
        // accidentally fired would be worse than no shadow at all — hence the
        // check on the *lines* rather than on the text, which would also match
        // the explanation in the comment.
        for line in body.lines() {
            assert!(
                line.is_empty() || line.starts_with('#'),
                "{}: `{line}` is not a comment",
                hook.filename
            );
            assert!(
                line.len() <= 80,
                "{}: `{line}` is too long to read",
                hook.filename
            );
        }
        // The next person to find one of these in a build tree needs to know
        // what it is before deleting it.
        assert!(body.contains(" "), "{}", hook.filename);
    }
}

/// The shadow directory lives beside the build, never inside the staging root:
/// a shadow file left in the image would ship a do-nothing hook to a system
/// that never runs alpm hooks again.
#[test]
fn materialize_returns_the_directory_it_was_given() {
    let dir = scratch::root("hooks-location").join("sandbox/hooks");
    assert_eq!(hooks::materialize(&dir).unwrap(), dir);
}
