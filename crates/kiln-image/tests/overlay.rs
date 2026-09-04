//! `[[file]]` entries. step 6.
//!
//! The routing table is most of this module and all of the user pain, so most
//! of the test is the table: every wrong answer is a file that is silently not
//! in the image, or is in it and does nothing.

mod scratch;

use kiln_image::overlay::{self, NoOwners, Owners, Route};
use kiln_manifest::FileEntry;
use std::collections::BTreeMap;

fn entry(target: &str, content: &str, mode: Option<u32>) -> (String, FileEntry) {
    (
        target.to_string(),
        FileEntry {
            target: target.to_string(),
            source: None,
            content: Some(content.to_string()),
            mode,
        },
    )
}

fn from_source(target: &str, source: &str) -> (String, FileEntry) {
    (
        target.to_string(),
        FileEntry {
            target: target.to_string(),
            source: Some(source.to_string()),
            content: None,
            mode: None,
        },
    )
}

#[test]
fn the_whole_routing_table() {
    let targets = [
        "/usr/bin/mytool",
        "/usr/lib/sysctl.d/99-mine.conf",
        "/etc/motd",
        "/etc/sudoers.d/10-wheel",
        "/usr/etc/sudoers.d/10-wheel",
        "/var/lib/myapp/seed.db",
        "/opt/thing/config",
        "/srv/http/index.html",
        "/boot/grub/custom.cfg",
        "/home/abdullah/.bashrc",
        "/root/.ssh/authorized_keys",
        "/tmp/scratch",
        "/run/nope",
        "/proc/nope",
        "/dev/nope",
        "/mnt/nope",
        "relative/path",
        "/usr/lib/trailing/",
        "/usr/lib/../etc/sneaky",
        "/toplevel",
    ];
    let rendered: Vec<String> = targets
        .iter()
        .map(|t| match overlay::route(t) {
            Ok(Route::Direct(at)) => format!("{t}\n  → {at}"),
            Ok(Route::Factory { at, restores }) => format!("{t}\n  → {at}\n  C {restores}"),
            Err(r) => {
                let hint = r.hint.map(|h| format!("\n      {h}")).unwrap_or_default();
                format!("{t}\n  ✗ {}{hint}", r.why)
            }
        })
        .collect();
    insta::assert_snapshot!(rendered.join("\n\n"));
}

/// CLAUDE.md's principle, made mechanical: "the user should never have to type
/// `ostree`". `/usr/etc` is the *result* of normalization. Accepting a write
/// there would work by accident today and collide with the `/etc` move
/// tomorrow, so it is refused with the path the user actually wanted.
#[test]
fn usr_etc_is_refused_with_the_path_to_write_instead() {
    let refusal = overlay::route("/usr/etc/sudoers.d/10-wheel").unwrap_err();
    assert_eq!(
        refusal.hint.as_deref(),
        Some("write `/etc/sudoers.d/10-wheel` instead")
    );
}

/// Normalization relocates `/opt` and `/srv` into `/var` before draining, so a file
/// targeting either is `/var` content and must take the same route — otherwise
/// it lands in a directory that becomes a symlink and is lost.
#[test]
fn opt_and_srv_are_var_content() {
    assert_eq!(
        overlay::route("/opt/thing/config"),
        Ok(Route::Factory {
            at: "usr/share/factory/var/opt/thing/config".into(),
            restores: "/var/opt/thing/config".into(),
        })
    );
}

#[test]
fn inline_content_lands_with_the_mode_it_asked_for() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch::root("overlay-inline");
    let config = scratch::root("overlay-inline-config");
    let entries = BTreeMap::from([entry("/etc/motd", "welcome\n", Some(0o600))]);

    let applied = overlay::apply(&root, &config, &entries, &NoOwners).unwrap();
    assert_eq!(applied.placed.len(), 1);
    assert_eq!(
        std::fs::read_to_string(root.join("etc/motd")).unwrap(),
        "welcome\n"
    );
    let mode = root
        .join("etc/motd")
        .metadata()
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o600);
}

/// An omitted `mode` preserves the source's mode, "masked to 0755/0644".
/// Read as quantized, not bitwise — a source that happens to be 0664 because of
/// the builder's umask must not put 0664 in the image, or the commit depends on
/// the machine that built it.
#[test]
fn an_omitted_mode_quantizes_rather_than_copying_the_builders_umask() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch::root("overlay-mode");
    let config = scratch::root("overlay-mode-config");
    scratch::file(&config, "files/motd", "hello\n", 0o664);
    scratch::file(&config, "bin/tool", "#!/bin/sh\n", 0o770);
    let entries = BTreeMap::from([
        from_source("/etc/motd", "files/motd"),
        from_source("/usr/bin/tool", "bin/tool"),
    ]);

    overlay::apply(&root, &config, &entries, &NoOwners).unwrap();
    let mode = |p: &str| root.join(p).metadata().unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode("etc/motd"), 0o644);
    assert_eq!(mode("usr/bin/tool"), 0o755);
}

#[test]
fn a_trailing_slash_copies_a_tree() {
    let root = scratch::root("overlay-tree");
    let config = scratch::root("overlay-tree-config");
    scratch::file(&config, "sysctl/10-a.conf", "a=1\n", 0o644);
    scratch::file(&config, "sysctl/nested/20-b.conf", "b=2\n", 0o644);
    let entries = BTreeMap::from([from_source("/usr/lib/sysctl.d", "sysctl/")]);

    let applied = overlay::apply(&root, &config, &entries, &NoOwners).unwrap();
    let mut at: Vec<&str> = applied.placed.iter().map(|p| p.at.as_str()).collect();
    at.sort();
    assert_eq!(
        at,
        [
            "usr/lib/sysctl.d/10-a.conf",
            "usr/lib/sysctl.d/nested/20-b.conf"
        ]
    );
    assert!(root.join("usr/lib/sysctl.d/nested/20-b.conf").is_file());
}

/// A symlink in a source tree is copied as a symlink. Following it would
/// resolve it against the *builder's* filesystem — the target is a path in the
/// image, which does not exist yet and would not mean the same thing if it did.
#[test]
fn a_symlink_is_copied_as_a_symlink() {
    let root = scratch::root("overlay-link");
    let config = scratch::root("overlay-link-config");
    scratch::file(&config, "lib/libfoo.so.1", "elf\n", 0o644);
    scratch::link(&config, "lib/libfoo.so", "libfoo.so.1");
    let entries = BTreeMap::from([from_source("/usr/lib", "lib/")]);

    overlay::apply(&root, &config, &entries, &NoOwners).unwrap();
    let link = root.join("usr/lib/libfoo.so");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(&link).unwrap().to_str(),
        Some("libfoo.so.1")
    );
}

/// Assembly step 6: checked against the pacman file DB. Overwriting a package's file
/// is almost always a mistake, and the mistake is invisible — the file is in
/// the image and the next update to that package silently takes it back.
#[test]
fn overwriting_a_package_owned_file_is_refused_and_names_the_package() {
    let root = scratch::root("overlay-conflict");
    let config = scratch::root("overlay-conflict-config");
    let entries = BTreeMap::from([entry(
        "/usr/lib/systemd/system/sshd.service",
        "[Unit]\n",
        None,
    )]);

    let err = overlay::apply(&root, &config, &entries, &Fake("openssh")).unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("owned by the package `openssh`"), "{text}");
    assert!(text.contains("drop-in"), "{text}");
    assert!(
        !root.join("usr/lib/systemd/system/sshd.service").exists(),
        "nothing is written when an entry is refused"
    );
}

/// every error in a phase, not the first one. Four impossible targets
/// are one run and four lines.
#[test]
fn every_refusal_is_reported_at_once() {
    let root = scratch::root("overlay-many");
    let config = scratch::root("overlay-many-config");
    let entries = BTreeMap::from([
        entry("/boot/x", "", None),
        entry("/home/me/x", "", None),
        entry("/proc/x", "", None),
        entry("/usr/bin/fine", "", None),
    ]);

    let err = overlay::apply(&root, &config, &entries, &NoOwners).unwrap_err();
    insta::assert_snapshot!(format!("{err}"));
}

/// A conflict must be found before anything is written. Otherwise a build fails
/// halfway with a partly-overlaid tree, and the next step works on it.
#[test]
fn a_refusal_writes_nothing_at_all() {
    let root = scratch::root("overlay-atomic");
    let config = scratch::root("overlay-atomic-config");
    let entries = BTreeMap::from([
        entry("/usr/lib/first.conf", "fine\n", None),
        entry("/proc/impossible", "", None),
    ]);

    assert!(overlay::apply(&root, &config, &entries, &NoOwners).is_err());
    assert!(!root.join("usr/lib/first.conf").exists());
}

/// A `/var` target becomes a *default*, not a file. That is a surprise worth
/// saying out loud: it is restored on a machine that has none and left alone on
/// a machine that has its own.
#[test]
fn a_var_target_becomes_a_factory_default_and_says_so() {
    let root = scratch::root("overlay-var");
    let config = scratch::root("overlay-var-config");
    let entries = BTreeMap::from([entry("/var/lib/myapp/seed.db", "seed\n", None)]);

    let applied = overlay::apply(&root, &config, &entries, &NoOwners).unwrap();
    assert!(root
        .join("usr/share/factory/var/lib/myapp/seed.db")
        .is_file());
    assert!(!root.join("var").exists(), "/var is not in the commit");

    let conf = std::fs::read_to_string(root.join(overlay::TMPFILES_PATH)).unwrap();
    assert!(
        conf.contains("C /var/lib/myapp/seed.db - - - -\n"),
        "{conf}"
    );
    insta::assert_snapshot!(applied.notes.join("\n"));
}

/// The drain owns `kiln-var.conf`; the overlay owns `kiln.conf`. Keeping them
/// apart is what lets `kiln owns /var/lib/myapp/seed.db` answer "your
/// configuration" rather than "some package".
#[test]
fn the_overlays_tmpfiles_fragment_is_not_the_drains() {
    assert_ne!(overlay::TMPFILES_PATH, "usr/lib/tmpfiles.d/kiln-var.conf");
}

/// A conflict against a package that owns a `/var` path is about the `/var`
/// path, not about the factory copy — no package has ever heard of
/// `/usr/share/factory`.
#[test]
fn a_var_conflict_names_the_var_path() {
    let root = scratch::root("overlay-var-conflict");
    let config = scratch::root("overlay-var-conflict-config");
    let entries = BTreeMap::from([entry("/var/lib/myapp/seed.db", "seed\n", None)]);

    let err = overlay::apply(
        &root,
        &config,
        &entries,
        &Only("/var/lib/myapp/seed.db", "myapp"),
    )
    .unwrap_err();
    assert!(
        format!("{err}").contains("/var/lib/myapp/seed.db is owned"),
        "{err}"
    );
}

struct Fake(&'static str);

impl Owners for Fake {
    fn owner_of(&self, _: &str) -> Option<String> {
        Some(self.0.to_string())
    }
}

struct Only(&'static str, &'static str);

impl Owners for Only {
    fn owner_of(&self, path: &str) -> Option<String> {
        (path == self.0).then(|| self.1.to_string())
    }
}
