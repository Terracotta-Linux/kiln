//! The sandbox actually running. Everything in `isolation.rs` asserts on a
//! command line; this asserts that the command line *works* — that `bwrap`
//! accepts it, that the network really is gone, and that a shimmed binary
//! really is intercepted with its call readable afterwards.
//!
//! Skipped rather than failed where bubblewrap or unprivileged user namespaces
//! are unavailable: a CI runner without them should not turn the whole suite
//! red for a reason that has nothing to do with the code.

use kiln_sandbox::{Bubblewrap, Sandbox, SandboxSpec, Shim};
use std::path::{Path, PathBuf};

/// A usr-merged root that borrows the host's `/usr` read-only — the same shape
/// a staging root has, without needing a package transaction to build one.
fn borrowed_root(name: &str) -> (PathBuf, PathBuf) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/test-roots")
        .join(name);
    std::fs::remove_dir_all(&base).ok();
    let root = base.join("root");
    std::fs::create_dir_all(root.join("usr")).unwrap();
    std::fs::create_dir_all(base.join("sandbox")).unwrap();
    #[cfg(unix)]
    for (link, target) in [("bin", "usr/bin"), ("lib", "usr/lib"), ("lib64", "usr/lib")] {
        std::os::unix::fs::symlink(target, root.join(link)).unwrap();
    }
    (root, base.join("sandbox"))
}

fn usable() -> bool {
    // `bwrap --version` exercises the same unprivileged-user-namespace path the
    // real thing needs, so it answers both questions at once.
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn a_sandboxed_command_runs_with_the_environment_it_was_given() {
    if !usable() {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    }
    let (root, scratch) = borrowed_root("sandbox-env");
    let spec = SandboxSpec::in_root(
        &root,
        [
            "/bin/sh".into(),
            "-c".into(),
            "echo \"$LANG $SOURCE_DATE_EPOCH\"".into(),
        ],
    )
    .with_bind(kiln_sandbox::Bind::ro("/usr", "/usr"));

    let out = Bubblewrap::new(scratch).run(&spec).unwrap();
    assert_eq!(out.stdout.trim(), "C.UTF-8 0");
}

/// The network constraint the whole model rests on. Asserted by trying to use
/// the network, not by grepping an argv.
#[test]
fn a_build_step_genuinely_has_no_network() {
    if !usable() {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    }
    let (root, scratch) = borrowed_root("sandbox-network");
    let spec = SandboxSpec::in_root(
        &root,
        [
            "/bin/sh".into(),
            "-c".into(),
            // Every interface, not just whether a name resolves: DNS can fail
            // for reasons that are not isolation.
            "ip -o link show | wc -l".into(),
        ],
    )
    .with_bind(kiln_sandbox::Bind::ro("/usr", "/usr"));

    let out = Bubblewrap::new(scratch).run(&spec).unwrap();
    assert_eq!(
        out.stdout.trim(),
        "1",
        "a build step must see loopback and nothing else"
    );
}

/// `kiln build -v` should be able to say `shimmed: systemctl
/// daemon-reload`. That requires the call to be intercepted *and* the record of
/// it to survive the sandbox exiting.
#[test]
fn a_shimmed_binary_is_intercepted_and_the_call_survives() {
    if !usable() {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    }
    let (root, scratch) = borrowed_root("sandbox-shims");
    let spec = SandboxSpec::in_root(
        &root,
        [
            "/bin/sh".into(),
            "-c".into(),
            "systemctl daemon-reload && udevadm trigger && echo did-not-fail".into(),
        ],
    )
    .with_bind(kiln_sandbox::Bind::ro("/usr", "/usr"))
    .with_shims(Shim::hostile_to_images());

    let out = Bubblewrap::new(scratch).run(&spec).unwrap();
    assert_eq!(out.stdout.trim(), "did-not-fail");
    assert_eq!(
        out.shimmed,
        ["systemctl daemon-reload", "udevadm trigger"],
        "both calls must be recorded, in order"
    );
}

/// A failing step must fail the build with the command and its output, not with
/// a bare status code ("the package name and the last 40 lines").
#[test]
fn a_failing_command_reports_what_failed_and_what_it_said() {
    if !usable() {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    }
    let (root, scratch) = borrowed_root("sandbox-failure");
    let spec = SandboxSpec::in_root(
        &root,
        [
            "/bin/sh".into(),
            "-c".into(),
            "echo 'something went wrong' >&2; exit 3".into(),
        ],
    )
    .with_bind(kiln_sandbox::Bind::ro("/usr", "/usr"));

    let err = Bubblewrap::new(scratch).run(&spec).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("exit status 3"), "got: {text}");
    assert!(text.contains("something went wrong"), "got: {text}");
}

/// A command that writes more than a pipe buffer must not hang.
///
/// This is a bug that happened: `wait()` before reading the pipes deadlocks the
/// moment a command writes past 64 KiB, because the child blocks on a full pipe
/// nobody is draining and the parent waits for a child that will never exit. It
/// survives every small command in this file and then hangs forever on the
/// first big one — `lsinitrd` over a 22 MB initramfs — with no output
/// and no error, which is the worst way for a build to fail.
///
/// A megabyte, and a timeout, so a regression is a red test in seconds rather
/// than a suite that never finishes.
#[test]
fn a_command_that_writes_megabytes_does_not_deadlock() {
    if !usable() {
        eprintln!("skipped: bubblewrap or user namespaces unavailable");
        return;
    }
    let (root, scratch) = borrowed_root("live-bigoutput");
    // 32768 lines of 32 characters on each stream: comfortably past the pipe
    // buffer on both, which is what makes the deadlock certain rather than
    // likely.
    let script = "i=0; while [ $i -lt 32768 ]; do \
                  echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; \
                  echo 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' >&2; \
                  i=$((i+1)); done";
    let spec = SandboxSpec::in_root(&root, ["/bin/sh".to_string(), "-c".into(), script.into()])
        .with_bind(kiln_sandbox::Bind::ro("/usr", "/usr"))
        .with_timeout(std::time::Duration::from_secs(60));

    let outcome = Bubblewrap::new(scratch)
        .run(&spec)
        .expect("a command writing megabytes must finish");
    assert!(outcome.ok(), "{}", outcome.stderr);
    assert_eq!(outcome.stdout.lines().count(), 32768);
    assert_eq!(outcome.stderr.lines().count(), 32768);
}
