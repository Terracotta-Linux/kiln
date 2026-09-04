//! Build scripts against a real overlayfs.
//!
//! **Privileged.** `mount -t overlay` needs root, and so does reading the
//! `trusted.overlay.opaque` xattr that says a directory was replaced rather
//! than merged. Run under `sudo -E cargo test -- --ignored`.
//!
//! The *sandbox* is a fake here, and deliberately so. What these tests are
//! about is the mechanism rests on — that the upper layer is the
//! changeset, that a whiteout becomes a deletion, that an opaque directory does
//! not leave its old contents behind — and a fake sandbox that writes directly
//! into the merged mount exercises every bit of that against the real kernel
//! filesystem. Standing a shell up inside a staging root to reach the same
//! overlay would test bubblewrap, which `kiln-sandbox`'s own live tests already
//! do, and the last test in this file does once more through the real spec.

mod fixture;

use kiln_image::overlay::{NoOwners, Owners};
use kiln_image::scripts::{self, Options};
use kiln_manifest::{Script, ScriptPhase};
use kiln_sandbox::{Outcome, Sandbox, SandboxSpec};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A sandbox that runs a closure against the merged mount instead of a command.
///
/// `spec.root` is the overlay's merged directory, so everything the closure
/// does is a genuine overlayfs write: copy-up, whiteouts and opaque markers are
/// all produced by the kernel exactly as a real script would produce them.
struct Doing<F: Fn(&Path)>(F);

impl<F: Fn(&Path)> Sandbox for Doing<F> {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn argv(&self, spec: &SandboxSpec) -> kiln_sandbox::Result<Vec<String>> {
        Ok(spec.command.clone())
    }
    fn run(&self, spec: &SandboxSpec) -> kiln_sandbox::Result<Outcome> {
        (self.0)(&spec.root);
        Ok(Outcome::default())
    }
}

/// A sandbox that fails, for the failure path.
struct Failing;

impl Sandbox for Failing {
    fn name(&self) -> &'static str {
        "failing"
    }
    fn argv(&self, spec: &SandboxSpec) -> kiln_sandbox::Result<Vec<String>> {
        Ok(spec.command.clone())
    }
    fn run(&self, spec: &SandboxSpec) -> kiln_sandbox::Result<Outcome> {
        Err(kiln_sandbox::Error::Failed {
            command: spec.command.join(" "),
            status: 3,
            stderr: "locale-gen: no such locale\n".into(),
        })
    }
}

/// A staging root shaped like a half-assembled image, with a package's file in
/// it to overwrite and a directory to replace.
fn staging(name: &str) -> (PathBuf, PathBuf) {
    let base = fixture::workspace().join("target/test-roots").join(name);
    std::fs::remove_dir_all(&base).ok();
    let root = base.join("root");
    for dir in ["usr/bin", "usr/share/fonts", "etc", "boot", "var/lib"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("etc/locale.gen"), "# commented out\n").unwrap();
    std::fs::write(root.join("usr/share/fonts/stale.ttf"), "old\n").unwrap();
    std::fs::write(root.join("usr/bin/keep"), "kept\n").unwrap();
    (root, base.join("work"))
}

fn one(name: &str, body: &str, phase: ScriptPhase) -> BTreeMap<String, Script> {
    BTreeMap::from([(
        name.to_string(),
        Script {
            name: name.to_string(),
            source: None,
            content: Some(body.to_string()),
            after: phase,
        },
    )])
}

fn options<'a>(root: &'a Path, work: &'a Path) -> Options<'a> {
    Options {
        root,
        work,
        config_root: root,
        image: "fixture",
        generation: 2,
        arch: "x86_64",
    }
}

fn run<F: Fn(&Path)>(
    scripts: &BTreeMap<String, Script>,
    root: &Path,
    work: &Path,
    owners: &dyn Owners,
    doing: F,
) -> kiln_image::tree::Result<scripts::Applied> {
    scripts::run(
        ScriptPhase::Files,
        scripts,
        &options(root, work),
        owners,
        &Doing(doing),
    )
}

/// The base case, and the one sentence is built on: the upper layer *is*
/// the changeset. Nothing here diffs a tree.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn what_a_script_wrote_is_in_the_changeset_and_in_the_staging_root() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-writes");
    let scripts = one("20-locale", "locale-gen\n", ScriptPhase::Files);

    let applied = run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::create_dir_all(merged.join("usr/lib/locale")).unwrap();
        std::fs::write(merged.join("usr/lib/locale/locale-archive"), "archive\n").unwrap();
        std::fs::write(merged.join("etc/locale.gen"), "en_US.UTF-8 UTF-8\n").unwrap();
    })
    .expect("the script succeeded");

    let ran = &applied.ran[0];
    let wrote: Vec<&str> = ran
        .changeset
        .wrote
        .iter()
        .map(|w| w.path.as_str())
        .collect();
    assert_eq!(wrote, ["/etc/locale.gen", "/usr/lib/locale/locale-archive"]);
    assert!(ran.changeset.deleted.is_empty());

    // And the changeset landed. The script wrote to the overlay; the staging
    // root gets it afterwards.
    assert_eq!(
        std::fs::read_to_string(root.join("etc/locale.gen")).unwrap(),
        "en_US.UTF-8 UTF-8\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("usr/lib/locale/locale-archive")).unwrap(),
        "archive\n"
    );
    // Untouched files are still untouched — an overlay merge is not a sync.
    assert_eq!(
        std::fs::read_to_string(root.join("usr/bin/keep")).unwrap(),
        "kept\n"
    );
}

/// A script writes to the overlay, never to the staging root. That is what
/// makes a failed script leave nothing behind, and it is asserted from inside
/// the script's own run rather than inferred afterwards.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn the_staging_root_is_untouched_while_the_script_is_running() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-isolation");
    let scripts = one("20-x", "true\n", ScriptPhase::Files);
    let seen = std::sync::Mutex::new(None);

    run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::write(merged.join("etc/new"), "written\n").unwrap();
        *seen.lock().unwrap() = Some(root.join("etc/new").exists());
    })
    .expect("the script succeeded");

    assert_eq!(
        *seen.lock().unwrap(),
        Some(false),
        "the staging root must not see the write until the changeset is merged down"
    );
    assert!(root.join("etc/new").exists(), "and must see it afterwards");
}

/// A failed script fails the build and leaves the staging root as it found it.
/// *non-zero exit fails the build, with the script's output inline*.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn a_failing_script_fails_the_build_and_changes_nothing() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-failure");
    let scripts = one("20-locale", "locale-gen\n", ScriptPhase::Files);

    let err = scripts::run(
        ScriptPhase::Files,
        &scripts,
        &options(&root, &work),
        &NoOwners,
        &Failing,
    )
    .expect_err("a script that exits non-zero must fail the build");

    let text = err.to_string();
    assert!(text.contains("20-locale"), "got: {text}");
    assert!(
        text.contains("no such locale"),
        "the reason survives: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("etc/locale.gen")).unwrap(),
        "# commented out\n",
        "a failed script must not have touched the staging root"
    );
}

/// overlayfs records a deletion as a character device with device number 0.
/// Reading it wrong leaves the file in the image; not reading it at all leaves
/// a character device in the image, at the path the script deleted.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn a_deleted_file_is_gone_from_the_staging_root() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-delete");
    let scripts = one("20-prune", "rm /usr/bin/keep\n", ScriptPhase::Files);

    let applied = run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::remove_file(merged.join("usr/bin/keep")).unwrap();
    })
    .expect("the script succeeded");

    assert_eq!(applied.ran[0].changeset.deleted, ["/usr/bin/keep"]);
    assert!(
        !root.join("usr/bin/keep").exists(),
        "the deletion must reach the staging root"
    );
    // And no whiteout node was copied in its place, which is the failure that
    // looks like success until something tries to read the path.
    assert!(root.join("usr/bin").exists());
}

/// A directory removed and recreated is marked `trusted.overlay.opaque` rather
/// than whited out. Treating it as an ordinary directory merges the new
/// contents over the old ones and ships both — files that the script deleted,
/// present in the image, with nothing anywhere saying why.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn a_directory_removed_and_recreated_does_not_keep_its_old_contents() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-opaque");
    let scripts = one(
        "20-fonts",
        "rm -rf /usr/share/fonts && mkdir /usr/share/fonts\n",
        ScriptPhase::Files,
    );

    let applied = run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::remove_dir_all(merged.join("usr/share/fonts")).unwrap();
        std::fs::create_dir(merged.join("usr/share/fonts")).unwrap();
        std::fs::write(merged.join("usr/share/fonts/new.ttf"), "new\n").unwrap();
    })
    .expect("the script succeeded");

    assert!(
        applied.ran[0]
            .changeset
            .deleted
            .contains(&"/usr/share/fonts".to_string()),
        "the replacement must be reported as a deletion: {:?}",
        applied.ran[0].changeset
    );
    assert!(
        !root.join("usr/share/fonts/stale.ttf").exists(),
        "the old contents must be gone"
    );
    assert!(root.join("usr/share/fonts/new.ttf").exists());
}

/// builds run as root so that ownership, setuid bits and capabilities
/// land in the commit exactly as declared. A script's changeset is no
/// exception, and a hand-rolled copy is exactly where those get dropped.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn modes_and_ownership_survive_the_merge() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (root, work) = staging("scripts-modes");
    let scripts = one("20-install", "install -m4755 …\n", ScriptPhase::Files);

    run(&scripts, &root, &work, &NoOwners, |merged| {
        let at = merged.join("usr/bin/privileged");
        std::fs::write(&at, "binary\n").unwrap();
        // chown before chmod: the kernel clears setuid and setgid on a
        // successful chown, so the other order silently produces an 0755 file
        // and this test would pass while proving nothing.
        std::os::unix::fs::chown(&at, Some(0), Some(972)).unwrap();
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o4755)).unwrap();
        std::os::unix::fs::symlink("privileged", merged.join("usr/bin/alias")).unwrap();
    })
    .expect("the script succeeded");

    let md = std::fs::metadata(root.join("usr/bin/privileged")).unwrap();
    assert_eq!(md.permissions().mode() & 0o7777, 0o4755, "setuid survived");
    assert_eq!(md.gid(), 972, "the group survived");

    let link = std::fs::symlink_metadata(root.join("usr/bin/alias")).unwrap();
    assert!(
        link.file_type().is_symlink(),
        "a symlink must arrive as a symlink, not as a copy of its target"
    );
}

/// the determinism audit. Two runs of the same script text over
/// the same tree must hash the same, or `kiln rebuild` cannot tell a
/// non-reproducible script from a changed one.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn the_same_script_over_the_same_tree_produces_the_same_digest() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let write = |merged: &Path| {
        std::fs::write(merged.join("etc/locale.gen"), "en_US.UTF-8 UTF-8\n").unwrap();
        std::fs::remove_file(merged.join("usr/bin/keep")).unwrap();
    };
    let scripts = one("20-locale", "locale-gen\n", ScriptPhase::Files);

    let digest = |name: &str| {
        let (root, work) = staging(name);
        let applied = run(&scripts, &root, &work, &NoOwners, write).expect("the script succeeded");
        applied.ran[0].digest.clone()
    };

    assert_eq!(
        digest("scripts-determinism-a"),
        digest("scripts-determinism-b")
    );

    // …and a script that did something else must not hash the same, or the
    // audit would pass on every script ever written.
    let (root, work) = staging("scripts-determinism-c");
    let other = run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::write(merged.join("etc/locale.gen"), "de_DE.UTF-8 UTF-8\n").unwrap();
    })
    .expect("the script succeeded");
    assert_ne!(other.ran[0].digest, digest("scripts-determinism-d"));
}

/// *a script that writes nothing raises a warning* — because it almost
/// certainly did not do what its author thought, and because normalization
/// already runs `ldconfig`, `depmod` and `fc-cache` without being asked.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn a_script_that_wrote_nothing_is_a_warning_rather_than_a_failure() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-empty");
    let scripts = one("20-ldconfig", "ldconfig\n", ScriptPhase::Files);

    let applied = run(&scripts, &root, &work, &NoOwners, |_| {}).expect("this is not a failure");

    assert!(applied.ran[0].changeset.is_empty());
    let note = applied.notes.join("\n");
    assert!(note.contains("20-ldconfig"), "got: {note}");
    assert!(
        note.contains("ldconfig"),
        "the note should name the alternative: {note}"
    );
}

struct Glibc;

impl Owners for Glibc {
    fn owner_of(&self, path: &str) -> Option<String> {
        (path == "/etc/locale.gen").then(|| "glibc".to_string())
    }
}

/// Reported with the owning package — and *not* refused, because
/// rewriting glibc's `locale.gen` is the example the section opens with.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn overwriting_a_package_file_is_reported_with_its_package() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-clobber");
    let scripts = one("20-locale", "locale-gen\n", ScriptPhase::Files);

    let applied = run(&scripts, &root, &work, &Glibc, |merged| {
        std::fs::write(merged.join("etc/locale.gen"), "en_US.UTF-8 UTF-8\n").unwrap();
    })
    .expect("a script may overwrite a package's file; a [[file]] may not");

    assert_eq!(applied.ran[0].clobbered.len(), 1);
    assert_eq!(applied.ran[0].clobbered[0].package, "glibc");
    let note = applied.notes.join("\n");
    assert!(note.contains("glibc"), "got: {note}");
    assert!(note.contains("/etc/locale.gen"), "got: {note}");
}

/// *writes to `/var` are fine — the drain runs afterwards. Writes to
/// `/boot` are rejected, as everywhere else.*
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn writing_under_boot_is_refused_and_writing_under_var_is_not() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-boot");
    let scripts = one("20-boot", "cp … /boot\n", ScriptPhase::Files);

    let err = run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::write(merged.join("boot/vmlinuz-linux"), "not yours\n").unwrap();
    })
    .expect_err("OSTree owns /boot");
    let text = err.to_string();
    assert!(text.contains("/boot/vmlinuz-linux"), "got: {text}");

    let (root, work) = staging("scripts-var");
    run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::create_dir_all(merged.join("var/lib/thing")).unwrap();
        std::fs::write(merged.join("var/lib/thing/seed"), "fine\n").unwrap();
    })
    .expect("/var is drained afterwards, not refused");
    assert!(root.join("var/lib/thing/seed").exists());
}

/// Both slots, and only the one asked for. Assembly steps 5 and 8 exist because a
/// script that needs `[[file]]` content in place and one that must run before
/// it are different jobs.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn each_phase_runs_only_its_own_scripts() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-phases");
    let mut scripts = one("10-early", "true\n", ScriptPhase::Packages);
    scripts.extend(one("20-late", "true\n", ScriptPhase::Files));

    let opts = options(&root, &work);
    let early = scripts::run(
        ScriptPhase::Packages,
        &scripts,
        &opts,
        &NoOwners,
        &Doing(|merged: &Path| {
            std::fs::write(merged.join("etc/early"), "1\n").unwrap();
        }),
    )
    .unwrap();
    let late = scripts::run(
        ScriptPhase::Files,
        &scripts,
        &opts,
        &NoOwners,
        &Doing(|merged: &Path| {
            std::fs::write(merged.join("etc/late"), "1\n").unwrap();
        }),
    )
    .unwrap();

    assert_eq!(early.ran.len(), 1);
    assert_eq!(early.ran[0].name, "10-early");
    assert_eq!(late.ran.len(), 1);
    assert_eq!(late.ran[0].name, "20-late");
    assert!(root.join("etc/early").exists() && root.join("etc/late").exists());
}

/// The record's `script_effects` is what `kiln rebuild` reads, so the
/// mapping from names to changeset digests has to be complete.
#[test]
#[ignore = "privileged: mounting an overlayfs needs root"]
fn every_script_that_ran_is_in_the_effects_the_record_stores() {
    if !fixture::require_root("mounting an overlay") {
        return;
    }
    let (root, work) = staging("scripts-effects");
    let mut scripts = one("10-a", "true\n", ScriptPhase::Files);
    scripts.extend(one("20-b", "true\n", ScriptPhase::Files));

    let applied = run(&scripts, &root, &work, &NoOwners, |merged| {
        std::fs::write(merged.join("etc/x"), "1\n").unwrap();
    })
    .unwrap();

    let effects = applied.effects();
    assert_eq!(
        effects.keys().collect::<Vec<_>>(),
        ["10-a", "20-b"],
        "both scripts, by name"
    );
    assert!(effects.values().all(|d| d.starts_with("b3:")));
}

/// The one test here that uses a real sandbox, and the only one that can prove
/// the *spec* works: the shebang is honoured, the script's text arrives, the
/// environment is what says, and the network is gone.
///
/// `/usr` is bound in because a bare staging root has no shell — that is the
/// only difference from a real run, and it is the same borrowing
/// `kiln-sandbox`'s live tests do.
#[test]
fn a_real_script_runs_with_the_environment_and_isolation_the_spec_describes() {
    if !bwrap_usable() {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    }
    let base = fixture::workspace()
        .join("target/test-roots")
        .join("scripts-live");
    std::fs::remove_dir_all(&base).ok();
    let root = base.join("root");
    std::fs::create_dir_all(root.join("usr")).unwrap();
    std::fs::create_dir_all(base.join("sandbox")).unwrap();
    for (link, target) in [("bin", "usr/bin"), ("lib", "usr/lib"), ("lib64", "usr/lib")] {
        std::os::unix::fs::symlink(target, root.join(link)).unwrap();
    }

    let text = "#!/bin/sh\n\
                echo \"$KILN_IMAGE $KILN_GENERATION $LANG $SOURCE_DATE_EPOCH\"\n\
                ip -o link show 2>/dev/null | grep -cv ' lo:' || true\n";
    let on_host = base.join("script");
    std::fs::write(&on_host, text).unwrap();
    // Executable, as `scripts::one` makes it: a shebang script is exec'd, not
    // handed to an interpreter, so the bit has to be on the host file.
    std::fs::set_permissions(
        &on_host,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let spec = scripts::spec(&root, &on_host, text, "workstation", 7)
        .with_bind(kiln_sandbox::Bind::ro("/usr", "/usr"));
    // The shebang decides the command; nothing wraps it in an interpreter.
    assert_eq!(spec.command, vec![scripts::SCRIPT_IN_SANDBOX]);

    let outcome = kiln_sandbox::Bubblewrap::new(base.join("sandbox"))
        .run(&spec)
        .expect("the script runs");
    let mut lines = outcome.stdout.lines();
    assert_eq!(lines.next(), Some("workstation 7 C.UTF-8 0"));
    // No interface but loopback: the sandbox's constraint, checked by trying rather
    // than by reading the argv back.
    assert_eq!(
        lines.next().unwrap_or("0").trim(),
        "0",
        "a build script must have no network: {}",
        outcome.stdout
    );
}

fn bwrap_usable() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
