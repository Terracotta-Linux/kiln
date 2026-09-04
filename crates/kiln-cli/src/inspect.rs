//! `kiln diff`, `kiln why`, `kiln owns`.
//!
//! The three questions somebody asks about an image they did not just build,
//! and the reason the build record exists in the first place: every one
//! of them has to be answerable about a generation built from a configuration
//! that has since been edited or deleted, which is the *normal* case when you
//! are debugging why the image you rolled back to behaves differently.
//!
//! So none of these commands reads `/etc/kiln`. `diff` reads two commits'
//! records; `why` and `owns` read a deployment's own pacman database, which
//! put at `/usr/lib/sysimage/pacman` inside the image precisely so that
//! they work offline against a booted system.

use crate::{check, paths};
use kiln_alpm::{Config, Session};
use kiln_diag::ExitCode;
use kiln_ostree::{commit, Generation, Metadata, Sysroot};
use kiln_record::Record;
use std::path::Path;

/// `kiln diff [<gen>] [<gen>]`.
///
/// The defaults are the whole ergonomics of the command, and they follow from
/// what a person is looking at when they type it:
///
/// | typed | compares |
/// |---|---|
/// | `kiln diff` | the booted generation against the one that boots next |
/// | `kiln diff 41` | generation 41 against the booted one |
/// | `kiln diff 39 41` | exactly those two |
///
/// This words the default as "booted vs pending", and pending is the deployment
/// staged for the next boot. When there is none — the ordinary steady state —
/// there is nothing to diff and the command says so and points at `kiln check`,
/// which answers the question the user was probably asking: what *would*
/// change. Silently diffing something else would be worse than declining.
pub fn diff(sysroot: Option<&Path>, from: Option<u64>, to: Option<u64>) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(exit) => return exit,
    };
    let generations = match sysroot.generations() {
        Ok(g) => g,
        Err(e) => return code(&e),
    };

    let (a, b) = match (from, to) {
        (Some(a), Some(b)) => (a, b),
        (Some(a), None) => match booted(&generations) {
            Some(b) if b != a => (a, b),
            Some(_) => {
                println!("Generation {a} is the one you are running; nothing to compare it to.");
                println!("`kiln diff {a} <gen>` compares it against another one.");
                return ExitCode::Ok;
            }
            None => {
                eprintln!(
                    "\x1b[1;31merror\x1b[0m nothing is booted from {}, so there is no second \
                     generation to compare against. Name both: `kiln diff <gen> <gen>`.",
                    root.display()
                );
                return ExitCode::System;
            }
        },
        (None, _) => match default_pair(&generations) {
            Some(pair) => pair,
            // Two different situations, and they need different answers.
            // Nothing booted means `--sysroot` against a machine that is not
            // this one, where "booted vs pending" has no first term at all.
            None if booted(&generations).is_none() => {
                println!("Nothing is booted from {}, so there is no", root.display());
                println!("\"booted vs pending\" to show. Name both: `kiln diff <gen> <gen>`.");
                println!("\n`kiln list` shows what is there.");
                return ExitCode::Ok;
            }
            None => {
                println!("Nothing is pending: the generation you are running is the one that");
                println!("boots next, so there is nothing to diff.");
                println!("\n`kiln check` compares your configuration against it.");
                return ExitCode::Ok;
            }
        },
    };

    let from = match record_of(&sysroot, a) {
        Ok(r) => r,
        Err(exit) => return exit,
    };
    let to = match record_of(&sysroot, b) {
        Ok(r) => r,
        Err(exit) => return exit,
    };

    let report = check::between(&from, &to);
    println!("generation {a} → {b}");
    println!("  built      {}  →  {}", from.built_at, to.built_at);
    println!();
    if report.is_empty() {
        // Two generations with the same plan are two builds of the same
        // configuration — a `--force` rebuild, or a `kiln rebuild`. Saying so
        // is more useful than an empty table.
        if from.plan_id == to.plan_id {
            println!("  Identical: both generations were built from the same plan.");
            println!("  plan {}", from.plan_id);
        } else {
            println!("  No difference in any input category.");
        }
        return ExitCode::Ok;
    }
    print!("{}", report.render());
    ExitCode::Ok
}

/// The booted generation and the one that boots next, when they differ.
///
/// The deployment list is in boot order, so the pending one is the front
/// of the list; it is "pending" exactly when it is not also the booted one.
fn default_pair(generations: &[Generation]) -> Option<(u64, u64)> {
    let next = generations.first()?;
    let booted = generations.iter().find(|g| g.booted)?;
    (next.number != booted.number).then_some((booted.number, next.number))
}

fn booted(generations: &[Generation]) -> Option<u64> {
    generations.iter().find(|g| g.booted).map(|g| g.number)
}

/// `kiln show <gen>` — the full manifest and build record of a
/// generation, read out of its commit.
///
/// The commit rather than the deployment, so this works on a generation that
/// was committed and never deployed, and on one whose deployment `kiln clean`
/// has since taken. Kiln puts the manifest and the record in commit metadata for
/// exactly this.
pub fn show(sysroot: Option<&Path>, generation: u64, verbose: bool) -> ExitCode {
    let root = paths::sysroot(sysroot);
    let sysroot = match open(&root) {
        Ok(s) => s,
        Err(exit) => return exit,
    };
    let (checksum, metadata) = match commit::find_generation(&sysroot.repo(), generation) {
        Ok(found) => found,
        Err(e) => return code(&e),
    };

    println!("generation  {}", metadata.generation);
    println!("image       {} {}", metadata.image, metadata.arch);
    println!("built       {} on {}", metadata.built_at, metadata.built_by);
    // never print an OSTree checksum where a generation number would do.
    // `kiln show` is one of the two places the design says it belongs.
    println!("commit      {checksum}");
    println!("plan        {}", metadata.plan_id);
    println!("config      {}", metadata.config_id);

    match &metadata.manifest {
        Some(manifest) => {
            println!();
            crate::show::summary(manifest, &[], verbose);
            crate::show::detail(manifest);
        }
        None => println!(
            "\nNo manifest in this commit: it was built by a Kiln that did not write one, \n\
             so `kiln rebuild {generation}` cannot reconstruct it either."
        ),
    }

    match &metadata.record {
        Some(record) => {
            println!();
            println!("record");
            println!("  snapshot        {}", record.repos.snapshot);
            counted("repo packages", record.repo_packages.len());
            counted("aur packages", record.aur_packages.len());
            counted("built packages", record.built_packages.len());
            counted("local packages", record.local_packages.len());
            counted("hashed files", record.local_files.len());
            counted("scripts", record.scripts.len());
            counted("service accounts", record.uid_map.len());
            if verbose {
                println!();
                println!("{}", record.to_json());
            } else {
                println!("\n  `kiln show {generation} --verbose` prints the whole record.");
            }
        }
        None => println!("\nNo build record in this commit."),
    }
    ExitCode::Ok
}

fn counted(what: &str, n: usize) {
    if n > 0 {
        println!("  {what:<15} {n}");
    }
}

/// `kiln why <package>` — what pulled it in.
///
/// Answered from the image, not from the plan, and the two really do differ:
/// the plan names the packages the *configuration* asked for, and the image
/// contains their whole dependency closure. "What pulled `libxkbcommon` in" is
/// a question about that closure, and only the pacman database has it.
pub fn why(sysroot: Option<&Path>, package: &str, generation: Option<u64>) -> ExitCode {
    let (root, metadata) = match target(sysroot, generation) {
        Ok(t) => t,
        Err(exit) => return exit,
    };
    let session = match session(&root, &metadata) {
        Ok(s) => s,
        Err(exit) => return exit,
    };

    let Some(found) = session.installed_package(package) else {
        eprintln!(
            "\x1b[1;31merror\x1b[0m generation {} does not contain `{package}`.",
            metadata.generation
        );
        eprintln!(
            "\n`kiln show {}` lists what it does contain.",
            metadata.generation
        );
        return ExitCode::System;
    };

    if !found.asked_for {
        // The message is about what the user typed, and they typed a virtual
        // name. Saying which real package answers it is the first half of the
        // answer, not a footnote.
        println!("`{package}` is provided by {}.", found.name);
    }
    println!("{} {}", found.name, found.version);

    // The build record says which *kind* of input it was, which the pacman
    // database cannot: an AUR package and a repository package are both just
    // installed packages by the time the image exists.
    if let Some(record) = &metadata.record {
        for line in provenance(record, &found.name) {
            println!("  {line}");
        }
    }

    if found.explicit {
        println!("  named in the configuration");
    }
    if !found.required_by.is_empty() {
        println!("  required by {}", join(&found.required_by));
    }
    if !found.optional_for.is_empty() {
        println!("  optional for {}", join(&found.optional_for));
    }
    if !found.explicit && found.required_by.is_empty() {
        // Neither asked for nor needed. Real, and worth saying plainly: it is
        // what an `exclude` or a removed dependency leaves behind, and it is
        // the answer to "why is this still here".
        println!("  nothing requires it, and the configuration does not name it");
    }
    ExitCode::Ok
}

/// What the build record says about how a package got into the image, beyond
/// "it is installed".
fn provenance(record: &Record, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = record.repo_packages.iter().find(|p| p.name == name) {
        out.push(format!("from the {} repository", p.repo));
    }
    if let Some(p) = record.aur_packages.iter().find(|p| p.name == name) {
        out.push(match &p.pulled_in_by {
            // nothing enters the image anonymously, and a transitively
            // pulled AUR package is marked with what required it.
            Some(by) => format!(
                "from the AUR at commit {}, pulled in by {by}",
                short(&p.aur_commit)
            ),
            None => format!("from the AUR at commit {}", short(&p.aur_commit)),
        });
    }
    if let Some(p) = record.built_packages.iter().find(|p| p.name == name) {
        out.push(match &p.kernel_evr {
            Some(kernel) => format!(
                "built from source against kernel {kernel} (build key {})",
                short(&p.build_key)
            ),
            None => format!("built from source (build key {})", short(&p.build_key)),
        });
    }
    out
}

/// `kiln owns <path>` — which package owns a file.
pub fn owns(sysroot: Option<&Path>, path: &str, generation: Option<u64>) -> ExitCode {
    let (root, metadata) = match target(sysroot, generation) {
        Ok(t) => t,
        Err(exit) => return exit,
    };
    let session = match session(&root, &metadata) {
        Ok(s) => s,
        Err(exit) => return exit,
    };

    // The user types the path they see on a running system; inside the
    // commit `/etc` is `/usr/etc`, and the pacman file list has neither
    // rewriting nor a leading slash. Both spellings are tried so that
    // `kiln owns /etc/pacman.conf` works on a booted machine and against a
    // `--sysroot` alike.
    let candidates = [path.to_string(), etc_to_usr_etc(path)];
    for candidate in &candidates {
        if let Some(owner) = session.owns(candidate) {
            println!("{owner}");
            if candidate != path {
                println!(
                    "  {path} is {candidate} in the image: Kiln moves /etc to /usr/etc and the \
                     live /etc is merged onto it at deploy."
                );
            }
            if let Some(found) = session.installed_package(&owner) {
                println!("  {} {}", found.name, found.version);
            }
            return ExitCode::Ok;
        }
    }

    println!(
        "No package in generation {} owns {path}.",
        metadata.generation
    );
    // The three ways a path exists in a Kiln image without a package owning it,
    // in the order they are likely. Each is a real answer, and "no owner" alone
    // sends the user looking for a bug that is not there.
    println!("  It may be a `[[file]]` Kiln placed, a build script's output");
    println!("  or runtime state under /var, which is not image content at all.");
    println!(
        "\n  `kiln show {}` lists what the generation was asked to contain.",
        metadata.generation
    );
    ExitCode::Ok
}

fn etc_to_usr_etc(path: &str) -> String {
    match path.trim_start_matches('/').strip_prefix("etc/") {
        Some(tail) => format!("usr/etc/{tail}"),
        None => path.trim_start_matches('/').to_string(),
    }
}

/// Which deployment `why` and `owns` ask, and what its commit says about
/// itself.
///
/// Defaults to the booted generation, because that is the system the person is
/// sitting in front of. A named generation has to be *deployed*: these two
/// commands read a checked-out tree, and a commit that was never deployed does
/// not have one.
fn target(
    sysroot: Option<&Path>,
    generation: Option<u64>,
) -> Result<(std::path::PathBuf, Metadata), ExitCode> {
    let root = paths::sysroot(sysroot);
    let sysroot = open(&root)?;
    let generations = sysroot.generations().map_err(|e| code(&e))?;

    let wanted = match generation {
        Some(n) => n,
        None => match generations
            .iter()
            .find(|g| g.booted)
            .or(generations.first())
        {
            Some(g) => g.number,
            None => {
                eprintln!(
                    "\x1b[1;31merror\x1b[0m there are no Kiln deployments on {}.",
                    root.display()
                );
                return Err(ExitCode::System);
            }
        },
    };

    let tree = sysroot.deployment_root(wanted).map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m {e}");
        if generations.iter().all(|g| g.number != wanted) {
            eprintln!(
                "\nA generation has to be deployed to be queried this way: `kiln why` and \
                 `kiln owns` read the image's own package database, and a commit that \
                 was never deployed has no tree checked out. `kiln show {wanted}` reads its \
                 record instead."
            );
        }
        ExitCode::System
    })?;

    let checksum = generations
        .iter()
        .find(|g| g.number == wanted)
        .map(|g| g.checksum.clone())
        .unwrap_or_default();
    let metadata = commit::read_metadata(&sysroot.repo(), &checksum).map_err(|e| code(&e))?;
    Ok((tree, metadata))
}

/// An alpm handle over a deployment's own database. No repositories are
/// registered: every question here is about what the image *has*, and a sync
/// database would only invite one about what it could have.
fn session(tree: &Path, metadata: &Metadata) -> Result<Session, ExitCode> {
    Session::open(Config::for_root(tree, &metadata.arch)).map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m reading the image's package database: {e}");
        ExitCode::System
    })
}

/// A generation's record, from the *commit* rather than from a deployment.
///
/// `commit::find_generation` walks the ref's history, so `kiln diff` reaches a
/// generation whose deployment `kiln clean` has since removed — which
/// names as one of the jobs the record exists for.
fn record_of(sysroot: &Sysroot, generation: u64) -> Result<Record, ExitCode> {
    let (_, metadata) =
        commit::find_generation(&sysroot.repo(), generation).map_err(|e| code(&e))?;
    metadata.record.ok_or_else(|| {
        eprintln!(
            "\x1b[1;31merror\x1b[0m generation {generation} carries no build record, so there \
             is nothing to compare. It was built by a Kiln that did not write one."
        );
        ExitCode::System
    })
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

fn join(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        // Long dependency lists are common — `glibc` is required by hundreds —
        // and a wrapped wall of names answers nothing. The count is the useful
        // part past a handful.
        n if n > 6 => format!("{} and {} others", names[..6].join(", "), n - 6),
        _ => names.join(", "),
    }
}

fn short(hash: &str) -> String {
    match hash.strip_prefix("b3:") {
        Some(hex) => format!("b3:{}", &hex[..8.min(hex.len())]),
        None => hash.chars().take(8).collect(),
    }
}

fn code(e: &kiln_ostree::Error) -> ExitCode {
    eprintln!("\x1b[1;31merror\x1b[0m {e}");
    ExitCode::System
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Somebody sitting in a booted system types the path they can see.
    /// Inside the commit it is under `/usr/etc`, which is where the pacman file
    /// list has it — and without the translation `kiln owns /etc/pacman.conf`
    /// says nothing owns a file that `pacman` itself installed.
    #[test]
    fn an_etc_path_is_also_looked_for_under_usr_etc() {
        assert_eq!(etc_to_usr_etc("/etc/pacman.conf"), "usr/etc/pacman.conf");
        assert_eq!(etc_to_usr_etc("etc/motd"), "usr/etc/motd");
        assert_eq!(etc_to_usr_etc("/usr/bin/ls"), "usr/bin/ls");
    }

    #[test]
    fn a_long_required_by_list_is_summarised_rather_than_wrapped() {
        let many: Vec<String> = (0..12).map(|n| format!("pkg{n}")).collect();
        assert_eq!(
            join(&many),
            "pkg0, pkg1, pkg2, pkg3, pkg4, pkg5 and 6 others"
        );
        assert_eq!(join(&many[..2]), "pkg0, pkg1");
    }
}
