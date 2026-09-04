//! Argument parsing. Hand-written: the surface is small and fixed, and a
//! derive macro would not make it clearer.

use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Global {
    pub config: Option<PathBuf>,
    pub sysroot: Option<PathBuf>,
    pub allow_external_sources: bool,
    pub module_root: Option<PathBuf>,
    pub verbose: bool,
}

#[derive(Debug)]
pub enum Command {
    Check {
        offline: bool,
        deep: bool,
    },
    Explain {
        key: Option<String>,
    },
    /// `kiln show` describes the configuration on disk; `kiln show <gen>`
    /// describes a generation, from its own commit.
    Show {
        generation: Option<u64>,
    },
    Init,
    Build {
        force: bool,
        offline: bool,
        /// Keep a failed build's root so it can be walked into.
        keep_failed: bool,
    },
    Apply {
        force: bool,
        offline: bool,
        keep_failed: bool,
    },
    List,
    Status,
    Rollback,
    /// `kiln deploy <gen>` — make a generation the default for the next boot.
    Deploy {
        generation: u64,
    },
    Pin {
        generation: u64,
        pinned: bool,
    },
    Clean {
        /// three generations, plus the baseline, plus anything pinned.
        keep: usize,
        dry_run: bool,
        remove_baseline: bool,
    },
    /// `kiln rm <gen>...` — undeploy generations by number.
    Rm {
        generations: Vec<u64>,
        remove_baseline: bool,
    },
    /// `kiln diff [<gen>] [<gen>]`, default booted vs pending.
    Diff {
        from: Option<u64>,
        to: Option<u64>,
    },
    /// `kiln why <package>` — what pulled it in.
    Why {
        package: String,
        generation: Option<u64>,
    },
    /// `kiln owns <path>` — which package owns a file.
    Owns {
        path: String,
        generation: Option<u64>,
    },
    SysrootInit,
    /// `kiln rebuild <gen>` —, reconstruct a past generation from its own
    /// record rather than from the configuration on disk.
    Rebuild {
        generation: u64,
    },
    Help,
    Version,
}

pub struct Cli {
    pub global: Global,
    pub command: Command,
}

const HELP: &str = "\
kiln — a declarative Linux system image builder

Building
  kiln check [--deep] [--offline]     what would change, without building
  kiln build [--force] [--offline]    build an image
  kiln apply [--force]                build, then stage for next boot
  kiln rebuild <gen>                  rebuild a past generation from its record
      --keep-failed                   on build/apply: keep a failed build root

Inspection
  kiln diff [<gen>] [<gen>]           what changed between two generations
  kiln why <package>                  what pulled a package into the image
  kiln owns <path>                    which package owns a file in the image
  kiln explain <key>                  which file set a config value, and to what
                                      also `<group>` and `<list>/<element>`
  kiln show [<gen>]                   the merged manifest, or a past generation

Deployments (by generation, never by OSTree index)
  kiln list                           every generation on this machine
  kiln status                         what is booted, what boots next, /etc drift
  kiln rollback                       boot the previous generation
  kiln deploy <gen>                   boot a specific generation
  kiln pin <gen> | unpin <gen>        keep a generation through `kiln clean`
  kiln rm <gen>...                    undeploy generations
  kiln clean [--keep N] [--dry-run]   keep N, the baseline, and anything pinned
      --remove-baseline               let `rm` and `clean` take generation 1

Storage
  kiln init                           scaffold /etc/kiln
  kiln sysroot init <path>            create an OSTree sysroot to build into

Global
  --config <path>                 entry point, or a directory containing system.toml
  --sysroot <path>                operate on another root
  --allow-external-sources        permit sources outside the config root
  --module-root <path>            override /usr/share/kiln/modules
  -v, --verbose
";

pub fn parse(argv: &[String]) -> Result<Cli, String> {
    let mut global = Global::default();
    let mut positional: Vec<String> = Vec::new();
    let mut flags: Vec<String> = Vec::new();

    // The default: three generations, plus the baseline, plus anything
    // pinned, plus the running system.
    let mut keep = DEFAULT_KEEP;

    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => {
                global.config = Some(need(&mut it, "--config")?.into());
            }
            "--sysroot" => global.sysroot = Some(need(&mut it, "--sysroot")?.into()),
            "--module-root" => global.module_root = Some(need(&mut it, "--module-root")?.into()),
            "--allow-external-sources" => global.allow_external_sources = true,
            "-v" | "--verbose" => global.verbose = true,
            // The one flag that takes a value and is not global. Handled here
            // rather than after the split because otherwise its argument lands
            // in `positional` and `kiln clean --keep 2` parses as a request to
            // clean generation 2 — which is a different command entirely.
            "--keep" => {
                let value = need(&mut it, "--keep")?;
                keep = value.parse().map_err(|_| {
                    format!("`--keep {value}` is not a number of generations to keep")
                })?;
            }
            s if s.starts_with('-') => flags.push(s.to_string()),
            s => positional.push(s.to_string()),
        }
    }

    let has = |f: &str| flags.iter().any(|x| x == f);
    let verb = positional.first().map(String::as_str).unwrap_or("help");

    let command = match verb {
        "help" | "" => Command::Help,
        "--help" | "-h" => Command::Help,
        "version" | "--version" => Command::Version,
        "check" => Command::Check {
            offline: has("--offline"),
            deep: has("--deep"),
        },
        "explain" => Command::Explain {
            key: positional.get(1).cloned(),
        },
        "show" => Command::Show {
            generation: match positional.get(1) {
                Some(arg) => Some(one_generation(arg)?),
                None => None,
            },
        },
        "init" => Command::Init,

        // `kiln upgrade` is recognized and redirected rather than erroring.
        "upgrade" => {
            return Err(
                "there is no `kiln upgrade`. With rolling builds there is exactly one way to \
                 get a new image: `kiln apply`, whether you edited TOML or want a new kernel."
                    .into(),
            )
        }
        "install" => {
            return Err(
                "there is no `kiln install`. Installation is an installer's job; Kiln exposes \
                 `--sysroot` and `kiln sysroot init` for one to build against."
                    .into(),
            )
        }
        "push" | "pull" | "remote" => {
            return Err(format!(
                "there is no `kiln {verb}`. Kiln is a distribution's build tool, not an \
                 image-shipping pipeline."
            ))
        }

        "build" => Command::Build {
            force: has("--force"),
            offline: has("--offline"),
            keep_failed: has("--keep-failed"),
        },
        "apply" => Command::Apply {
            force: has("--force"),
            offline: has("--offline"),
            keep_failed: has("--keep-failed"),
        },
        "list" => Command::List,
        "status" => Command::Status,
        "rollback" => Command::Rollback,
        "clean" => Command::Clean {
            keep,
            dry_run: has("--dry-run"),
            remove_baseline: has("--remove-baseline"),
        },
        "rm" => Command::Rm {
            generations: generations(&positional, "rm")?,
            remove_baseline: has("--remove-baseline"),
        },

        // `kiln diff [<gen>] [<gen>]`. Both optional, and what the
        // omissions mean is the command's whole ergonomics — see `inspect::diff`.
        "diff" => {
            let mut given = Vec::new();
            for arg in positional.iter().skip(1) {
                given.push(one_generation(arg)?);
            }
            if given.len() > 2 {
                return Err("`kiln diff` compares at most two generations".into());
            }
            match given.len() {
                0 => Command::Diff {
                    from: None,
                    to: None,
                },
                1 => Command::Diff {
                    from: Some(given[0]),
                    to: None,
                },
                _ => Command::Diff {
                    from: Some(given[0]),
                    to: Some(given[1]),
                },
            }
        }
        "why" => Command::Why {
            package: named(&positional, "why", "a package", "kiln why mesa")?,
            generation: optional_generation(&positional)?,
        },
        "owns" => Command::Owns {
            path: named(&positional, "owns", "a path", "kiln owns /usr/bin/ls")?,
            generation: optional_generation(&positional)?,
        },
        "deploy" => Command::Deploy {
            generation: generation(&positional, "deploy")?,
        },
        "pin" => Command::Pin {
            generation: generation(&positional, "pin")?,
            pinned: true,
        },
        "unpin" => Command::Pin {
            generation: generation(&positional, "unpin")?,
            pinned: false,
        },
        // `sysroot init` is the only two-word verb, and is the reason it
        // is not `kiln init` — that one scaffolds a configuration, and
        // conflating "make me a config" with "make me a bootable root" is a
        // mistake somebody makes exactly once.
        "sysroot" => match positional.get(1).map(String::as_str) {
            Some("init") => {
                // The installer's table writes it as `kiln sysroot init /mnt`, and that is the
                // form an installer will type. Taking the path positionally as
                // well as from `--sysroot` costs three lines; not taking it
                // meant `kiln sysroot init /mnt` silently initializing `/`,
                // which is the one machine an installer is certain not to mean.
                if let Some(path) = positional.get(2) {
                    match &global.sysroot {
                        Some(flag) if flag != Path::new(path) => {
                            return Err(format!(
                                "`kiln sysroot init {path}` and `--sysroot {}` name different \
                                 roots; give the target once",
                                flag.display()
                            ))
                        }
                        _ => global.sysroot = Some(path.into()),
                    }
                }
                Command::SysrootInit
            }
            Some(other) => {
                return Err(format!(
                    "unknown `kiln sysroot {other}`; the only subcommand is `init`"
                ))
            }
            None => return Err("`kiln sysroot` needs a subcommand: `kiln sysroot init`".into()),
        },

        "rebuild" => Command::Rebuild {
            generation: generation(&positional, "rebuild")?,
        },

        other => {
            let known = [
                "check", "build", "apply", "rebuild", "explain", "show", "init", "list", "status",
                "rollback", "deploy", "diff", "why", "owns", "pin", "unpin", "rm", "clean",
                "sysroot",
            ];
            let hint = kiln_diag::did_you_mean(other, known)
                .map(|h| format!(" — {h}"))
                .unwrap_or_default();
            return Err(format!(
                "unknown command `{other}`{hint}\n\nrun `kiln help`"
            ));
        }
    };

    if let Command::Explain { key: None } = command {
        return Err("`kiln explain` needs a key, for example `kiln explain boot.timeout`".into());
    }

    Ok(Cli { global, command })
}

/// generations are the only IDs the CLI accepts. An OSTree deployment
/// index would renumber under the user's feet.
fn generation(positional: &[String], verb: &str) -> Result<u64, String> {
    let Some(arg) = positional.get(1) else {
        return Err(format!(
            "`kiln {verb}` needs a generation, for example `kiln {verb} 41`. \
             `kiln list` shows them."
        ));
    };
    one_generation(arg)
}

/// The same explanation, from the one place the message lives. Every command that takes a
/// generation gives the same explanation, because the wrong model behind the
/// mistake — that these are OSTree deployment indices — is the same one.
fn one_generation(arg: &str) -> Result<u64, String> {
    arg.parse().map_err(|_| {
        format!(
            "`{arg}` is not a generation number. Kiln uses the generation from `kiln list`, \
             never an OSTree deployment index — indices renumber as deployments come and go."
        )
    })
}

/// Three, plus the baseline, plus anything pinned, plus what is booted.
pub const DEFAULT_KEEP: usize = 3;

/// One or more generations, for `kiln rm`.
fn generations(positional: &[String], verb: &str) -> Result<Vec<u64>, String> {
    let rest = &positional[1.min(positional.len())..];
    if rest.is_empty() {
        return Err(format!(
            "`kiln {verb}` needs at least one generation, for example `kiln {verb} 38 39`. \
             `kiln list` shows them."
        ));
    }
    rest.iter().map(|a| one_generation(a)).collect()
}

/// The non-numeric argument a command is named for — `kiln why <package>`,
/// `kiln owns <path>`.
fn named(positional: &[String], verb: &str, what: &str, example: &str) -> Result<String, String> {
    positional
        .get(1)
        .cloned()
        .ok_or_else(|| format!("`kiln {verb}` needs {what}, for example `{example}`"))
}

/// A trailing generation on an inspection command: `kiln owns /usr/bin/ls 41`
/// asks a past image rather than the booted one.
fn optional_generation(positional: &[String]) -> Result<Option<u64>, String> {
    match positional.get(2) {
        None => Ok(None),
        Some(arg) => one_generation(arg).map(Some),
    }
}

fn need<'a>(it: &mut impl Iterator<Item = &'a String>, what: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{what} needs a value"))
}

pub fn help() -> &'static str {
    HELP
}
