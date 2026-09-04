//! UID pinning: the seed's bytes, the capture, and the drift report.
//! step 3.
//!
//! Everything here is unprivileged. `systemd-sysusers` is behind a trait, so
//! the part that decides a generation's account layout — the fragment's text —
//! is tested as a value, and the part that shells out is tested for *what it
//! was asked to do*.

mod scratch;

use kiln_image::uid::{self, Drift, Sysusers};
use kiln_resolve::{IdEntry, UidMap};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

fn user(uid: u32, gid: u32, home: &str, shell: &str) -> IdEntry {
    IdEntry {
        uid,
        gid,
        home: home.into(),
        shell: shell.into(),
    }
}

fn arch_like() -> UidMap {
    let mut map = UidMap::new();
    map.groups.insert("systemd-journal".into(), 972);
    map.groups.insert("http".into(), 33);
    map.groups.insert("users".into(), 100);
    map.users
        .insert("http".into(), user(33, 33, "/srv/http", "/usr/bin/nologin"));
    // The awkward one: `games` has no group of its own name, and its primary
    // group is `users`.
    map.users.insert(
        "games".into(),
        user(12, 100, "/usr/share/games", "/usr/bin/nologin"),
    );
    map
}

#[test]
fn the_seed_names_groups_by_name_and_never_invents_one() {
    insta::assert_snapshot!(uid::render_seed(&arch_like()));
}

/// The reason `UidMap` is two maps rather than one. `http` owns the group
/// `http`; `games` does not own a group `games`, it borrows `users`. A flat map
/// of optional ids cannot tell those apart, and getting it backwards either
/// invents a group at gid 100 named `games` or leaves `u games 12:100`
/// referring to a gid no line creates.
#[test]
fn a_user_without_a_group_of_its_own_name_borrows_the_right_one() {
    let text = uid::render_seed(&arch_like());
    assert!(text.contains("u games 12:users - /usr/share/games /usr/bin/nologin\n"));
    assert!(text.contains("u http 33:http - /srv/http /usr/bin/nologin\n"));
    assert!(!text.contains("g games"));
}

/// Groups have to be declared before the `u` lines that name them: sysusers
/// requires the gid a `u` line mentions to be created by some line.
#[test]
fn groups_come_before_the_users_that_reference_them() {
    let text = uid::render_seed(&arch_like());
    let last_group = text.rfind("\ng ").expect("a g line");
    let first_user = text.find("\nu ").expect("a u line");
    assert!(last_group < first_user);
}

/// An empty home or shell renders as `-`, which is sysusers' "you choose" —
/// not as an empty field, which is a parse error.
#[test]
fn a_blank_home_renders_as_a_dash() {
    let mut map = UidMap::new();
    map.users.insert("nullmail".into(), user(8, 8, "", ""));
    assert!(uid::render_seed(&map).contains("u nullmail 8:8 - - -\n"));
}

#[test]
fn capture_reads_back_what_the_tree_has() {
    let root = scratch::root("uid-capture");
    scratch::file(
        &root,
        "etc/group",
        "root:x:0:\nhttp:x:33:\nsystemd-journal:x:972:\nusers:x:100:\n",
        0o644,
    );
    scratch::file(
        &root,
        "etc/passwd",
        "root:x:0:0::/root:/bin/bash\n\
         http:x:33:33::/srv/http:/usr/bin/nologin\n\
         games:x:12:100:games:/usr/share/games:/usr/bin/nologin\n",
        0o644,
    );

    let map = uid::capture(&root);
    assert_eq!(map.groups.get("systemd-journal"), Some(&972));
    assert_eq!(
        map.users.get("http"),
        Some(&user(33, 33, "/srv/http", "/usr/bin/nologin"))
    );
    assert_eq!(map.users.get("games").map(|e| e.gid), Some(100));
    assert!(map.groups.contains_key("users"));
}

/// Login accounts are not image content. If one is in the tree
/// — a hand-edited staging root, a package doing something strange — it must
/// not be replayed into the next generation, because Kiln does not manage it
/// and pinning it would claim otherwise.
#[test]
fn capture_stops_at_the_system_range() {
    let root = scratch::root("uid-login");
    scratch::file(&root, "etc/group", "abdullah:x:1000:\nhttp:x:33:\n", 0o644);
    scratch::file(
        &root,
        "etc/passwd",
        "abdullah:x:1000:1000::/home/abdullah:/bin/bash\nhttp:x:33:33::/srv/http:/usr/bin/nologin\n",
        0o644,
    );

    let map = uid::capture(&root);
    assert!(!map.users.contains_key("abdullah"));
    assert!(!map.groups.contains_key("abdullah"));
    assert!(map.users.contains_key("http"));
    assert_eq!(uid::SYSTEM_MAX, 999);
}

/// A generation-1 build has nothing to replay. Writing an empty fragment would
/// put a file in the image that claims to pin something and pins nothing.
#[test]
fn an_empty_map_writes_no_fragment_and_does_not_run_sysusers() {
    let root = scratch::root("uid-empty");
    let spy = Spy::default();
    uid::seed(&root, &UidMap::new(), &spy).unwrap();
    assert!(!root.join(uid::SEED_PATH).exists());
    assert_eq!(spy.calls(), Vec::<PathBuf>::new());
}

/// The seed runs against the staging root. Running it against `/` would edit
/// the *builder's* account files, which is the single worst thing in this
/// module and the reason the call is behind a trait at all.
#[test]
fn sysusers_is_pointed_at_the_staging_root() {
    let root = scratch::root("uid-seed");
    let spy = Spy::default();
    uid::seed(&root, &arch_like(), &spy).unwrap();

    assert_eq!(spy.calls(), vec![root.clone()]);
    let written = std::fs::read_to_string(root.join(uid::SEED_PATH)).unwrap();
    assert_eq!(written, uid::render_seed(&arch_like()));
}

/// The fragment has to be processed before any package's. Package fragments in
/// Arch are named after their package (`systemd.conf`, `dbus.conf`), so the
/// `00-` prefix is what makes "first" true by construction rather than by
/// alphabetical luck.
#[test]
fn the_seed_sorts_before_any_package_fragment() {
    let name = Path::new(uid::SEED_PATH).file_name().unwrap();
    assert!(name.to_str().unwrap().starts_with("00-"));
}

#[test]
fn drift_reports_a_moved_gid_and_a_vanished_account() {
    let seed = arch_like();
    let mut actual = arch_like();
    actual.groups.insert("systemd-journal".into(), 973);
    actual.users.remove("games");

    let found = uid::drift(&seed, &actual);
    assert_eq!(found.len(), 2);
    assert!(matches!(&found[0], Drift::Vanished { account, .. } if account == "games"));
    assert!(matches!(&found[1], Drift::Moved { account, was, now }
            if account == "systemd-journal" && was == "gid 972" && now == "gid 973"));
    insta::assert_snapshot!(found
        .iter()
        .map(Drift::describe)
        .collect::<Vec<_>>()
        .join("\n"));
}

/// A package changing an account's home is not drift. It is a package changing
/// its mind, which is legitimate; only the numbers are load-bearing for `/var`.
#[test]
fn a_moved_home_is_not_drift() {
    let seed = arch_like();
    let mut actual = arch_like();
    actual.users.get_mut("http").unwrap().home = "/var/www".into();
    assert_eq!(uid::drift(&seed, &actual), Vec::new());
}

#[derive(Default)]
struct Spy {
    calls: RefCell<Vec<PathBuf>>,
}

impl Spy {
    fn calls(&self) -> Vec<PathBuf> {
        self.calls.borrow().clone()
    }
}

impl Sysusers for Spy {
    fn run(&self, root: &Path) -> kiln_image::Result<()> {
        self.calls.borrow_mut().push(root.to_path_buf());
        Ok(())
    }
}
