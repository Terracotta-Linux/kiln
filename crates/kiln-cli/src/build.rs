//! `kiln build` and `kiln apply`.
//!
//! `build` produces a commit. `apply` produces a commit and stages it for the
//! next boot. There is no third thing: rules out live-apply, so the only
//! way a change reaches a running system is one image and one reboot.

use crate::pipeline::{self, Context};
use crate::{disk, paths};
use kiln_diag::ExitCode;
use kiln_image::assemble::{self, Options};
use kiln_ostree::commit::{self, CommitOptions};
use kiln_ostree::Sysroot;
use kiln_resolve::BuildPlan;
use kiln_sandbox::Bubblewrap;
use std::path::Path;

pub fn run(
    fe: &kiln_config::Frontend,
    ctx: &Context,
    force: bool,
    offline: bool,
    keep_failed: bool,
    deploy_after: bool,
) -> ExitCode {
    if !is_root() {
        eprintln!(
            "\x1b[1;31merror\x1b[0m building an image needs root: the transaction creates files \
             owned by root with the modes the packages declare.\n\n\
             As an ordinary user libalpm extracts the archive, fails every chown, logs a \
             *warning*, and reports success — producing a tree whose ownership, setuid bits \
             and capabilities are all wrong."
        );
        return ExitCode::System;
    }

    // The installer's table is an order, and nothing used to enforce it. `kiln build`
    // creates `ostree/repo` on its own, so building into a target that was
    // never initialized succeeds, commits real generations, and then fails at
    // `kiln deploy` with libostree's `fstatat(ostree/deploy)` — several minutes
    // and several hundred megabytes after the mistake was made. Said here, it
    // costs one command; discovered there, it reads like a broken build.
    if !paths::is_initialized(&ctx.sysroot) {
        eprintln!(
            "\x1b[1;33mwarning\x1b[0m {} is not an initialized Kiln sysroot, so the \
             generation this\n          build commits cannot be deployed there yet. Run \
             `kiln sysroot init --sysroot {}`\n          — before or after this build, it \
             takes no argument from either.",
            ctx.sysroot.display(),
            ctx.sysroot.display()
        );
    }

    let plan = match pipeline::plan(fe, ctx, offline) {
        Ok(plan) => plan,
        Err(code) => return code,
    };

    // Refusing a no-op is the whole reason the plan/realize split exists:
    // resolution is cheap enough to ask the question before paying for the
    // answer.
    if !force && pipeline::is_no_op(ctx, &plan) {
        println!("Nothing to do: the newest generation already matches this configuration.");
        println!("  plan {}", plan.plan_id());
        println!("\n`kiln build --force` rebuilds anyway.");
        return ExitCode::Ok;
    }
    pipeline::report_volatile(&plan);

    // Before anything is downloaded or unpacked, because the failure
    // this replaces is a pacman transaction that runs out of disk halfway
    // through — which leaves a staging root that is neither a tree nor nothing,
    // an hour spent, and an error from libalpm about one file.
    if let Some(warning) = headroom(ctx) {
        eprintln!("{warning}");
    }

    match build(&fe.manifest, ctx, &plan, keep_failed) {
        Err(code) => code,
        Ok((committed, _)) => {
            println!(
                "\nGeneration {} committed as {}.",
                committed.generation,
                &committed.checksum[..12]
            );
            if !deploy_after {
                println!("`kiln apply` stages it for the next boot.");
                return ExitCode::Ok;
            }
            stage(ctx, &committed, &fe.manifest)
        }
    }
}

/// The whole of a build, from a plan and the manifest it came from.
///
/// Takes a `Manifest` rather than a `Frontend` because `kiln rebuild` has no
/// frontend to offer: its manifest comes out of a commit, not out of
/// `/etc/kiln`, and that is the entire point of `kiln rebuild`.
///
/// The commit, and the record that went into it. Both, because `kiln rebuild`
/// compares the record it just produced against the one it started from
/// and re-reading it out of the commit to do that would be
/// asking libostree for something this function already has.
pub type Built = (commit::Committed, kiln_record::Record);

pub fn build(
    manifest: &kiln_manifest::Manifest,
    ctx: &Context,
    plan: &BuildPlan,
    keep_failed: bool,
) -> Result<Built, ExitCode> {
    let plan_id = plan.plan_id().to_string();
    let work = pipeline::fresh_build_dir(&ctx.state, &plan_id).map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m preparing the build directory: {e}");
        ExitCode::System
    })?;

    println!("Building {} ({})", manifest.image.name, &plan_id[..15]);

    // Realization, and the only network in a build. Assembly runs with it
    // off, which is what makes "installs from the artifact store" a fact rather
    // than a hope.
    //
    // Building comes before fetching, and that order is load-bearing: an AUR
    // package's runtime dependencies are named nowhere in the plan (stops
    // the closure wherever the official repositories can satisfy one), so they
    // are only discoverable from the artifact itself. `fetch` is handed the
    // built packages and lets libalpm resolve what they need.
    let repos = kiln_resolve::repositories(manifest);
    let network = kiln_aur::Network::default();
    let artifacts = crate::realize::realize(
        plan,
        &crate::realize::Options {
            ctx,
            manifest,
            repos: repos.clone(),
            work: work.clone(),
            keep_failed,
        },
        &network,
    )?;
    let fetched = crate::realize::fetch(plan, &ctx.state, repos.clone(), &artifacts)?;
    println!("  {fetched} packages fetched");

    // Decided once, and used for both copies of the record: the one the
    // assembler writes into the tree (step 11) and the one that goes into
    // the commit's metadata. Computing it twice is how they came to
    // disagree.
    let repo = commit::open_or_create(&paths::repo(&ctx.sysroot)).map_err(fail)?;
    let generation = commit::next_generation(&repo, &plan.image.ostree_ref()).map_err(fail)?;
    drop(repo);

    let opts = Options {
        root: work.join("root"),
        config_root: ctx.config_root.clone(),
        work: work.clone(),
        cache: paths::cache(&ctx.state),
        artifacts: artifacts.files(),
        repos,
        syncdb_from: ctx.state.join("cache/syncdb"),
        gpgdir: ctx.state.join("keyring"),
        generation,
    };

    let sandbox = Bubblewrap::new(work.join("sandbox"));
    let report = kiln_image::assemble::assemble(plan, manifest, &opts, &sandbox).map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m {e}");
        ExitCode::Build
    })?;

    describe(&report, ctx.verbose);

    let record = report
        .record
        .clone()
        .expect("a successful assembly always writes a record");
    let committed = commit::commit(
        &opts.root,
        plan,
        &record,
        manifest,
        &CommitOptions {
            repo: paths::repo(&ctx.sysroot),
            generation,
            built_by: paths::hostname(),
            subject: Some(format!("kiln {}", manifest.image.name)),
        },
    )
    .map_err(fail)?;

    // The staging root is cache, and keeping it after a *successful*
    // build wastes several gigabytes per generation for no reason: everything
    // in it is now in the commit.
    std::fs::remove_dir_all(&work).ok();
    Ok((committed, record))
}

pub fn stage(
    ctx: &Context,
    committed: &commit::Committed,
    manifest: &kiln_manifest::Manifest,
) -> ExitCode {
    let sysroot = match Sysroot::open(&ctx.sysroot) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\x1b[1;31merror\x1b[0m {e}");
            return ExitCode::System;
        }
    };
    // The generation goes on probation as it is staged: if it does not
    // reach `boot-complete.target` in `TRIES` attempts, GRUB selects the
    // rollback entry and `kiln status` says so.
    match sysroot.deploy(
        &committed.checksum,
        manifest,
        &manifest.image.name,
        kiln_image::bootcount::TRIES,
    ) {
        Ok(deployed) => {
            println!(
                "Generation {} is staged for the next boot.",
                deployed.generation
            );
            println!("Reboot to use it. `kiln rollback` returns to the previous one.");
            // Silent degradation here would be the worst kind:
            // the image boots, and the two things that stop working — a
            // regenerated grub.cfg and automatic rollback — are both invisible
            // until the day they are needed.
            if deployed.backend == kiln_ostree::Backend::None && ctx.sysroot == Path::new("/") {
                eprintln!(
                    "\x1b[1;33mwarning\x1b[0m this image ships no `grub`, so libostree writes \
                     BLS entries and\n          nothing regenerates /boot/grub/grub.cfg. Add \
                     `include = [\"@kiln/boot/grub2\"]`\n          to turn that on."
                );
            }
            // Said out loud rather than done quietly: a regular file
            // there means this machine would have stopped in the initramfs on
            // exactly this deploy, and whoever installed it should hear so.
            if deployed.grub_cfg_repaired {
                println!(
                    "Repaired /boot/grub/grub.cfg — it was a regular file naming a bootversion\n\
                     this deploy renames away, and is now a symlink to the configuration\n\
                     libostree regenerates."
                );
            }
            match &deployed.counted {
                kiln_ostree::Counter::Armed(tries) => println!(
                    "If it does not reach boot-complete.target in {tries} attempts, the \
                     previous generation boots instead."
                ),
                // Already explained by the `grub` warning above when that is
                // the cause; on a `--sysroot` deploy it is simply not this
                // machine's business yet.
                kiln_ostree::Counter::ImageCannotBless => {}
                kiln_ostree::Counter::Unwritable(why) => eprintln!(
                    "\x1b[1;33mwarning\x1b[0m the boot counter could not be armed, so \
                     automatic rollback on boot\n          failure is off for this generation \
: {why}"
                ),
            }
            if deployed.baseline {
                println!(
                    "Pinned as the baseline: `kiln clean` keeps it without `--remove-baseline` \
."
                );
            }
            ExitCode::Ok
        }
        Err(e) => {
            eprintln!("\x1b[1;31merror\x1b[0m {e}");
            ExitCode::System
        }
    }
}

/// "a build needs roughly twice the image size free, and Kiln checks
/// before starting rather than failing halfway through a transaction with a
/// full disk."
///
/// A **warning**, not a refusal. The estimate is exactly that, and a wrong
/// estimate that refuses to build is worse than a mid-build failure the user
/// was warned about. Kiln says what it thinks and lets the person decide.
///
/// The cheap question is asked first and usually ends it: if there is room for
/// even a generously-sized image, nothing is measured. Only a build that is
/// about to be warned about pays for walking the previous deployment — which is
/// several gigabytes of `stat` calls, and not something to spend on every build
/// to produce a warning that is not going to be printed.
fn headroom(ctx: &Context) -> Option<String> {
    let space = disk::space(&ctx.state.join("build"))?;
    if space.free >= disk::build_needs(disk::ASSUMED_IMAGE) {
        return None;
    }

    // Tight. Now it is worth being accurate rather than generous: the last
    // generation's real size is what this build is about to cost again.
    let previous = previous_image_size(ctx).filter(|size| *size > 0);
    let needed = disk::build_needs(previous.unwrap_or(disk::ASSUMED_IMAGE));
    if space.free >= needed {
        return None;
    }
    Some(format!(
        "\x1b[1;33mwarning\x1b[0m {} free where a build of this image wants about {}.\n\
         \x20         {}\n\
         \x20         `kiln clean` frees old generations and trims the artifact cache.",
        disk::human(space.free),
        disk::human(needed),
        match previous {
            Some(size) => format!("The last generation is {}.", disk::human(size)),
            None => "No previous generation to measure, so this is an estimate.".to_string(),
        }
    ))
}

/// The on-disk size of the newest deployment, or `None` when there is not one
/// to measure — a first build, or a sysroot this Kiln cannot open.
fn previous_image_size(ctx: &Context) -> Option<u64> {
    let sysroot = Sysroot::open(&ctx.sysroot).ok()?;
    let newest = sysroot.generations().ok()?.first()?.number;
    let root = sysroot.deployment_root(newest).ok()?;
    Some(kiln_image::tree::tree_size(&root))
}

fn describe(report: &assemble::Report, verbose: bool) {
    println!("  {} packages", report.installed.len());
    if !report.files.placed.is_empty() {
        println!("  {} files", report.files.placed.len());
    }
    if !report.units.enabled.is_empty() || !report.units.masked.is_empty() {
        println!(
            "  {} units enabled, {} masked",
            report.units.enabled.len(),
            report.units.masked.len()
        );
    }
    if let Some(kernel) = &report.kernel {
        println!("  kernel {}", kernel.version);
    }

    // The overlay upper layer *is* the changeset, so this costs
    // nothing to know — and a script is the one input whose effect Kiln cannot
    // predict, which makes it the one worth showing.
    for ran in &report.scripts.ran {
        let n = ran.changeset.wrote.len();
        let deleted = ran.changeset.deleted.len();
        print!("  script {:<24} wrote {n} path{}", ran.name, plural(n));
        if deleted > 0 {
            print!(", removed {deleted}");
        }
        println!();
        if verbose {
            for written in &ran.changeset.wrote {
                println!("    {:<52}{}", written.path, size(written.bytes));
            }
            for path in &ran.changeset.deleted {
                println!("    {path} (removed)");
            }
        }
    }

    // drift is a warning, not an error — by the time it is visible the
    // tree is built, and refusing to finish leaves nothing to act on.
    for drift in &report.uid_drift {
        eprintln!("\x1b[1;33mwarning\x1b[0m {}", drift.describe());
    }
    for warning in &report.units.warnings {
        eprintln!("\x1b[1;33mwarning\x1b[0m {warning}");
    }
    // A script that wrote nothing, or wrote over a package's file. Both are
    // warnings rather than notes: this says so for the first, and the second
    // is the whole of "scripts cannot *silently* clobber package content".
    for note in &report.scripts.notes {
        eprintln!("\x1b[1;33mwarning\x1b[0m {note}");
    }
    for note in &report.files.notes {
        if verbose {
            println!("  note: {note}");
        }
    }
    if verbose {
        for call in &report.shimmed {
            println!("  shimmed: {call}");
        }
        for hook in &report.hooks_shadowed {
            println!("  shadowed hook: {hook}");
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Sizes are for the eye, not for arithmetic: a locale archive is the reason
/// this prints one at all, and `19293696` does not read as
/// "that is most of what this script did".
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn fail(e: kiln_ostree::Error) -> ExitCode {
    eprintln!("\x1b[1;31merror\x1b[0m {e}");
    ExitCode::System
}

pub fn is_root() -> bool {
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
