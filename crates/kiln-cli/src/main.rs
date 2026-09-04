//! `kiln`.
//!
//! The commands that a phase cannot serve yet are present and say so plainly
//! rather than being absent — "unknown command" would be a worse answer to a
//! question the design has already answered.

mod args;
mod build;
mod check;
mod deep;
mod deployments;
mod disk;
mod drift;
mod explain;
mod init;
mod inspect;
mod paths;
mod pipeline;
mod realize;
mod rebuild;
mod show;

use args::Command;
use kiln_diag::{render_all, ExitCode};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&argv);
    std::process::exit(code.code());
}

fn run(argv: &[String]) -> ExitCode {
    let cli = match args::parse(argv) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("\x1b[1;31merror\x1b[0m {msg}");
            return ExitCode::Config;
        }
    };

    match &cli.command {
        Command::Help => {
            print!("{}", args::help());
            ExitCode::Ok
        }
        Command::Version => {
            println!(
                "kiln {} (schema {}, hash epoch {})",
                env!("CARGO_PKG_VERSION"),
                kiln_manifest::SCHEMA_VERSION,
                kiln_manifest::HASH_EPOCH
            );
            ExitCode::Ok
        }
        Command::Init => init::run(cli.global.config.as_deref()),

        Command::List => deployments::list(cli.global.sysroot.as_deref()),
        Command::Status => deployments::status(cli.global.sysroot.as_deref(), cli.global.verbose),
        Command::Rollback => deployments::rollback(cli.global.sysroot.as_deref()),
        Command::Deploy { generation } => {
            deployments::set_default(cli.global.sysroot.as_deref(), *generation)
        }
        Command::Pin { generation, pinned } => {
            deployments::pin(cli.global.sysroot.as_deref(), *generation, *pinned)
        }
        Command::Clean {
            keep,
            dry_run,
            remove_baseline,
        } => deployments::clean(
            cli.global.sysroot.as_deref(),
            *keep,
            *dry_run,
            *remove_baseline,
        ),
        Command::Rm {
            generations,
            remove_baseline,
        } => deployments::rm(cli.global.sysroot.as_deref(), generations, *remove_baseline),

        // a rebuild reads the *commit*, not `/etc/kiln`. It does not go
        // through `frontend()` on purpose — a configuration that has since been
        // edited into an invalid state, or deleted, is the normal case for the
        // questions a rebuild answers, and refusing to start because today's
        // TOML does not parse would defeat the whole command.
        Command::Rebuild { generation } => {
            match kiln_config::discover::entry_point(cli.global.config.as_deref()) {
                Ok(entry) => {
                    let sysroot = paths::sysroot(cli.global.sysroot.as_deref());
                    rebuild::run(
                        &pipeline::Context {
                            state: paths::state(&sysroot),
                            sysroot,
                            config_root: entry.config_root,
                            verbose: cli.global.verbose,
                        },
                        *generation,
                    )
                }
                // The config root is still needed for one thing: a `[[file]]` or a
                // script with a `source` has its bytes on disk and nowhere else, so
                // a rebuild has to know where to look even though it reads no TOML.
                Err(diag) => {
                    let mut errs = kiln_diag::Errors::new();
                    errs.push(diag);
                    eprint!("{}", render_all(&errs));
                    ExitCode::Config
                }
            }
        }
        // None of the three reads `/etc/kiln`: they answer questions
        // about a *generation*, and the configuration it was built from is very
        // often one that has since been edited or deleted.
        Command::Diff { from, to } => inspect::diff(cli.global.sysroot.as_deref(), *from, *to),
        Command::Why {
            package,
            generation,
        } => inspect::why(cli.global.sysroot.as_deref(), package, *generation),
        Command::Owns { path, generation } => {
            inspect::owns(cli.global.sysroot.as_deref(), path, *generation)
        }

        Command::SysrootInit => deployments::sysroot_init(cli.global.sysroot.as_deref()),

        Command::Build { .. } | Command::Apply { .. } => frontend(&cli),

        // `kiln show <gen>` reads the commit, so it answers about a
        // generation whose configuration is gone. `kiln show` with no argument
        // is a question about the configuration and goes through the frontend.
        Command::Show {
            generation: Some(generation),
        } => inspect::show(
            cli.global.sysroot.as_deref(),
            *generation,
            cli.global.verbose,
        ),

        Command::Check { .. } | Command::Explain { .. } | Command::Show { .. } => frontend(&cli),
    }
}

fn frontend(cli: &args::Cli) -> ExitCode {
    let opts = kiln_config::Options {
        allow_external_sources: cli.global.allow_external_sources,
        module_root: cli.global.module_root.clone(),
    };

    let fe = match kiln_config::load(cli.global.config.as_deref(), &opts) {
        Ok(fe) => fe,
        Err(errs) => {
            eprint!("{}", render_all(&errs));
            return ExitCode::Config;
        }
    };

    if !fe.warnings.is_empty() {
        eprint!("{}", render_all(&fe.warnings));
    }

    let sysroot = paths::sysroot(cli.global.sysroot.as_deref());
    let ctx = pipeline::Context {
        state: paths::state(&sysroot),
        sysroot,
        config_root: fe.config_root.clone(),
        verbose: cli.global.verbose,
    };

    match &cli.command {
        Command::Explain { key } => explain::run(&fe, key.as_deref().unwrap_or_default()),
        Command::Show { .. } => {
            show::summary(&fe.manifest, &fe.files, cli.global.verbose);
            show::detail(&fe.manifest);
            ExitCode::Ok
        }
        Command::Check { offline, deep } => check_command(cli, &fe, &ctx, *offline, *deep),
        Command::Build {
            force,
            offline,
            keep_failed,
        } => build::run(&fe, &ctx, *force, *offline, *keep_failed, false),
        Command::Apply {
            force,
            offline,
            keep_failed,
        } => build::run(&fe, &ctx, *force, *offline, *keep_failed, true),
        _ => unreachable!(),
    }
}

/// `kiln check` answers one question — what would change — and its exit
/// code is the answer: 10 when something would, 0 when nothing would. That is
/// what makes it usable from a timer or a prompt without parsing anything.
fn check_command(
    cli: &args::Cli,
    fe: &kiln_config::Frontend,
    ctx: &pipeline::Context,
    offline: bool,
    deep: bool,
) -> ExitCode {
    show::summary(&fe.manifest, &fe.files, cli.global.verbose);

    if deep && offline {
        eprintln!(
            "\n\x1b[1;31merror\x1b[0m  `--deep` and `--offline` ask for opposite things: \
             `--deep` exists to\n        fetch the inputs that cannot be resolved without \
             fetching."
        );
        return ExitCode::Config;
    }

    let plan = match pipeline::plan(fe, ctx, offline) {
        Ok(plan) => plan,
        Err(code) => return code,
    };

    // Deep resolution happens before the comparison rather than after it: a volatile input that
    // has moved is a change, and the report should say so in the same breath as
    // every other category rather than as a footnote to it.
    let deep_report = deep.then(|| {
        println!(
            "\nResolving {} volatile input{} by fetching…",
            plan.volatile.len(),
            if plan.volatile.len() == 1 { "" } else { "s" }
        );
        let network = kiln_aur::Network::default();
        let report = deep::resolve(&plan, &fe.manifest, ctx, &network);
        if !report.resolved.is_empty() || !report.unresolved.is_empty() {
            print!("{}", report.render());
        }
        report
    });

    let Some(record) = pipeline::deployed_record(ctx) else {
        println!("\nConfiguration is valid, and this machine has no Kiln generation to compare");
        println!("against yet. `kiln apply` builds the first one.");
        println!("  plan {}", plan.plan_id());
        if deep_report.is_none() {
            pipeline::report_volatile(&plan);
        }
        // Nothing is deployed, so nothing *changed*. reserves 10 for
        // "found changes", and reporting changes against nothing would make the
        // code useless in the one place it is read from — a timer.
        return ExitCode::Ok;
    };

    if record.plan_id() == plan.plan_id() {
        println!("\nUp to date.  generation {}", record.generation);
        if deep_report.is_none() {
            pipeline::report_volatile(&plan);
            return ExitCode::Ok;
        }
        // A `--deep` that could not answer must not report "up to date":
        // is explicit that an untrustworthy check is worse than no check, and
        // the user asked for the precise answer.
        return deep_report
            .and_then(|r| deep::exit_code(&r))
            .unwrap_or(ExitCode::Ok);
    }

    let report = check::diff(&record, &plan);
    println!(
        "\nUpdate available.  gen {} → pending  (plan {} → {})",
        record.generation,
        shorten(&record.plan_id),
        shorten(&plan.plan_id().to_string())
    );
    println!();
    print!("{}", report.render());
    if deep_report.is_none() {
        pipeline::report_volatile(&plan);
    }
    println!("\nBuild it with:  kiln apply");
    ExitCode::ChangesFound
}

fn shorten(hash: &str) -> String {
    match hash.strip_prefix("b3:") {
        Some(hex) => format!("b3:{}…", &hex[..8.min(hex.len())]),
        None => hash.to_string(),
    }
}
