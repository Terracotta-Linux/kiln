//! What the isolation actually is.
//!
//! asks for tests that assert on the exact `SandboxSpec` — "that
//! the build phase really has `Network::Disabled`". These go one step further
//! and assert on the **argv each backend produces**, because a spec that says
//! `Network::Disabled` and a backend that forgets `--unshare-net` is precisely
//! the failure a spec-only test cannot see.
//!
//! None of this needs root, a container, or the backends to be installed.

use kiln_sandbox::{Bubblewrap, Network, Nspawn, Sandbox, SandboxSpec, SandboxUser, Shim};

fn spec() -> SandboxSpec {
    SandboxSpec::in_root(
        "/var/lib/kiln/build/b3-7f2a/root",
        [
            "depmod".to_string(),
            "-a".to_string(),
            "6.19.2-arch1".to_string(),
        ],
    )
}

fn bwrap() -> Bubblewrap {
    Bubblewrap::new("/var/lib/kiln/build/b3-7f2a/sandbox")
}

fn render(argv: &[String]) -> String {
    argv.join(" ")
}

/// The network constraint the rest of the model rests on: *a script runs with
/// `CLONE_NEWNET` and no interfaces. Not configurable.* With the network off, a
/// build step's output is a pure function of things Kiln already hashes.
#[test]
fn a_build_step_has_no_network_by_default() {
    assert_eq!(spec().network, Network::Disabled);
    assert!(render(&bwrap().argv(&spec()).unwrap()).contains("--unshare-net"));
    assert!(render(&Nspawn::new("/tmp/s").argv(&spec()).unwrap()).contains("--private-network"));
}

/// The default must survive someone constructing a spec without thinking about
/// it. This is the test that fails if `in_root` ever grows a network default of
/// `Enabled` for convenience.
#[test]
fn network_must_be_asked_for_explicitly() {
    let enabled = spec().with_network(Network::Enabled);
    assert!(!render(&bwrap().argv(&enabled).unwrap()).contains("--unshare-net"));
}

#[test]
fn the_full_bubblewrap_command_line() {
    insta::assert_snapshot!(render(&bwrap().argv(&spec()).unwrap()));
}

#[test]
fn the_full_nspawn_command_line() {
    insta::assert_snapshot!(render(
        &Nspawn::new("/var/lib/kiln/build/b3-7f2a/sandbox")
            .argv(&spec())
            .unwrap()
    ));
}

/// hostile scriptlet behaviour is neutralized by shimming, and shimming
/// only works if the wrappers come first on `PATH`.
#[test]
fn shims_win_over_the_images_own_binaries() {
    let s = spec().with_shims(Shim::hostile_to_images());
    assert!(s.effective_path().starts_with(kiln_sandbox::SHIM_DIR));

    let argv = render(&bwrap().argv(&s).unwrap());
    assert!(argv.contains(&format!(
        "--ro-bind /var/lib/kiln/build/b3-7f2a/sandbox/shims {}",
        kiln_sandbox::SHIM_DIR
    )));
    // The log is a *file* bind from the host. Without it the shims would append
    // to the `/run` tmpfs and the record of what a scriptlet tried to do would
    // vanish with the namespace.
    assert!(argv.contains(&format!(
        "--bind /var/lib/kiln/build/b3-7f2a/sandbox/shims/calls.log {}",
        kiln_sandbox::SHIM_LOG
    )));
}

#[test]
fn a_shim_records_the_call_and_succeeds() {
    let script = Shim::new("systemctl").script();
    assert!(script.starts_with("#!/bin/sh\n"));
    assert!(script.contains("exit 0"));
    assert!(script.contains(kiln_sandbox::SHIM_LOG));
    // One call per line. A shell `printf '%s\\n'` writes a literal
    // backslash-n instead of a newline, and every shimmed call then lands on
    // one unreadable line — which is exactly what happened the first time.
    assert!(
        script.contains(r"printf '%s\n'"),
        "the newline must not be double-escaped:\n{script}"
    );
}

/// the environment is cleared, then explicitly populated. A build that
/// can see the host's environment can behave differently on two machines with
/// identical inputs.
#[test]
fn the_environment_is_cleared_then_populated() {
    let argv = render(&bwrap().argv(&spec()).unwrap());
    assert!(argv.contains("--clearenv"));
    assert!(argv.contains("--setenv SOURCE_DATE_EPOCH 0"));
    assert!(argv.contains("--setenv LANG C.UTF-8"));
}

/// Kiln puts cgroup limits on the nspawn backend. bubblewrap must refuse them
/// rather than ignore them: a caller that asked for a memory cap and silently
/// did not get one is worse off than one that never asked.
#[test]
fn bubblewrap_refuses_limits_it_cannot_enforce() {
    let mut s = spec();
    s.limits.memory = Some(2 << 30);
    let err = bwrap().argv(&s).unwrap_err();
    assert!(err.to_string().contains("cannot enforce"), "got: {err}");
    // nspawn can, and says so in the argv rather than in a comment.
    assert!(render(&Nspawn::new("/tmp/s").argv(&s).unwrap())
        .contains("--property MemoryMax=2147483648"));
}

/// The reverse: an unprivileged user is bubblewrap's job, because nspawn's
/// `--user` resolves inside a container that may not have the account yet.
#[test]
fn nspawn_refuses_an_unprivileged_user_rather_than_running_as_root() {
    let s = spec().with_user(SandboxUser::Unprivileged {
        uid: 1000,
        gid: 1000,
    });
    let err = Nspawn::new("/tmp/s").argv(&s).unwrap_err();
    assert!(err.to_string().contains("cannot enforce"), "got: {err}");

    let argv = render(&bwrap().argv(&s).unwrap());
    assert!(
        argv.contains("setpriv --reuid 1000 --regid 1000 --clear-groups"),
        "{argv}"
    );
}

/// Privileges are dropped *inside* the sandbox, never by remapping the user
/// namespace around it — and this is the assertion, because `--unshare-user
/// --uid 1000` is the obvious-looking version and is wrong twice.
///
/// It maps exactly one id, so bubblewrap resolves every bind source as the
/// mapped user and a source whose *ancestor* belongs to somebody else becomes
/// `Can't find source path …: Permission denied` about a path that is plainly
/// there. And it maps the caller — root — *onto* the build user, so
/// everything root owns appears to belong to the build. In phase 1's sandbox,
/// which binds the host `/`, that is write access to the whole machine.
#[test]
fn dropping_privileges_does_not_remap_root_onto_the_build_user() {
    let s = spec().with_user(SandboxUser::Unprivileged {
        uid: 1000,
        gid: 1000,
    });
    let argv = render(&bwrap().argv(&s).unwrap());
    assert!(!argv.contains("--unshare-user"), "{argv}");
    assert!(!argv.contains("--uid"), "{argv}");
    // The drop happens after `--`, so it is the sandboxed command that loses
    // the privilege and bubblewrap keeps what it needs to do the mounts.
    let (before, after) = argv.split_once(" -- ").expect("a command separator");
    assert!(!before.contains("setpriv"), "{before}");
    assert!(after.starts_with("setpriv "), "{after}");
}

/// A command that runs as root gets bubblewrap's own tmpfs; one that does not
/// gets a `/tmp` it can actually write to. Widening the mode for root would buy
/// nothing, so it is not done.
#[test]
fn a_private_tmpfs_is_writable_exactly_when_the_command_is_not_root() {
    let unprivileged = render(
        &bwrap()
            .argv(&spec().with_user(SandboxUser::Unprivileged {
                uid: 1000,
                gid: 1000,
            }))
            .unwrap(),
    );
    assert!(
        unprivileged.contains("--perms 1777 --tmpfs /tmp"),
        "{unprivileged}"
    );

    let as_root = render(&bwrap().argv(&spec()).unwrap());
    assert!(as_root.contains("--tmpfs /tmp"), "{as_root}");
    assert!(!as_root.contains("--perms"), "{as_root}");
}

#[test]
fn an_empty_command_is_refused_by_both_backends() {
    let mut s = spec();
    s.command.clear();
    assert!(bwrap().argv(&s).is_err());
    assert!(Nspawn::new("/tmp/s").argv(&s).is_err());
}
