//! Frontend → resolution → realization → assembly → commit → deploy.
//!
//!
//! `check`, `build` and `apply` are the same pipeline stopped at three
//! different points, and they are written that way here so they cannot drift.

use crate::paths;
use kiln_diag::{render_all, ExitCode};
use kiln_ostree::{commit, Sysroot};
use kiln_record::Record;
use kiln_resolve::BuildPlan;
use std::path::{Path, PathBuf};

pub struct Context {
    pub sysroot: PathBuf,
    pub state: PathBuf,
    pub config_root: PathBuf,
    pub verbose: bool,
}

/// Resolve the configuration into a plan.
///
/// The UID seed comes from the deployed generation's record. A machine
/// with no commits yet has nothing to replay, which is the first build and not
/// an error.
pub fn plan(
    fe: &kiln_config::Frontend,
    ctx: &Context,
    offline: bool,
) -> Result<BuildPlan, ExitCode> {
    let seed = deployed_record(ctx)
        .map(|r| r.next_seed())
        .unwrap_or_default();

    let opts = kiln_resolve::Options::new(&ctx.state)
        .offline(offline)
        .with_uid_map(seed);
    let network = kiln_aur::Network::default();
    let inputs = kiln_resolve::Inputs::new(&network);

    kiln_resolve::resolve(&fe.manifest, &ctx.config_root, &opts, &inputs).map_err(|errs| {
        eprint!("{}", render_all(&errs));
        ExitCode::Resolution
    })
}

/// The record of the generation at the head of `kiln/<image>/<arch>`, if there
/// is one. Change detection compares against what is deployed.
pub fn deployed_record(ctx: &Context) -> Option<Record> {
    let sysroot = Sysroot::open(&ctx.sysroot).ok()?;
    let generations = sysroot.generations().ok()?;
    let current = generations
        .iter()
        .find(|g| g.booted)
        .or(generations.first())?;
    let metadata = commit::read_metadata(&sysroot.repo(), &current.checksum).ok()?;
    metadata.record
}

/// Is this plan already what is deployed?
pub fn is_no_op(ctx: &Context, plan: &BuildPlan) -> bool {
    deployed_record(ctx).is_some_and(|r| r.plan_id() == plan.plan_id())
}

/// Volatile inputs are reported separately and never guessed: an
/// untrustworthy `kiln check` is worse than no `kiln check`.
pub fn report_volatile(plan: &BuildPlan) {
    if plan.volatile.is_empty() {
        return;
    }
    println!(
        "\n{} input{} could not be checked without fetching:",
        plan.volatile.len(),
        if plan.volatile.len() == 1 { "" } else { "s" }
    );
    for v in &plan.volatile {
        println!("  {}   {}", v.input, v.reason);
    }
    println!("\n  `kiln check --deep` fetches them and answers precisely.");
}

/// Everything in `/var/lib/kiln` is cache and history, so a build
/// directory left behind by a failed run costs disk and nothing else — but a
/// *reused* one would be a build that inherited state, and step 1 says
/// assembly builds from nothing.
pub fn fresh_build_dir(state: &Path, plan_id: &str) -> std::io::Result<PathBuf> {
    let dir = paths::build_dir(state, plan_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
