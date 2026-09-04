//! `kiln rebuild <gen>`.
//!
//! *"`kiln rebuild <gen>` reconstructs a past generation from its own record —
//! the recorded snapshot date drives the Archive mirrors, and the recorded
//! checksums and AUR commits pin everything else. That is the reproducibility
//! story, and it costs the user no ceremony up front."*
//!
//! The configuration on disk is not read, and neither is `/etc/kiln`. Both
//! inputs come out of the commit: the **manifest** it was built from and the
//! **record** of what that resolved to (step 11). That is what makes
//! this work on a machine whose configuration has since been edited or deleted
//! — which calls the normal case for the questions people actually ask.
//!
//! What comes back is not necessarily identical, and the command's job is to
//! say so precisely rather than to claim success:
//!
//! - **A different `plan_id`** means the inputs no longer resolve to what they
//!   did. Almost always a package the Archive no longer serves at that version.
//! - **A different changeset for a build script** means that script is not a
//!   pure function of its inputs. This is the one audit that
//!   finds the single unreproducible script in a configuration, instead of
//!   leaving you to wonder why two builds differ.
//! - **A different commit checksum** with everything else equal means the tree
//!   differs for a reason nothing above explains, which is a bug in Kiln rather
//!   than in the configuration.

use crate::build;
use crate::paths;
use crate::pipeline::Context;
use kiln_diag::{render_all, ExitCode};
use kiln_manifest::{Manifest, Snapshot};
use kiln_ostree::commit;
use kiln_record::Record;
use kiln_resolve::BuildPlan;

pub fn run(ctx: &Context, generation: u64) -> ExitCode {
    if !build::is_root() {
        eprintln!(
            "\x1b[1;31merror\x1b[0m rebuilding an image needs root, for the same reason \
             building one does."
        );
        return ExitCode::System;
    }

    let repo = match commit::open_or_create(&paths::repo(&ctx.sysroot)) {
        Ok(repo) => repo,
        Err(e) => return fail(&e),
    };
    let (checksum, metadata) = match commit::find_generation(&repo, generation) {
        Ok(found) => found,
        Err(e) => return fail(&e),
    };
    drop(repo);

    // Both halves, or nothing. A generation built by a Kiln that wrote no
    // manifest into its commit cannot be rebuilt, and saying that plainly is
    // better than rebuilding the packages and silently dropping every
    // `[[file]]`, unit state and build script the image also had.
    let (Some(record), Some(manifest)) = (metadata.record, metadata.manifest) else {
        eprintln!(
            "\x1b[1;31merror\x1b[0m generation {generation} ({}) does not carry both the build \
             record and the manifest it was built from, so it cannot be reconstructed.\n\n\
             It was built by a Kiln that wrote only one of the two. Generations built from \
             here on carry both (step 11).",
            &checksum[..12]
        );
        return ExitCode::System;
    };

    println!(
        "Rebuilding generation {generation} ({}) from its own record.",
        &checksum[..12]
    );
    println!("  snapshot {}", record.repos.snapshot);
    println!("  built    {}", record.built_at);

    let pinned = pin_to_snapshot(&manifest, &record);
    let plan = match resolve(&pinned, ctx, &record) {
        Ok(plan) => plan,
        Err(code) => return code,
    };

    // Reported before building rather than after, because a plan that already
    // differs tells you the rebuild will differ and why — and the build takes
    // minutes to reach the same conclusion less clearly.
    report_inputs(&record, &plan);

    // `kiln rebuild` reconstructs a past generation, and a build that kept its failed
    // roots around is a debugging aid a rebuild never asked for.
    let (committed, rebuilt) = match build::build(&pinned, ctx, &plan, false) {
        Ok(built) => built,
        Err(code) => return code,
    };

    report_result(ctx, &record, &rebuilt, &plan, &committed, &checksum)
}

/// *the recorded snapshot date drives the Archive mirrors*.
///
/// The recorded date replaces whatever the manifest said, including
/// `Snapshot::Latest` — a manifest that tracked live mirrors in June still
/// records the date it resolved on, and that single field is what makes a past
/// image reconstructible without anyone having pinned anything in advance.
/// Resolving against today's mirrors instead would rebuild the
/// configuration, not the generation.
fn pin_to_snapshot(manifest: &Manifest, record: &Record) -> Manifest {
    let mut pinned = manifest.clone();
    pinned.repos.snapshot = Snapshot::Date(record.repos.snapshot.clone());
    pinned
}

/// Resolve the pinned manifest, seeded with the ids the original generation
/// actually seeded from.
///
/// `uid_seed`, not `uid_map`: the seed is what went into the original's
/// `plan_id`, and replaying the *captured* map instead would produce a
/// different plan than the one being reconstructed for a reason that has
/// nothing to do with the mirrors.
fn resolve(manifest: &Manifest, ctx: &Context, record: &Record) -> Result<BuildPlan, ExitCode> {
    let opts = kiln_resolve::Options::new(&ctx.state).with_uid_map(record.seeded_with());
    let network = kiln_aur::Network::default();
    let inputs = kiln_resolve::Inputs::new(&network);

    // The config root is still needed, and this is the one place a rebuild
    // touches the configuration tree: a `[[file]]` with a `source`, or a script
    // with one, has its bytes on disk and nowhere else. The record pins their
    // digests, so `report_inputs` says so when they have changed — but Kiln
    // cannot reconstruct bytes it never stored.
    kiln_resolve::resolve(manifest, &ctx.config_root, &opts, &inputs).map_err(|errs| {
        eprint!("{}", render_all(&errs));
        eprintln!(
            "\n\x1b[1;33mnote\x1b[0m   this resolved against the Archive mirrors for {}, not \
             today's.\n        A package the Archive no longer serves at that version fails \
             here.",
            record.repos.snapshot
        );
        ExitCode::Resolution
    })
}

/// What the rebuild resolved to, against what the generation recorded.
fn report_inputs(record: &Record, plan: &BuildPlan) {
    let report = crate::check::diff(record, plan);
    if report.is_empty() {
        println!(
            "\nEvery input resolved to what generation {} recorded.",
            record.generation
        );
        return;
    }
    println!("\n\x1b[1;33mwarning\x1b[0m the inputs no longer resolve to what was recorded:\n");
    print!("{}", report.render());
    println!(
        "The rebuild goes ahead with what the Archive serves today. What comes out is a new\n\
         generation that is *close to* generation {}, not a copy of it.",
        record.generation
    );
}

/// Non-determinism is the reason a rebuild is worth doing even when it
/// succeeds: a script that produced a different changeset from identical
/// inputs is the one script in the configuration that is not reproducible, and
/// nothing else in Kiln can find it.
///
/// Exits **3** — build failure — when the rebuild did not reproduce, even
/// though a commit was written. has no code for "succeeded but the audit
/// found something", and 3 is the honest one of the codes that exist: the
/// command's job is to reproduce a generation, and it did not. It is
/// deliberately not 10, which reserves for `kiln check` finding changes —
/// a `kiln rebuild` in a CI job should fail the job, not be read as "an update
/// is available".
fn report_result(
    ctx: &Context,
    record: &Record,
    rebuilt: &Record,
    plan: &BuildPlan,
    committed: &kiln_ostree::commit::Committed,
    original: &str,
) -> ExitCode {
    println!(
        "\nGeneration {} committed as {}.",
        committed.generation,
        &committed.checksum[..12]
    );

    let now = &rebuilt.script_effects;
    let unreproducible: Vec<&String> = record
        .script_effects
        .keys()
        .filter(|name| {
            now.get(*name)
                .is_some_and(|d| d != &record.script_effects[*name])
        })
        .collect();

    if !unreproducible.is_empty() {
        println!("\n\x1b[1;33mwarning\x1b[0m these build scripts are not reproducible:\n");
        for name in &unreproducible {
            println!("  script {name}");
            println!("    was {}", short(&record.script_effects[*name]));
            println!("    now {}", short(&now[*name]));
        }
        println!(
            "\nEach one produced a different changeset from the same text over the same tree,\n\
             so its output depends on something Kiln does not hash — the clock, a random\n\
             value, or the order a directory was read in."
        );
        return ExitCode::Build;
    }

    if record.plan_id != plan.plan_id().to_string() {
        println!(
            "\nThe plan differs from generation {}'s, so the commit legitimately differs too.",
            record.generation
        );
        return ExitCode::Ok;
    }
    // The *content* checksum, never the commit checksum. A rebuild of
    // generation 4 is parented on generation 9, so its commit checksum differs
    // from the original's however identical the tree is — comparing those would
    // report every rebuild in existence as a determinism failure.
    let was = match original_content(ctx, original) {
        Ok(was) => was,
        Err(e) => return fail(&e),
    };
    if committed.content_checksum == was {
        println!(
            "\nThe tree is identical to generation {}'s. OSTree stored no new file objects\n\
             for it, so the rebuild cost no disk.",
            record.generation
        );
        return ExitCode::Ok;
    }

    // Same plan, same scripts, different tree. Nothing the user did explains
    // this, so it is Kiln's problem and the message says so rather than
    // leaving them to conclude their configuration is at fault.
    println!(
        "\n\x1b[1;33mwarning\x1b[0m the plan is identical to generation {}'s and every build \
         script agreed,\n        but the tree came out different ({} against {}).\n\n\
         That is a determinism bug in Kiln itself, not in your configuration.",
        record.generation,
        &committed.content_checksum[..12],
        &was[..12]
    );
    ExitCode::Build
}

fn original_content(ctx: &Context, checksum: &str) -> Result<String, kiln_ostree::Error> {
    let repo = commit::open_or_create(&paths::repo(&ctx.sysroot))?;
    commit::content_checksum(&repo, checksum)
}

fn short(hash: &str) -> String {
    let hex = hash.strip_prefix("b3:").unwrap_or(hash);
    format!("b3:{}", &hex[..hex.len().min(12)])
}

fn fail(e: &kiln_ostree::Error) -> ExitCode {
    eprintln!("\x1b[1;31merror\x1b[0m {e}");
    ExitCode::System
}
