//! The command surface, exercised as a user meets it.
//!
//! Through the real binary rather than through the modules, because what is
//! being checked is argv in and exit code out — and the exit codes are a
//! documented interface (0 ok, 1 config, 2 resolution, 3 build, 4 system,
//! 10 `kiln check` found changes), not an implementation detail.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn kiln(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(args)
        .output()
        .expect("the kiln binary should run")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/test-roots")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn help_lists_every_command_the_design_promises() {
    let out = kiln(&["help"]);
    assert_eq!(code(&out), 0);
    let text = stdout(&out);
    for verb in [
        "check",
        "build",
        "apply",
        "rebuild",
        "explain",
        "show",
        "diff",
        "why",
        "owns",
        "list",
        "status",
        "rollback",
        "deploy",
        "pin",
        "rm",
        "clean",
        "init",
        "sysroot init",
    ] {
        assert!(
            text.contains(verb),
            "help does not mention `{verb}`:\n{text}"
        );
    }
}

/// generations are the only IDs the CLI accepts, and it says why rather
/// than just refusing. Someone who typed an OSTree index has a specific wrong
/// model, and the message is the only chance to correct it.
#[test]
fn a_non_numeric_generation_explains_what_a_generation_is() {
    let out = kiln(&["deploy", "abc"]);
    assert_eq!(code(&out), 1);
    let text = stderr(&out);
    assert!(text.contains("not a generation number"), "{text}");
    assert!(text.contains("indices renumber"), "{text}");
}

#[test]
fn deploy_without_a_generation_says_where_to_find_one() {
    let out = kiln(&["deploy"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("kiln list"), "{}", stderr(&out));
}

/// Kiln's closed decisions, reachable from the command line. Someone arriving
/// from another tool types these, and "unknown command" would tell them nothing
/// about why.
#[test]
fn the_commands_that_will_never_exist_say_why() {
    for (argv, expect) in [
        (["upgrade"], "exactly one way to get a new image"),
        (["install"], "Installation is an installer's job"),
        (["push"], "not an image-shipping pipeline"),
    ] {
        let out = kiln(&argv);
        assert_eq!(code(&out), 1, "`kiln {}` should exit 1", argv[0]);
        assert!(
            stderr(&out).contains(expect),
            "`kiln {}` said:\n{}",
            argv[0],
            stderr(&out)
        );
    }
}

#[test]
fn an_unknown_command_suggests_the_near_miss() {
    let out = kiln(&["staus"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("status"), "{}", stderr(&out));
}

/// A directory that is not a sysroot exits 4 — system, not config — and points
/// at the command that would make it one.
#[test]
fn listing_a_directory_that_is_not_a_sysroot_says_how_to_make_one() {
    let dir = scratch("cli-not-a-sysroot");
    let out = kiln(&["--sysroot", dir.to_str().unwrap(), "list"]);
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("sysroot init"), "{}", stderr(&out));
}

/// Refusing up front is the difference between a clear message and an
/// image whose ownership, setuid bits and capabilities are all wrong — libalpm
/// logs those failures as *warnings* and reports success.
#[test]
fn building_as_an_ordinary_user_is_refused_before_anything_happens() {
    if is_root() {
        eprintln!("skipped: this test is about not being root");
        return;
    }
    let dir = scratch("cli-unprivileged");
    std::fs::write(dir.join("system.toml"), "kiln = 1\n").unwrap();
    let out = kiln(&["--config", dir.to_str().unwrap(), "build"]);
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("needs root"), "{}", stderr(&out));
}

/// `kiln sysroot` alone is a real mistake with a one-word fix, so the message
/// is the fix rather than a usage dump.
#[test]
fn sysroot_without_a_subcommand_names_the_only_one() {
    let out = kiln(&["sysroot"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("kiln sysroot init"),
        "{}",
        stderr(&out)
    );
}

/// The installer writes its step as `kiln sysroot init /mnt`, so that has
/// to be the form that works. It silently initialized `/` before, which is the
/// one root an installer is certain not to mean.
#[test]
fn sysroot_init_takes_the_target_the_design_writes_it_with() {
    let target = scratch("sysroot-init-positional");
    let out = kiln(&["sysroot", "init", target.to_str().unwrap()]);
    // Without root it cannot finish, but the failure must name the target it
    // was given rather than `/`.
    let said = format!("{}{}", stdout(&out), stderr(&out));
    assert!(said.contains(target.to_str().unwrap()), "{said}");
}

#[test]
fn two_different_sysroots_in_one_command_is_refused_rather_than_guessed() {
    let out = kiln(&["--sysroot", "/mnt", "sysroot", "init", "/other"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("name different roots"),
        "{}",
        stderr(&out)
    );
}

/// The corpus's `workstation`, which is the one configuration that exercises
/// every shape `kiln explain` has an answer for.
fn workstation(args: &[&str]) -> Output {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let config = root.join("tests/corpus/valid/workstation");
    let modules = root.join("modules");
    let mut argv = vec![
        "--config",
        config.to_str().unwrap(),
        "--module-root",
        modules.to_str().unwrap(),
    ];
    argv.extend_from_slice(args);
    kiln(&argv)
}

/// The headline promise, which was not being kept: the payoff for carrying
/// spans is a *module name*, and an absolute path into the module root is the
/// same information in the form nobody can use.
#[test]
fn explain_names_a_module_the_way_the_user_wrote_it() {
    let out = workstation(&["explain", "kernel.cmdline"]);
    assert_eq!(code(&out), 0);
    let said = stdout(&out);
    assert!(said.contains("@kiln/gpu/nvidia-open:14"), "{said}");
    assert!(!said.contains("/modules/gpu"), "{said}");
}

/// A list has contributors, not a winner — and now says which
/// contributor asked for which element.
#[test]
fn explain_a_list_names_the_file_behind_each_element() {
    let out = workstation(&["explain", "packages.repo"]);
    assert_eq!(code(&out), 0);
    let said = stdout(&out);
    assert!(said.contains("unions into it"), "{said}");
    assert!(said.contains("neovim"), "{said}");
    assert!(said.contains("gnome-shell"), "{said}");
    assert!(said.contains("@kiln/desktop/gnome:5"), "{said}");
    // "overriding" would misdescribe a union.
    assert!(!said.contains("overriding"), "{said}");
}

#[test]
fn explain_an_element_answers_which_file_asked_for_it() {
    let out = workstation(&["explain", "packages.repo/gnome-shell"]);
    assert_eq!(code(&out), 0);
    assert!(
        stdout(&out).contains("@kiln/desktop/gnome:5"),
        "{}",
        stdout(&out)
    );

    // A near miss inside a list suggests the sibling rather than the schema.
    let out = workstation(&["explain", "packages.repo/nvim"]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("neovim"), "{}", stdout(&out));
}

/// A group lists what is under it, *including* the keys nothing set — which is
/// the half somebody asking "what can I put in [boot]" actually needs.
#[test]
fn explain_a_group_lists_the_defaults_too() {
    let out = workstation(&["explain", "boot"]);
    assert_eq!(code(&out), 0);
    let said = stdout(&out);
    assert!(said.contains("boot.timeout"), "{said}");
    assert!(said.contains("boot.initramfs"), "{said}");
    assert!(said.contains("Kiln's default"), "{said}");
}

/// Three different kinds of nothing, and only the last is a mistake.
#[test]
fn explain_tells_the_three_kinds_of_unset_apart() {
    // A default.
    let out = workstation(&["explain", "kernel.headers"]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("Kiln's default"), "{}", stdout(&out));

    // A list nothing contributes to. Not an error: an included file could have.
    let out = workstation(&["explain", "packages.file"]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("empty"), "{}", stdout(&out));

    // A key that does not exist.
    let out = workstation(&["explain", "boot.timout"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("boot.timeout"), "{}", stderr(&out));
}

/// nothing is glob-loaded, so the include graph is the documentation of
/// what a system is made of. `include` is consumed by the graph and is not in
/// the merged document, so the generic answer would have been "empty".
#[test]
fn explain_include_prints_the_graph_rather_than_a_value() {
    let out = workstation(&["explain", "include"]);
    assert_eq!(code(&out), 0);
    let said = stdout(&out);
    assert!(said.contains("system.toml"), "{said}");
    assert!(said.contains("hardware.toml"), "{said}");
    assert!(said.contains("@kiln/profiles/minimal"), "{said}");
    assert!(!said.contains("empty"), "{said}");
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(2)?
                .parse::<u32>()
                .ok()
        })
        == Some(0)
}

/// `kiln rebuild` takes a generation like every other command that
/// names one, and says so rather than parsing `rebuild` as a request to rebuild
/// something unspecified.
#[test]
fn rebuild_without_a_generation_says_where_to_find_one() {
    let out = kiln(&["rebuild"]);
    assert_eq!(code(&out), 1);
    let text = stderr(&out);
    assert!(text.contains("kiln rebuild 41"), "got: {text}");
    assert!(text.contains("kiln list"), "got: {text}");
}

/// An OSTree deployment index is not a generation, and typing one has to
/// be an error with an explanation rather than a rebuild of the wrong image.
#[test]
fn rebuild_refuses_something_that_is_not_a_generation_number() {
    let out = kiln(&["rebuild", "latest"]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("not a generation number"));
}

/// `--deep` exists to fetch the inputs that cannot be resolved without
/// fetching, so `--deep --offline` asks for two opposite things. Refusing with
/// an explanation beats silently honouring one of them — which would be a
/// `check` that quietly answered a different question from the one asked.
#[test]
fn deep_and_offline_are_refused_together() {
    let dir = scratch("cli-deep-offline");
    std::fs::write(dir.join("system.toml"), "kiln = 1\n").unwrap();
    let out = kiln(&[
        "--config",
        dir.to_str().unwrap(),
        "check",
        "--deep",
        "--offline",
    ]);
    assert_eq!(code(&out), 1);
    let text = stderr(&out);
    assert!(text.contains("opposite"), "got: {text}");
    assert!(text.contains("--deep"), "got: {text}");
}

// ── phase 4 ─────────────────────────────────────────────────────────────────

/// Every verb in the design's CLI surface is a real command now, so none
/// of them may fall through to "unknown command". The phase-3 build answered
/// `diff`, `why`, `owns` and `rm` with a "not yet" and exit 2; a user typing one
/// today must get the command, not an apology.
#[test]
fn the_inspection_commands_are_real_rather_than_deferred() {
    for argv in [
        vec!["diff"],
        vec!["why", "mesa"],
        vec!["owns", "/usr/bin/ls"],
        vec!["rm", "3"],
    ] {
        let dir = scratch(&format!("cli-real-{}", argv[0]));
        let mut full = vec!["--sysroot", dir.to_str().unwrap()];
        full.extend(argv.iter().copied());
        let out = kiln(&full);
        let text = format!("{}{}", stdout(&out), stderr(&out));
        assert!(
            !text.contains("not yet") && !text.contains("unknown command"),
            "`kiln {}` is still deferred:\n{text}",
            argv[0]
        );
        // An empty directory is not a sysroot, so the honest answer is 4 —
        // system, not "this command does not exist".
        assert_eq!(code(&out), 4, "`kiln {}` said:\n{text}", argv[0]);
        assert!(text.contains("sysroot init"), "{text}");
    }
}

/// Also true for the commands phase 4 added: the explanation of what a generation is
/// belongs to every command that takes one, not only to the ones that had it
/// first.
#[test]
fn the_new_commands_explain_what_a_generation_is() {
    for argv in [vec!["rm", "abc"], vec!["diff", "abc"], vec!["show", "abc"]] {
        let out = kiln(&argv);
        assert_eq!(code(&out), 1, "`kiln {}` should exit 1", argv[0]);
        assert!(
            stderr(&out).contains("not a generation number"),
            "`kiln {}` said:\n{}",
            argv[0],
            stderr(&out)
        );
    }
}

#[test]
fn rm_without_a_generation_says_where_to_find_one() {
    let out = kiln(&["rm"]);
    assert_eq!(code(&out), 1);
    let text = stderr(&out);
    assert!(text.contains("at least one generation"), "got: {text}");
    assert!(text.contains("kiln list"), "got: {text}");
}

/// `--keep` takes a value, and the parser has to know that before the
/// verb is dispatched — otherwise `kiln clean --keep 2` reads as a request to
/// clean *generation 2*, which is a different command with a worse outcome.
#[test]
fn clean_keep_takes_its_argument_rather_than_leaving_it_positional() {
    let dir = scratch("cli-clean-keep");
    let out = kiln(&["--sysroot", dir.to_str().unwrap(), "clean", "--keep", "2"]);
    // Not a sysroot, so it fails — but on the sysroot, never on parsing.
    assert_eq!(code(&out), 4, "{}", stderr(&out));

    let bad = kiln(&["clean", "--keep", "many"]);
    assert_eq!(code(&bad), 1);
    assert!(stderr(&bad).contains("--keep many"), "{}", stderr(&bad));
}

/// The two crates have to agree on one path, and they cannot share a
/// constant: `kiln-image` writes the script into the tree and `kiln-ostree`
/// looks for it in a commit before arming the boot counter, and neither crate
/// depends on the other. If they drift, arming silently stops happening and the
/// only symptom is a feature that is quietly off.
#[test]
fn the_two_crates_agree_on_where_the_bless_script_lives() {
    assert_eq!(
        kiln_image::bootcount::SCRIPT,
        kiln_ostree::grubenv::BLESS,
        "kiln-image writes the boot-success script somewhere kiln-ostree does not look"
    );
}

/// The counting ladder in the generated grub.d fragment must have one
/// branch per attempt: a snippet built for three tries and a counter armed at
/// five would spend two attempts in a branch that does not exist and fall
/// straight through to the rollback entry on the first boot.
#[test]
fn the_boot_counter_ladder_matches_the_number_of_tries() {
    let snippet = kiln_image::bootcount::snippet(kiln_image::bootcount::TRIES);
    let branches = snippet.matches("set boot_counter=").count();
    assert_eq!(
        branches as u32,
        kiln_image::bootcount::TRIES,
        "the ladder and TRIES disagree:\n{snippet}"
    );
}

/// A target that was **built into** before it was initialized has an
/// `ostree/repo` and no `ostree/deploy`, and every deployment command fails on
/// it with libostree's `fstatat(ostree/deploy): No such file or directory`.
///
/// The hint used to be gated on the repository being *absent*, which suppressed
/// it in exactly this case — the one where the user has done several minutes of
/// work and needs one command, not a syscall name. Recorded as a test because
/// the wrong condition looked entirely reasonable.
#[test]
fn a_sysroot_built_into_before_it_was_initialized_says_so() {
    let dir = scratch("cli-built-not-initialized");
    // What `kiln build --sysroot` leaves behind, and nothing more.
    std::fs::create_dir_all(dir.join("ostree/repo")).unwrap();

    for argv in [vec!["deploy", "1"], vec!["list"], vec!["status"]] {
        let mut full = vec!["--sysroot", dir.to_str().unwrap()];
        full.extend(argv.iter().copied());
        let out = kiln(&full);
        let text = stderr(&out);
        assert_eq!(code(&out), 4, "`kiln {}` said:\n{text}", argv[0]);
        assert!(
            text.contains("kiln sysroot init"),
            "`kiln {}` did not name the command that fixes it:\n{text}",
            argv[0]
        );
        assert!(
            text.contains("Nothing is lost"),
            "`kiln {}` did not say the commits survive:\n{text}",
            argv[0]
        );
    }
}
