//! `kiln list`, `status`, `deploy`, `rollback`, `pin`, `rm`, `clean`, and
//! `sysroot init`.
//!
//! Every one of these takes a **generation**, never an OSTree deployment index.
//! Indices renumber as deployments come and go — today's 1 is tomorrow's 0 —
//! which makes `kiln rm 1` a footgun. Generations are assigned at commit
//! time and are stable forever.

use crate::{disk, paths};
use kiln_diag::ExitCode;
use kiln_image::bootcount;
use kiln_ostree::{deploy, grubenv, Counting, Generation, Removal, Sysroot};
use std::path::Path;

pub fn list(sysroot: Option<&Path>) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let generations = match sysroot.generations() {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };
    if generations.is_empty() {
        println!("No Kiln deployments on {}.", root.display());
        println!("`kiln apply` builds one.");
        return ExitCode::Ok;
    }
    print!("{}", render(&generations));
    ExitCode::Ok
}

/// The status table. The status column carries the three facts a person acts on:
/// which one they are running, which one `kiln rollback` would take them to,
/// and which ones `kiln clean` will not remove.
pub fn render(generations: &[Generation]) -> String {
    let rows: Vec<(&Generation, String)> = generations
        .iter()
        .map(|g| (g, status_of(g).join(", ")))
        .collect();

    // Measured rather than fixed. A deployment can be booted, the rollback
    // target and the baseline at once, and a fixed column narrower than that
    // pushes every later column out of line on exactly the machine where the
    // listing matters most.
    let width = rows
        .iter()
        .map(|(_, s)| display_width(s))
        .chain(std::iter::once("STATUS".len()))
        .max()
        .unwrap_or(6);

    let mut out = format!(
        "{:>4}  {:<width$} {:<14} {:<19} {}\n",
        "GEN", "STATUS", "COMMIT", "GENERATED", "IMAGE"
    );
    for (g, status) in rows {
        out.push_str(&format!(
            "{:>4}  {}{} {:<14} {:<19} {}\n",
            g.number,
            status,
            " ".repeat(width - display_width(&status)),
            &g.checksum[..12.min(g.checksum.len())],
            g.built_at,
            g.image
        ));
    }
    out
}

/// The status column carries the facts a person acts on: which one they are
/// running, which one `kiln rollback` would take them to, and which ones
/// `kiln clean` will not remove.
fn status_of(g: &Generation) -> Vec<&'static str> {
    let mut status = Vec::new();
    if g.booted {
        status.push("● booted");
    } else if g.boots_next {
        // The generation `kiln apply` just staged. Without it the listing says
        // nothing whatever about the row the user is waiting on, while marking
        // the *running* one "rollback target" — both true, and together they
        // read as though the new generation had not been deployed at all.
        //
        // `else if`, because in the steady state the booted deployment is also
        // the one at the front, and saying so there is noise on every row that
        // matters least.
        status.push("boots next");
    }
    if g.rollback_target {
        status.push("rollback target");
    }
    // Before `pinned`, and instead of it: the baseline is pinned by
    // Kiln rather than by the user, and a listing that said "pinned" about
    // generation 1 would invite a `kiln unpin 1` that does not mean what the
    // person typing it thinks it means.
    if g.baseline {
        status.push("baseline");
    } else if g.pinned {
        status.push("pinned");
    }
    status
}

/// Columns are laid out in characters, and `●` is one character in three bytes.
/// `str::len` would over-pad every booted row by two.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

pub fn status(sysroot: Option<&Path>, verbose: bool) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let generations = match sysroot.generations() {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };

    // The booted one when there is one, otherwise the one that boots next —
    // which is the honest answer when `kiln status` is run under `--sysroot`
    // against a machine that is not this one.
    let subject = generations
        .iter()
        .find(|g| g.booted)
        .or_else(|| generations.first());
    let Some(g) = subject else {
        println!("No Kiln deployments on {}.", root.display());
        return ExitCode::Ok;
    };

    println!("generation  {}", g.number);
    println!("image       {}", g.image);
    println!("built       {}", g.built_at);
    // never print a checksum where a generation number would do. It is
    // the identity `kiln show` and a bug report need, and noise everywhere else.
    if verbose {
        println!("commit      {}", g.checksum);
    }
    println!(
        "state       {}",
        if g.booted { "booted" } else { "next boot" }
    );
    // The "pending update": something is deployed that the machine is not
    // running. Worth its own line because the deployment list does not say it
    // — the pending generation is simply first, which looks exactly like the
    // ordinary case where the booted one is.
    if let Some(next) = generations.first().filter(|n| n.number != g.number) {
        println!(
            "pending     generation {} boots next — reboot to use it",
            next.number
        );
    }
    if let Some(target) = generations.iter().find(|x| x.rollback_target) {
        println!("rollback    generation {}", target.number);
    }
    if let Some(line) = boot_counting(&root, &generations) {
        println!("{line}");
    }
    print!("{}", etc_drift(&sysroot, g, verbose));
    if verbose {
        print!("\n{}", render(&generations));
    }
    ExitCode::Ok
}

/// What the live `/etc` has that the generation did not ship, and what
/// that means — reported against the generation being described, because it is
/// that generation's `/usr/etc` the merge will diff at the next deploy.
///
/// Every failure here degrades to silence rather than to an error. A drift
/// report is an extra a status command offers; a `kiln status` that refuses to
/// tell you what is booted because it could not stat a file in `/etc` would be
/// a worse command than one that never had the feature.
fn etc_drift(sysroot: &Sysroot, g: &Generation, verbose: bool) -> String {
    let Ok(deployment) = sysroot.deployment_root(g.number) else {
        return String::new();
    };
    let manifest = kiln_ostree::commit::read_metadata(&sysroot.repo(), &g.checksum)
        .ok()
        .and_then(|m| m.manifest);
    crate::drift::report(&deployment, manifest.as_ref(), verbose).unwrap_or_default()
}

/// Report the outcome of boot counting in `kiln status`.
///
/// The interesting case is a demotion, and it is only reportable *because* the
/// counter is a file rather than a filename: after GRUB has selected the
/// rollback entry there is nothing about the deployment list that says why, and
/// "you are running an older generation than the one you applied" is not a
/// thing a user should have to notice for themselves.
pub fn boot_counting(root: &Path, generations: &[Generation]) -> Option<String> {
    match grubenv::counting(root, bootcount::TRIES) {
        Counting::Off => None,
        // `saturating_sub`, because the counter on disk was armed by whichever
        // Kiln staged the deployment: a machine that upgraded Kiln between the
        // deploy and the boot can legitimately hold a `left` larger than this
        // build's `TRIES`, and a status command must not panic over it.
        Counting::Armed { left, tries } => Some(format!(
            "boot        attempt {} of {} — this generation has not been marked good yet",
            tries.saturating_sub(left),
            tries
        )),
        Counting::Exhausted { tries } => {
            // The deployment list is in boot order, so the generation that ran
            // out of attempts is the one at the front — and it is not the one
            // running, or it would not have been demoted.
            let failed = generations.first().filter(|g| !g.booted)?;
            let running = generations.iter().find(|g| g.booted)?;
            Some(format!(
                "boot        generation {} failed to boot {tries} times and was demoted;\n\
                 \x20           you are running generation {}. `kiln deploy {}` tries it again.",
                failed.number, running.number, failed.number
            ))
        }
    }
}

/// No argument: "the previous one" is what a person means at 2am, and
/// `kiln deploy <gen>` already exists for when they mean something specific.
pub fn rollback(sysroot: Option<&Path>) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(code) => return code,
    };
    match deploy::rollback(&sysroot) {
        Ok(g) => {
            println!("generation {} will boot next.", g.number);
            println!("Reboot to use it; `kiln rollback` again to go back.");
            ExitCode::Ok
        }
        Err(e) => fail(&e),
    }
}

/// `kiln deploy <gen>` — make a generation the one that boots next.
///
/// Two jobs behind one verb, and the second is the whole of the installer's "make it
/// bootable" row. A generation that is already deployed is *reordered*; one that
/// has only ever been **committed** — which is everything `kiln build` produces,
/// since building is not deploying — is deployed here and now.
///
/// Only the first existed at first, so an installer following the table got
/// `there is no generation 1; this machine has no Kiln deployments yet` about a
/// generation it had just watched being committed. The message was true and
/// answered a question nobody asked: `set_default` looks at the deployment
/// list, and the thing being named was in the repository.
pub fn set_default(sysroot: Option<&Path>, generation: u64) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let generations = match sysroot.generations() {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };

    if generations.iter().any(|g| g.number == generation) {
        return match sysroot.set_default(generation) {
            Ok(g) => {
                println!("generation {} will boot next.", g.number);
                ExitCode::Ok
            }
            Err(e) => fail(&e),
        };
    }
    deploy_committed(&sysroot, generation)
}

/// Deploy a generation that exists as a commit and has never been deployed.
///
/// The manifest comes out of the commit's own metadata, not off disk.
/// That is not a convenience: kargs are **fully declarative**, so
/// deploying with the wrong set — or none — produces a machine that boots once
/// or not at all, and the only set that is right is the one this generation was
/// built from. A configuration that has since been edited is not it.
fn deploy_committed(sysroot: &Sysroot, generation: u64) -> ExitCode {
    let (checksum, metadata) =
        match kiln_ostree::commit::find_generation(&sysroot.repo(), generation) {
            Ok(found) => found,
            Err(e) => {
                let code = fail(&e);
                // The wrong assumption behind this error is usually that
                // `build` deploys. It does not, and on a fresh target
                // there is simply nothing committed to deploy yet.
                eprintln!(
                    "\n`kiln build --sysroot {}` commits one; `kiln apply` commits and \
                     deploys\nit in a single step.",
                    sysroot.path().display()
                );
                return code;
            }
        };

    let Some(manifest) = metadata.manifest else {
        eprintln!(
            "\x1b[1;31merror\x1b[0m generation {generation} carries no manifest, so there is no \
             way to know which\n        kernel command line it was built with. Kargs are fully \
             declarative: deploying\n        without them produces a machine that boots \
             once, or not at all.\n\n\
             It was built by a Kiln that did not record one. `kiln apply` builds and deploys a \
             new\n        generation from your configuration."
        );
        return ExitCode::System;
    };

    match sysroot.deploy(&checksum, &manifest, &metadata.image, bootcount::TRIES) {
        Ok(deployed) => {
            println!(
                "Generation {} deployed; it will boot next.",
                deployed.generation
            );
            if deployed.baseline {
                println!(
                    "Pinned as the baseline: `kiln clean` keeps it without `--remove-baseline` \
."
                );
            }
            if deployed.backend == kiln_ostree::Backend::None && sysroot.path() != Path::new("/") {
                // Expected under `--sysroot`, and worth saying once: an
                // installer that does not know this looks for a bug in Kiln
                // instead of running the `grub-install` it already owns.
                println!(
                    "\nBLS entries were written and no /boot/grub/grub.cfg: libostree's grub2 \
                     backend\ncannot run against a sysroot that is not `/`. Install the \
                     bootloader onto\nthe disk — `grub-install` — and it will generate the \
                     config from the deployed tree."
                );
            }
            ExitCode::Ok
        }
        Err(e) => fail(&e),
    }
}

pub fn pin(sysroot: Option<&Path>, generation: u64, pinned: bool) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(code) => return code,
    };
    match sysroot.set_pinned(generation, pinned) {
        Ok(()) => {
            let verb = if pinned { "pinned" } else { "unpinned" };
            println!("generation {generation} {verb}.");
            ExitCode::Ok
        }
        Err(e) => fail(&e),
    }
}

/// `kiln clean [--keep N] [--dry-run] [--remove-baseline]`.
///
/// Two budgets, not one: the deployments, and the artifact cache. They are the
/// same command because they are the same question — "this machine has one
/// disk, give some of it back" — and a `clean` that freed three generations and
/// left 20 GiB of packages would be answering half of it.
pub fn clean(
    sysroot: Option<&Path>,
    keep: usize,
    dry_run: bool,
    remove_baseline: bool,
) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let generations = match sysroot.generations() {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };

    let plan = Removal::budget(&generations, keep, remove_baseline);
    report_removal(&plan, keep);

    if !dry_run && !plan.is_empty() {
        if let Err(e) = sysroot.remove(&plan.remove) {
            return fail(&e);
        }
    } else if !dry_run {
        // Nothing to undeploy, but libostree's own prune still has objects to
        // drop from a generation removed earlier or a build that was
        // interrupted between commit and deploy.
        if let Err(e) = sysroot.cleanup() {
            return fail(&e);
        }
    }

    trim_cache(&paths::state(&root), dry_run);
    if dry_run {
        println!("\nNothing was removed. Run without `--dry-run` to do it.");
    }
    ExitCode::Ok
}

/// `kiln rm <gen>...`.
pub fn rm(sysroot: Option<&Path>, wanted: &[u64], remove_baseline: bool) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let generations = match sysroot.generations() {
        Ok(g) => g,
        Err(e) => return fail(&e),
    };

    // Named but not there at all. Distinct from "named and refused", and worth
    // its own message: a typo and a protected generation are different
    // mistakes, and the explanation of what a generation is belongs to the
    // first one.
    let unknown: Vec<u64> = wanted
        .iter()
        .filter(|w| !generations.iter().any(|g| g.number == **w))
        .copied()
        .collect();
    if !unknown.is_empty() {
        let have: Vec<String> = generations.iter().map(|g| g.number.to_string()).collect();
        eprintln!(
            "\x1b[1;31merror\x1b[0m no deployment for generation {}; this machine has {}",
            numbers(&unknown),
            if have.is_empty() {
                "none".to_string()
            } else {
                have.join(", ")
            }
        );
        return ExitCode::System;
    }

    let plan = Removal::requested(&generations, wanted, remove_baseline);
    report_removal(&plan, 0);
    if plan.is_empty() {
        // Everything asked for was protected. That is a refusal, not a
        // success: a script that runs `kiln rm 1 && …` should not proceed.
        return ExitCode::System;
    }
    match sysroot.remove(&plan.remove) {
        Ok(()) => ExitCode::Ok,
        Err(e) => fail(&e),
    }
}

/// What a removal would do, in the same words for `rm` and `clean`.
fn report_removal(plan: &Removal, keep: usize) {
    if plan.remove.is_empty() && plan.refused.is_empty() {
        println!("Nothing to remove; every generation is inside the budget.");
        return;
    }
    if !plan.remove.is_empty() {
        println!("Removing generation {}.", numbers(&plan.remove));
    } else if keep > 0 {
        println!("Nothing to remove; every generation is protected.");
    }
    for (number, why) in &plan.refused {
        println!("  keeping generation {number}: {why}");
    }
}

/// The second rule, applied to `/var/lib/kiln/cache/pkg`.
fn trim_cache(state: &Path, dry_run: bool) {
    let dir = paths::cache(state);
    let Some(space) = disk::space(&dir) else {
        return;
    };
    let budget = disk::cache_budget(space.total);
    let cached = disk::cached(&dir);
    let held: u64 = cached.iter().map(|c| c.bytes).sum();
    let evicted = disk::evict(&cached, budget);
    if evicted.is_empty() {
        if held > 0 {
            println!(
                "Artifact cache {} of {} budget.",
                disk::human(held),
                disk::human(budget)
            );
        }
        return;
    }
    let freed: u64 = evicted.iter().map(|c| c.bytes).sum();
    println!(
        "Artifact cache {} over its {} budget: dropping {} oldest package{}, {}.",
        disk::human(held.saturating_sub(budget)),
        disk::human(budget),
        evicted.len(),
        if evicted.len() == 1 { "" } else { "s" },
        disk::human(freed)
    );
    if dry_run {
        return;
    }
    for entry in evicted {
        // A file that will not delete is not a reason to fail the command:
        // everything under /var/lib/kiln is cache, and the next build
        // simply pays to fetch it again.
        std::fs::remove_file(&entry.path).ok();
    }
}

/// `38, 39` — the generations a message names.
fn numbers(of: &[u64]) -> String {
    of.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
}

/// Kiln does not install anything; this exists so a separate installer
/// can be written against it.
pub fn sysroot_init(sysroot: Option<&Path>) -> ExitCode {
    let root = paths::sysroot(sysroot);
    match Sysroot::init(&root) {
        Ok(_) => {
            println!("Initialized an OSTree sysroot at {}.", root.display());
            // these settings are the reason `sysroot init` exists as a
            // command rather than as three lines in an installer's script.
            println!("  stateroot   {}", kiln_ostree::deploy::STATEROOT);
            // Not "grub2": the backend is decided per deploy, from the commit
            // and from whether the sysroot is `/`. Saying grub2
            // here would promise an installer something it is not going to get.
            println!("  bootloader  BLS entries; grub2 once deploying to /");
            println!(
                "  boot        {} attempts before automatic rollback",
                bootcount::TRIES
            );
            println!(
                "\n`kiln build --sysroot {}` can now commit into it.",
                root.display()
            );
            ExitCode::Ok
        }
        Err(e) => fail(&e),
    }
}

/// Open a sysroot, and say something useful when it is not one.
///
/// The hint used to be gated on the *repository* being absent, which suppressed
/// it in the one case that most needs it. `kiln build --sysroot X` creates
/// `ostree/repo` by itself, so a target built into but never initialized has a
/// repository — and the user got libostree's bare
/// `fstatat(ostree/deploy): No such file or directory` and nothing to act on.
/// That is not a hypothetical; it is what an installer following the steps out of
/// order actually hits.
fn open(root: &Path) -> Result<Sysroot, ExitCode> {
    Sysroot::open(root).map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m {e}");
        if !paths::is_initialized(root) {
            eprintln!(
                "\n`kiln sysroot init --sysroot {}` creates the layout this needs \
                .",
                root.display()
            );
            // The commits are fine — they are in the repository and keep their
            // generation numbers. Saying so is the difference between "run one
            // command" and "start the install again".
            if paths::repo(root).exists() {
                eprintln!(
                    "\n{} has a Kiln repository but no stateroot: it was built into before it \
                     was\ninitialized. Nothing is lost — initialize it and the generations \
                     already committed\nthere are deployable, `kiln list` will show them.",
                    root.display()
                );
            }
        }
        ExitCode::System
    })
}

fn fail(e: &kiln_ostree::Error) -> ExitCode {
    eprintln!("\x1b[1;31merror\x1b[0m {e}");
    ExitCode::System
}
