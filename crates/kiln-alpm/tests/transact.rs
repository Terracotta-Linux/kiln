//! The transaction: packages actually landing in a staging root.
//!
//! **Privileged.** Extracting a package means creating files owned by root with
//! the modes the archive declares, which an ordinary user cannot do — and
//! faking it would test something other than what a build does. These are
//! ignored by default and run under `sudo -E cargo test -- --ignored`, or in a
//! privileged CI container.

mod fixture;

use kiln_alpm::Transaction;
use std::path::PathBuf;

fn require_root(what: &str) -> bool {
    let root = effective_uid() == 0;
    if !root {
        eprintln!("skipped: {what} needs root");
    }
    root
}

fn effective_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(2)?
                .parse()
                .ok()
        })
        .unwrap_or(u32::MAX)
}

#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_transaction_installs_packages_and_records_them() {
    if !require_root("installing packages") {
        return;
    }
    let mut s = fixture::staging("transact-install");
    let t = Transaction::new(["fixture-base".to_string()]).explicitly(["fixture-base".to_string()]);

    s.fetch(&t).expect("fetching from the fixture repository");
    let report = s.install(&t).expect("installing into the staging root");

    assert_eq!(
        report.installed,
        ["fixture-base", "fixture-filesystem", "fixture-libfoo"]
    );
    // the database is part of the image and lives in /usr, so a booted
    // system can answer `pacman -Q` offline.
    let root = s.config().root.clone();
    assert!(root.join("usr/lib/sysimage/pacman/local").is_dir());
    assert!(!root.join("var/lib/pacman/local").exists());
    assert!(root.join("usr/lib/libfoo.so").exists());
}

/// *"a scriptlet that fails aborts the build with the package name and the
/// last 40 lines."*
///
/// libalpm does not do that. It logs `command failed to execute correctly` at
/// ERROR level and reports the commit as **successful** — correct for
/// `pacman -Syu`, where a broken scriptlet should not brick a running system,
/// and wrong for an image build. Kiln has to notice, and this is the test that
/// says so.
///
/// The failure here is real rather than contrived: `fixture-filesystem` ships
/// no shell, so `fixture-app`'s `.INSTALL` cannot run in the chroot. That is
/// also the shape of the mistake this catches in production — a scriptlet
/// reaching for something the image does not contain.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_failing_scriptlet_aborts_rather_than_passing_quietly() {
    if !require_root("running scriptlets") {
        return;
    }
    let mut s = fixture::staging("transact-scriptlet");
    let t = Transaction::new(["fixture-app".to_string()]);
    s.fetch(&t).unwrap();

    let err = s
        .install(&t)
        .expect_err("a scriptlet that could not run must fail the build");
    match &err {
        kiln_alpm::Error::TransactionErrors { during, messages } => {
            assert_eq!(during.as_deref(), Some("the package `fixture-app`"));
            assert!(!messages.is_empty(), "the reason must survive");
        }
        other => panic!("expected a transaction error, got {other:?}"),
    }
    // The message has to name the package, or "a scriptlet failed somewhere" is
    // all the user gets.
    assert!(err.to_string().contains("fixture-app"), "got: {err}");
}

/// The other half: output is attributed to the package that produced it, so a
/// build log can be read package by package rather than as one wall of text.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn scriptlet_output_is_attributed_to_the_package_that_produced_it() {
    if !require_root("running scriptlets") {
        return;
    }
    let mut s = fixture::staging("transact-attribution");
    let t = Transaction::new(["fixture-app".to_string()]);
    s.fetch(&t).unwrap();
    let err = s.install(&t).unwrap_err();
    let kiln_alpm::Error::TransactionErrors { .. } = &err else {
        panic!("expected a transaction error, got {err:?}")
    };

    // Re-run against a root that already has everything, to read the report
    // rather than the error. The attribution is what is under test.
    let mut s = fixture::staging("transact-attribution-2");
    let t = Transaction::new(["fixture-libfoo".to_string()]);
    s.fetch(&t).unwrap();
    let report = s.install(&t).unwrap();
    assert_eq!(
        report
            .scriptlets
            .iter()
            .map(|o| o.package.as_str())
            .collect::<Vec<_>>(),
        ["fixture-libfoo"],
        "every installed package gets its own bucket, empty or not"
    );
}

/// builds run as root, always. As an ordinary user libalpm extracts the
/// archive, fails every chown, logs a *warning*, and reports success — leaving
/// a tree with the wrong ownership and no error anywhere. Kiln refuses instead.
/// This one is deliberately **not** ignored: it is the case that must hold for
/// the user who runs `kiln build` without sudo.
#[test]
fn a_transaction_refuses_to_run_without_root() {
    if effective_uid() == 0 {
        eprintln!("skipped: this asserts the unprivileged path");
        return;
    }
    let mut s = fixture::staging("transact-unprivileged");
    let t = Transaction::new(["fixture-libfoo".to_string()]);
    let err = s.install(&t).unwrap_err();
    assert!(matches!(err, kiln_alpm::Error::NotRoot), "got {err:?}");
    assert!(err.to_string().contains("ownership"), "got: {err}");
}

/// package-shipped hooks always run and cannot be disabled —
/// libalpm scans `/usr/share/libalpm/hooks` unconditionally and `--hookdir`
/// does not suppress it. Asserted by the hook's *effect*, not by the fact that
/// libalpm mentioned it.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_package_shipped_hook_always_runs() {
    if !require_root("running alpm hooks") {
        return;
    }
    let mut s = fixture::staging("transact-hooks");
    let t = Transaction::new(["fixture-hook".to_string(), "fixture-libfoo".to_string()]);
    s.fetch(&t).unwrap();
    let report = s.install(&t).unwrap();

    assert!(
        report.hooks.iter().any(|h| h.contains("99-fixture")),
        "the package's own hook must have run: {:?}",
        report.hooks
    );
    let marker = s.config().root.join("fixture-hook-ran");
    assert!(
        marker.is_file(),
        "the hook must actually have done its work, not merely been listed"
    );
    assert!(std::fs::read_to_string(&marker)
        .unwrap()
        .contains("the-package-hook"));
}

/// other half, and the mechanism `kiln-image` depends on: the only
/// lever over a package-shipped hook is **same-filename shadowing** from a
/// later `HookDir`. It shadows `21-systemd-tmpfiles`, `90-dracut-install`,
/// `60-dracut-remove` and `60-depmod` this way, so if this stops working the
/// image quietly regains a pile of machine state and a wasted dracut run.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_hook_can_be_shadowed_by_filename_from_a_later_hookdir() {
    if !require_root("running alpm hooks") {
        return;
    }
    let shadows = fixture::scratch("transact-shadow-hooks").join("hooks");
    std::fs::create_dir_all(&shadows).unwrap();
    // A hook with the same *filename* and nothing in it. libalpm reads the
    // later directory's copy instead of the package's.
    std::fs::write(shadows.join("99-fixture.hook"), "# Shadowed by Kiln.\n").unwrap();

    let mut s = fixture::staging_with_hookdir("transact-shadow", &shadows);
    let t = Transaction::new(["fixture-hook".to_string(), "fixture-libfoo".to_string()]);
    s.fetch(&t).unwrap();
    s.install(&t).unwrap();

    assert!(
        !s.config().root.join("fixture-hook-ran").exists(),
        "the shadowed hook must not have run"
    );
}

/// The route everything realization builds takes into the image.
///
/// An AUR package, a `packages.build` recipe's output and an out-of-tree
/// module are all `.pkg.tar.zst` files that no repository has ever heard of.
/// They still go through pacman — Kiln calls that a firm rule, not an
/// optimization — so libalpm has to load them from disk, and the file list has
/// to come with them or conflict detection has nothing to check.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_package_file_installs_from_disk_and_its_dependencies_resolve_from_the_repositories() {
    if !require_root("installing a package file") {
        return;
    }
    let mut s = fixture::staging("transact-local");
    let file = package_file("fixture-alt");

    // Nothing names `fixture-alt`. The only thing the transaction is told is
    // where the file is; `fixture-libfoo` is its dependency and has to be found
    // by libalpm, from the sync database, and downloaded.
    let t = Transaction::new(Vec::new()).with_locals([file.clone()]);
    assert!(!t.is_empty(), "a transaction with only locals is not empty");

    s.fetch(&t).expect("fetching the dependency closure");
    let report = s.install(&t).expect("installing the package file");

    assert!(
        report.installed.contains(&"fixture-alt".to_string()),
        "the file itself: {:?}",
        report.installed
    );
    assert!(
        report.installed.contains(&"fixture-libfoo".to_string()),
        "its dependency, resolved from the repository: {:?}",
        report.installed
    );
    let root = s.config().root.clone();
    assert!(root.join("usr/bin/fixture-app").exists());
    // it lands in the image's database like anything else, so a booted
    // image can say who owns the file.
    assert_eq!(
        s.owns("/usr/bin/fixture-app").as_deref(),
        Some("fixture-alt")
    );
}

/// A path is not a package name. Putting one in `packages` produces libalpm's
/// "no package named `…`", which is a true statement about the wrong question —
/// which is why locals are a separate list.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_package_file_named_as_a_package_is_refused_rather_than_silently_skipped() {
    if !require_root("opening a transaction") {
        return;
    }
    let mut s = fixture::staging("transact-local-by-name");
    let file = package_file("fixture-alt");
    let t = Transaction::new([file.to_string_lossy().into_owned()]);

    let err = s.install(&t).expect_err("a path resolves to no package");
    assert!(
        matches!(err, kiln_alpm::Error::NotFound { .. }),
        "got {err:?}"
    );
}

/// An unreadable file is named, with its path, rather than surfacing as
/// whatever libalpm says about a null pointer.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_file_that_is_not_a_package_names_itself() {
    if !require_root("opening a transaction") {
        return;
    }
    let mut s = fixture::staging("transact-local-garbage");
    let junk = s
        .config()
        .root
        .parent()
        .unwrap()
        .join("not-a-package.pkg.tar.zst");
    std::fs::write(&junk, b"this is not an archive\n").unwrap();

    let err = s
        .install(&Transaction::new(Vec::new()).with_locals([junk.clone()]))
        .expect_err("garbage is not a package");
    assert!(
        err.to_string().contains("not-a-package.pkg.tar.zst"),
        "the message must name the file: {err}"
    );
}

/// The one `.pkg.tar.zst` in the fixture repository for `name`, as a path.
/// Standing in for something Kiln built: a real package archive that no
/// registered database knows about, since the test hands over the file and
/// never the name.
fn package_file(name: &str) -> PathBuf {
    let repo = fixture::repo();
    std::fs::read_dir(&repo)
        .expect("the fixture repository")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{name}-")) && n.ends_with(".pkg.tar.zst"))
        })
        .unwrap_or_else(|| panic!("no {name} package in {}", repo.display()))
}
