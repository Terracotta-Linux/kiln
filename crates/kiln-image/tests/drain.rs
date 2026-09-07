//! The `/var` drain.
//!
//! If this gets it wrong the symptom is a service that breaks on first boot
//! with a confusing error — so the plan is examined as a value here, in
//! daylight, rather than inferred from a booted VM.

mod scratch;

use kiln_image::drain;
use scratch::*;

/// A staging root with all three cases of the drain plus the things says
/// to drop.
fn tree(name: &str) -> std::path::PathBuf {
    let root = root(name);
    account_files(&root);

    dir(&root, "var/lib/fixture", 0o755);
    file(&root, "var/lib/fixture/seed.db", "seed\n", 0o644);
    dir(&root, "var/lib/fixture/private", 0o700);

    // A relative symlink: the case that needs an `L` line rather than a factory
    // copy.
    link(&root, "var/lock", "../run/lock");
    link(&root, "var/run", "../run");

    // Dropped: the pacman package owns this directory regardless of DBPath.
    dir(&root, "var/lib/pacman/local", 0o755);
    // Dropped: caches and log *content* carry wall-clock data.
    dir(&root, "var/cache/ldconfig", 0o755);
    file(&root, "var/cache/ldconfig/aux-cache", "binary\n", 0o644);
    dir(&root, "var/log/old", 0o755);
    file(
        &root,
        "var/log/pacman.log",
        "[2026-08-31] installed\n",
        0o644,
    );

    root
}

#[test]
fn the_three_cases_get_three_different_verbs() {
    let root = tree("drain-cases");
    let plan = drain::plan(&root).unwrap();

    let line = |path: &str| {
        plan.lines
            .iter()
            .find(|l| l.path() == path)
            .unwrap_or_else(|| panic!("no line for {path}\n{:#?}", plan.lines))
            .render()
    };

    assert_eq!(
        line("/var/lib/fixture"),
        "d /var/lib/fixture 0755 root root -"
    );
    assert_eq!(
        line("/var/lib/fixture/seed.db"),
        "C /var/lib/fixture/seed.db - - - -"
    );
    // `C` *stats* the factory path, so a relative link would resolve inside the
    // factory tree and dangle. tmpfiles has a verb for this and the drain must
    // use it.
    assert_eq!(line("/var/lock"), "L /var/lock - - - - ../run/lock");
}

#[test]
fn a_file_goes_to_the_factory_and_a_symlink_does_not() {
    let root = tree("drain-factory");
    let plan = drain::plan(&root).unwrap();

    let targets: Vec<String> = plan
        .factory
        .iter()
        .map(|c| {
            c.to.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(targets, ["usr/share/factory/var/lib/fixture/seed.db"]);
}

/// the `pacman` package owns `/var/lib/pacman`, so the
/// database relocation does not remove it. A faithful drain would emit a line
/// recreating it on every boot — making the contract true at build time and
/// false at runtime.
#[test]
fn the_paths_that_must_not_come_back_are_dropped_with_a_reason() {
    let root = tree("drain-dropped");
    let plan = drain::plan(&root).unwrap();

    let dropped: Vec<&str> = plan.dropped.iter().map(|(p, _)| p.as_str()).collect();
    assert!(dropped.contains(&"/var/lib/pacman"));
    assert!(dropped.contains(&"/var/cache/ldconfig"));
    assert!(dropped.contains(&"/var/log/pacman.log"));
    assert!(plan.dropped.iter().all(|(_, why)| !why.is_empty()));

    // Nothing under a dropped directory may reappear by another route.
    assert!(
        !plan
            .lines
            .iter()
            .any(|l| l.path().starts_with("/var/lib/pacman")),
        "the pacman database directory must not be recreated at boot"
    );
    assert!(!plan
        .factory
        .iter()
        .any(|c| c.from.to_string_lossy().contains("ldconfig")));
}

/// Log *content* is dropped, but the directories stay: a service that expects
/// `/var/log/its-name` to exist should find it.
#[test]
fn log_directories_survive_but_log_files_do_not() {
    let root = tree("drain-logs");
    let plan = drain::plan(&root).unwrap();
    assert!(plan.lines.iter().any(|l| l.path() == "/var/log/old"));
    assert!(!plan.lines.iter().any(|l| l.path() == "/var/log/pacman.log"));
}

/// Normalization turns `/home`, `/root`, `/opt` and `/srv` into symlinks into `/var`.
/// Without these lines they point at nothing on a machine with an empty `/var`.
#[test]
fn the_targets_of_the_top_level_symlinks_always_exist() {
    let root = tree("drain-always");
    let plan = drain::plan(&root).unwrap();
    for path in [
        "/var/home",
        "/var/roothome",
        "/var/opt",
        "/var/srv",
        "/var/mnt",
    ] {
        assert!(
            plan.lines.iter().any(|l| l.path() == path),
            "{path} must be recreated even though no package shipped it"
        );
    }
    // journald fixes its own ACLs, but the emitted line should
    // carry the right group rather than leave it to be corrected.
    let journal = plan
        .lines
        .iter()
        .find(|l| l.path() == "/var/log/journal")
        .unwrap();
    assert_eq!(
        journal.render(),
        "d /var/log/journal 2755 root systemd-journal -"
    );
}

/// A package that ships one of those directories with its own mode keeps it.
#[test]
fn a_package_that_ships_a_default_directory_wins_over_the_default() {
    let root = root("drain-override");
    account_files(&root);
    dir(&root, "var/srv", 0o750);
    let plan = drain::plan(&root).unwrap();

    let srv: Vec<String> = plan
        .lines
        .iter()
        .filter(|l| l.path() == "/var/srv")
        .map(|l| l.render())
        .collect();
    assert_eq!(
        srv,
        ["d /var/srv 0750 root root -"],
        "exactly one line, the package's"
    );
}

/// Regression test: VSCodium ships
/// `.../syntaxes/Regular Expressions (JavaScript).tmLanguage` into `/opt`,
/// which on an OSTree system is `/var/opt`. The drain refused any path
/// containing whitespace, on the belief that `tmpfiles.d` could not express
/// one — so a single file name stopped a build dead after 697 packages had
/// been fetched and an AUR package built. It can: systemd unquotes and
/// unescapes each field.
#[test]
fn a_path_with_whitespace_is_quoted_rather_than_refused() {
    let root = root("drain-whitespace");
    account_files(&root);
    dir(&root, "var/lib/bad name", 0o755);
    file(
        &root,
        "var/lib/bad name/Regular Expressions (JavaScript).tmLanguage",
        "{}\n",
        0o644,
    );

    let rendered = drain::plan(&root).unwrap().render();
    assert!(
        rendered.contains(r#"d "/var/lib/bad name" 0755 root root -"#),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            r#"C "/var/lib/bad name/Regular Expressions (JavaScript).tmLanguage" - - - -"#
        ),
        "{rendered}"
    );
    // Quoted only where it has to be: an unquoted path keeps the fragment
    // readable, and almost nothing in /var needs the quotes.
    assert!(
        rendered.contains("d /var/home 0755 root root -"),
        "{rendered}"
    );
}

/// The escaping is only correct if *systemd* agrees, so this asks it rather
/// than asserting on a string a human reasoned about: drain a root full of
/// awkward names, hand the fragment to the real `systemd-tmpfiles`, and check
/// every path comes back byte for byte.
///
/// The exit status is deliberately not the assertion. Chowning to `root` needs
/// root, and that failure says nothing about the escaping — while a field
/// systemd cannot parse creates nothing at all, so the names are the evidence
/// either way.
#[test]
fn systemd_tmpfiles_recreates_the_awkward_names_exactly() {
    if std::process::Command::new("systemd-tmpfiles")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: systemd-tmpfiles is not installed");
        return;
    }
    let root = root("drain-tmpfiles-roundtrip");
    account_files(&root);

    // Whitespace, and then the three that quoting alone does not settle: `%`
    // is expanded *after* unquoting, and a backslash or a quote inside quotes
    // has to survive the unescaping pass intact.
    let dirs = [
        "Regular Expressions (JavaScript).tmLanguage",
        "100% done",
        // No whitespace, so it stays unquoted — and still has to be doubled,
        // because the specifier pass does not care about quotes.
        "pct%only",
        "back\\slash",
        "double\"quote",
        "single'quote",
        "tab\there",
        "new\nline",
    ];
    for name in dirs {
        dir(&root, &format!("var/lib/{name}"), 0o755);
    }
    file(&root, "var/lib/a file.db", "seed\n", 0o644);
    link(&root, "var/a link", "../run/some place");

    let plan = drain::plan(&root).unwrap();
    drain::apply(&root, &plan).unwrap();
    let out = std::process::Command::new("systemd-tmpfiles")
        .arg("--create")
        .arg(format!("--root={}", root.display()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Failed to parse") && !stderr.contains("Invalid argument"),
        "systemd could not parse what the drain wrote:\n{stderr}\n\n{}",
        plan.render()
    );

    for name in dirs {
        let at = root.join("var/lib").join(name);
        assert!(at.is_dir(), "{name:?} was not recreated\n{stderr}");
    }
    assert!(root.join("var/lib/a file.db").is_file(), "{stderr}");
    assert_eq!(
        std::fs::read_link(root.join("var/a link")).unwrap(),
        std::path::Path::new("../run/some place"),
        "{stderr}"
    );
}

/// anything large going into the factory means a package is shipping
/// data that belongs in `/usr`. The spike never fired this warning, so it is
/// worth a test rather than a hope.
#[test]
fn an_oversized_factory_file_is_warned_about_by_name() {
    let root = root("drain-fat");
    account_files(&root);
    file(
        &root,
        "var/lib/fat/blob",
        &"x".repeat(drain::FACTORY_WARN_BYTES as usize + 1),
        0o644,
    );
    let plan = drain::plan(&root).unwrap();
    assert_eq!(plan.warnings.len(), 1);
    assert!(
        plan.warnings[0].contains("/var/lib/fat/blob"),
        "{:?}",
        plan.warnings
    );
    assert!(plan.warnings[0].contains("8.0 MiB"), "{:?}", plan.warnings);
}

/// Order is content-determined, never directory-order-determined: two builds of
/// the same tree must render the same file.
#[test]
fn the_rendered_fragment_is_stable() {
    let one = drain::plan(&tree("drain-stable-a")).unwrap();
    let two = drain::plan(&tree("drain-stable-b")).unwrap();
    assert_eq!(one.render(), two.render());
    insta::assert_snapshot!(one.render());
}

/// Applying the plan must leave `/var` empty — that is the contract and
/// the commit filter will reject the tree otherwise.
#[test]
fn applying_the_plan_empties_var_and_writes_the_fragment() {
    let root = tree("drain-apply");
    let plan = drain::plan(&root).unwrap();
    drain::apply(&root, &plan).unwrap();

    assert!(
        std::fs::read_dir(root.join("var"))
            .unwrap()
            .next()
            .is_none(),
        "/var must be empty after the drain"
    );
    let fragment = std::fs::read_to_string(root.join("usr/lib/tmpfiles.d/kiln-var.conf")).unwrap();
    assert!(fragment.contains("d /var/lib/fixture 0755 root root -"));
    assert_eq!(
        std::fs::read_to_string(root.join("usr/share/factory/var/lib/fixture/seed.db")).unwrap(),
        "seed\n"
    );
}
