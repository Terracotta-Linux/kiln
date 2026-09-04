//! Realization: getting every input onto this machine.
//!
//! The expensive, networked half of the build — and the *only* part of a build
//! that touches the network. Assembly step 4 installs "from the artifact store",
//! which is a promise that by then everything is already here: with the network
//! off during assembly, a package that failed to download would surface as a
//! transaction that aborts partway through a tree, rather than as a download
//! that failed.
//!
//! Two halves, and the diagram below is the map:
//!
//! ```text
//! repo package ─────────► fetch          libalpm, DOWNLOAD_ONLY
//! AUR package ──────────┐
//! PKGBUILD ─────────────┤ build's two phases, per build_key,
//! kernel module ────────┘                behind the build cache
//! local .pkg.tar.zst ───► verify+hand over
//! ```
//!
//! Everything on the right becomes a `.pkg.tar.zst` on disk, and the assembler
//! is handed the files. It is one alpm transaction either way — the firm rule
//! is that packaged content goes through pacman, so a built package is not a
//! second mechanism, it is the same mechanism with the file coming from
//! somewhere else.
//!
//! **The plan is the complete input list.** Nothing here asks the AUR RPC a
//! question or resolves a name: the plan already says which commit of which
//! `pkgbase` to clone, which recipe directory to build, and which build key
//! decides whether anything is built at all.

use crate::paths;
use crate::pipeline::Context;
use kiln_alpm::{Config, Request, Session};
use kiln_build::{key::Ingredients, module, BuildRoot, Builder, Recipe};
use kiln_diag::ExitCode;
use kiln_image::assemble;
use kiln_manifest::{Hash, Manifest};
use kiln_resolve::{BuildPlan, ResolvedInput};
use kiln_sandbox::{Bubblewrap, Sandbox};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Download every repository package the plan names into the artifact store.
///
/// The artifacts realization *built* go in too, even though a file on disk
/// needs no downloading: they are what pulls their own dependencies into the
/// cache. Kiln stops the AUR closure wherever the official repositories can
/// satisfy a dependency, so an AUR package's `qt6-base` is named nowhere in the
/// plan — and assembly, with the network off, would resolve it and then fail
/// reaching for a mirror that is not there.
///
/// Returns how many package files the store now holds for this plan. The store
/// is content-addressed and shared between generations, so a rebuild
/// after a one-package change downloads one package.
pub fn fetch(
    plan: &BuildPlan,
    state: &Path,
    repos: Vec<kiln_alpm::RepoSpec>,
    artifacts: &Artifacts,
) -> Result<usize, ExitCode> {
    let files = artifacts.files();
    let transaction = assemble::main_transaction(plan, &files);
    if transaction.is_empty() {
        return Ok(0);
    }

    // A resolution-shaped session: an empty root, because nothing is being
    // installed here. Pointing it at the staging root would make libalpm
    // consider what is already there and download less than the transaction
    // will need.
    let config = Config::for_resolution(state, &plan.image.arch).with_repos(repos);
    let mut session = Session::open(config).map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m {e}");
        ExitCode::System
    })?;

    let files = session.fetch(&transaction).map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m {e}");
        ExitCode::Resolution
    })?;

    let _ = paths::cache(state);
    Ok(files.len())
}

/// What realization produced, keyed by the input that produced it.
///
/// A map rather than a flat list of files because a split package produces
/// several artifacts from one input, and `kiln build -v` should be able to say
/// which input a file came from — and because the next build's dependencies are
/// looked up *by package name* when they were built rather than downloaded.
#[derive(Debug, Default)]
pub struct Artifacts {
    pub produced: BTreeMap<String, Produced>,
}

#[derive(Debug, Clone)]
pub struct Produced {
    pub files: Vec<PathBuf>,
    /// worth reporting. A user who waited zero seconds deserves to know
    /// why.
    pub from_cache: bool,
    /// `aur`, `build`, `module`, `file` — what kind of input this was, for the
    /// one line printed about it.
    pub kind: &'static str,
}

impl Artifacts {
    pub fn files(&self) -> Vec<PathBuf> {
        self.produced
            .values()
            .flat_map(|p| p.files.iter().cloned())
            .collect()
    }

    /// The package files a name was realized into, if it was realized rather
    /// than downloaded. What a later build root needs in order to depend on an
    /// AUR package no mirror has.
    fn for_dependency(&self, name: &str) -> Option<&Produced> {
        self.produced.get(name)
    }
}

/// What realization needs that is not in the plan.
pub struct Options<'a> {
    pub ctx: &'a Context,
    pub manifest: &'a Manifest,
    pub repos: Vec<kiln_alpm::RepoSpec>,
    /// `/var/lib/kiln/build/<plan_id>`. Build roots and scratch recipes live
    /// here, so a failed build leaves everything about itself in one directory.
    pub work: PathBuf,
    /// Keep a failed build's root, and print how to walk back into it.
    pub keep_failed: bool,
}

/// Turn every non-repository input into a package file on disk.
///
/// Errors are collected, not returned at the first one. *one package
/// failing reports all packages that failed, not just the first — the builder
/// continues independent branches of the DAG before reporting.* A dependent of
/// something that failed is not an independent branch, and is skipped rather
/// than attempted and failed a second time for a reason that is not its own.
pub fn realize(
    plan: &BuildPlan,
    opts: &Options<'_>,
    transport: &dyn kiln_aur::Transport,
) -> Result<Artifacts, ExitCode> {
    let mut artifacts = Artifacts::default();
    let jobs = jobs(plan);

    // Nothing to build: resolution already verified the checksum against
    // the bytes on disk, which is the whole guarantee the key exists for.
    for input in &plan.inputs {
        if let ResolvedInput::FilePackage { path, .. } = input {
            artifacts.produced.insert(
                path.clone(),
                Produced {
                    files: vec![opts.ctx.config_root.join(path)],
                    from_cache: false,
                    kind: "file",
                },
            );
        }
    }

    if jobs.is_empty() {
        return Ok(artifacts);
    }
    println!(
        "  {} package{} to realize from source",
        jobs.len(),
        plural(jobs.len())
    );

    let builder = Builder::new(&opts.ctx.state);
    let sandbox = Bubblewrap::new(opts.work.join("build-sandbox"));
    let sources = kiln_build::Sources {
        repos: opts.repos.clone(),
        arch: plan.image.arch.clone(),
        cache: paths::cache(&opts.ctx.state),
        gpgdir: opts.ctx.state.join("keyring"),
        syncdb_from: opts.ctx.state.join("cache/syncdb"),
    };

    // Opened once and reused: the only thing it is asked is what a build-time
    // dependency closure resolves to, which is metadata already on disk.
    let mut session = Session::open(
        Config::for_resolution(&opts.ctx.state, &plan.image.arch).with_repos(opts.repos.clone()),
    )
    .map_err(|e| {
        eprintln!("\x1b[1;31merror\x1b[0m {e}");
        ExitCode::System
    })?;

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut failed: BTreeSet<String> = BTreeSet::new();

    for job in &jobs {
        if let Some(blocker) = job.blocked_by(&failed) {
            println!("  {:<28} skipped — `{blocker}` failed", job.name());
            failed.insert(job.name().to_string());
            continue;
        }
        job.announce();
        match build_one(
            job,
            opts,
            &builder,
            &sandbox,
            &sources,
            &mut session,
            &artifacts,
            transport,
        ) {
            Ok(produced) => {
                describe(job.name(), &produced);
                artifacts.produced.insert(job.name().to_string(), produced);
            }
            Err(why) => {
                failed.insert(job.name().to_string());
                failures.push((job.name().to_string(), why));
            }
        }
    }

    if failures.is_empty() {
        return Ok(artifacts);
    }
    eprintln!(
        "\n\x1b[1;31merror\x1b[0m {} package{} failed to build:\n",
        failures.len(),
        plural(failures.len())
    );
    for (name, why) in &failures {
        eprintln!("\x1b[1m{name}\x1b[0m");
        eprintln!("{}\n", highlight(why));
    }
    Err(ExitCode::Build)
}

/// One thing to build, in the order it has to happen.
enum Job {
    /// An AUR package. There is no separate AUR builder — a clone is
    /// a recipe directory like any other.
    Aur {
        name: String,
        pkgbase: String,
        commit: String,
        evr: String,
        /// The chain back to what the configuration asked for. nothing
        /// enters the image anonymously, and a failure has to name the whole
        /// chain rather than a package the user never wrote down.
        pulled_in_by: Option<String>,
    },
    /// a PKGBUILD in the configuration tree.
    Recipe {
        name: String,
        path: String,
        key: Hash,
    },
    /// an out-of-tree module, compiled against the kernel in the image.
    Module {
        name: String,
        source: String,
        key: Hash,
        kernel_evr: String,
    },
}

impl Job {
    fn name(&self) -> &str {
        match self {
            Job::Aur { name, .. } | Job::Recipe { name, .. } | Job::Module { name, .. } => name,
        }
    }

    /// The package this one cannot be built without, when that package has
    /// already failed.
    fn blocked_by(&self, failed: &BTreeSet<String>) -> Option<String> {
        match self {
            Job::Aur { pulled_in_by, .. } => pulled_in_by
                .as_ref()
                .filter(|by| failed.contains(*by))
                .cloned(),
            _ => None,
        }
    }

    /// The trust seam: *Kiln prints a one-line summary on the first build of
    /// any new AUR package*. Printed at realization rather than at resolution
    /// because this is the moment a stranger's code is about to run, and a line
    /// printed minutes earlier during a `kiln check` is not that moment.
    fn announce(&self) {
        match self {
            Job::Aur {
                name,
                pkgbase,
                commit,
                evr,
                pulled_in_by,
            } => {
                print!(
                    "  \x1b[1maur\x1b[0m {name} {evr} — pkgbase {pkgbase}, commit {}",
                    &commit[..commit.len().min(7)]
                );
                match pulled_in_by {
                    Some(by) => println!(", pulled in by {by}"),
                    None => println!(),
                }
            }
            Job::Recipe { name, path, .. } => println!("  \x1b[1mbuild\x1b[0m {name} ({path})"),
            Job::Module {
                name, kernel_evr, ..
            } => println!("  \x1b[1mmodule\x1b[0m {name} against kernel {kernel_evr}"),
        }
    }
}

/// Every input that has to be built, in an order that respects what depends on
/// what.
///
/// AUR packages come first and deepest-first: `pulled_in_by` records what pulled each one
/// in, so a package's dependencies are strictly deeper than it is, and building
/// from the bottom means an AUR build-dependency is already an artifact by the
/// time the package that needs it assembles its build root.
fn jobs(plan: &BuildPlan) -> Vec<Job> {
    let mut aur: Vec<(usize, Job)> = Vec::new();
    let mut recipes: Vec<Job> = Vec::new();
    let mut modules: Vec<Job> = Vec::new();

    let parents: BTreeMap<&str, Option<&str>> = plan
        .inputs
        .iter()
        .filter_map(|i| match i {
            ResolvedInput::AurPackage {
                name, pulled_in_by, ..
            } => Some((name.as_str(), pulled_in_by.as_deref())),
            _ => None,
        })
        .collect();

    for input in &plan.inputs {
        match input {
            ResolvedInput::AurPackage {
                name,
                pkgbase,
                evr,
                aur_commit,
                pulled_in_by,
                ..
            } => aur.push((
                depth_of(name, &parents),
                Job::Aur {
                    name: name.clone(),
                    pkgbase: pkgbase.clone(),
                    commit: aur_commit.clone(),
                    evr: evr.clone(),
                    pulled_in_by: pulled_in_by.clone(),
                },
            )),
            ResolvedInput::BuiltPackage {
                name,
                path,
                build_key,
                ..
            } => recipes.push(Job::Recipe {
                name: name.clone(),
                path: path.clone(),
                key: build_key.clone(),
            }),
            ResolvedInput::KernelModule {
                name,
                source,
                build_key,
                kernel_evr,
                ..
            } => modules.push(Job::Module {
                name: name.clone(),
                source: source.clone(),
                key: build_key.clone(),
                kernel_evr: kernel_evr.clone(),
            }),
            _ => {}
        }
    }

    // Deepest first, then by name so two packages at the same depth build in an
    // order that does not depend on how the plan was walked.
    aur.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name().cmp(b.1.name())));

    let mut out: Vec<Job> = aur.into_iter().map(|(_, job)| job).collect();
    out.extend(recipes);
    // Last: a module needs the kernel's headers and nothing else, and putting
    // it after the packages keeps the longest builds at the end where a failure
    // has already cost the least.
    out.extend(modules);
    out
}

/// How far a package is from something the configuration named. The closure is
/// a tree by construction (records the *first* package to reach each one),
/// so this walk terminates; the cap is belt and braces against a record that
/// somehow is not.
fn depth_of(name: &str, parents: &BTreeMap<&str, Option<&str>>) -> usize {
    let mut at = name;
    for depth in 0..kiln_aur::closure::MAX_DEPTH + 1 {
        match parents.get(at).copied().flatten() {
            Some(parent) => at = parent,
            None => return depth,
        }
    }
    kiln_aur::closure::MAX_DEPTH
}

#[allow(clippy::too_many_arguments)]
fn build_one(
    job: &Job,
    opts: &Options<'_>,
    builder: &Builder,
    sandbox: &dyn Sandbox,
    sources: &kiln_build::Sources,
    session: &mut Session,
    have: &Artifacts,
    transport: &dyn kiln_aur::Transport,
) -> Result<Produced, String> {
    let arch = &sources.arch;
    let scratch = opts.work.join("recipes").join(job.name());
    let kind: &'static str;

    // The recipe directory, and the identity the cache turns on. A
    // `packages.build` entry and a module already have a `build_key` from the
    // plan; an AUR package cannot, because resolution never had the recipe —
    // its identity is the commit, so that is what the key is built from.
    let (dir, key) = match job {
        Job::Aur {
            name,
            pkgbase,
            commit,
            ..
        } => {
            kind = "aur";
            let _ = std::fs::remove_dir_all(&scratch);
            if let Some(parent) = scratch.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            transport
                .clone_at(&kiln_aur::repository(pkgbase), commit, &scratch)
                .map_err(|e| format!("cloning `{name}`: {e}"))?;
            (scratch.clone(), None)
        }
        Job::Recipe { path, key, .. } => {
            kind = "build";
            // A copy, not the configuration tree itself: this makes the config
            // root somewhere Kiln reads and never writes, and `makepkg` writes
            // to the directory it runs in.
            let _ = std::fs::remove_dir_all(&scratch);
            copy_tree(&opts.ctx.config_root.join(path), &scratch)?;
            (scratch.clone(), Some(key.clone()))
        }
        Job::Module {
            name,
            source,
            key,
            kernel_evr,
        } => {
            kind = "module";
            let dir = module::materialize(
                name,
                &opts.ctx.config_root.join(source),
                &scratch,
                arch,
                &opts.manifest.kernel.package,
                kernel_evr,
            )
            .map_err(|e| e.to_string())?;
            (dir, Some(key.clone()))
        }
    };

    let tree = tree_hash(job, &opts.manifest.local_digests);
    let recipe = Recipe::read(&dir, job.name(), tree, arch, sandbox).map_err(|e| e.to_string())?;

    // Every name the build root has to hold. `depends` as well as
    // `makedepends`: this names only the latter, but `makepkg --nodeps` will
    // not install a runtime dependency and there is no network to install it
    // from, so a package that links against its own `depends` would fail with a
    // missing header rather than with anything about dependencies.
    let wanted: Vec<String> = recipe
        .meta
        .depends
        .iter()
        .chain(&recipe.meta.makedepends)
        .chain(&recipe.meta.checkdepends)
        .map(|d| kiln_aur::closure::bare_name(d).to_string())
        .collect();

    // A dependency Kiln built rather than downloaded goes in as a file: no
    // mirror has it, and asking libalpm for it by name is how an AUR package
    // that depends on another AUR package fails.
    let mut from_repos: Vec<String> = Vec::new();
    let mut prebuilt: Vec<PathBuf> = Vec::new();
    for name in &wanted {
        match have.for_dependency(name) {
            Some(produced) => prebuilt.extend(produced.files.iter().cloned()),
            None => from_repos.push(name.clone()),
        }
    }
    from_repos.sort();
    from_repos.dedup();

    let key = match key {
        Some(key) => key,
        // The build key's ingredients, for the one input kind resolution could not
        // compute them for. The recipe identity is the AUR commit, which is
        // Kiln's whole position on what an AUR package *is*; the build-time
        // closure comes from the same repository snapshot as the image, which
        // is what makes the key correct rather than merely fast.
        None => {
            let makedeps = closure_evrs(session, &from_repos)
                .map_err(|e| format!("resolving what `{}` builds against: {e}", job.name()))?;
            Ingredients::new(aur_recipe_identity(job), arch)
                .with_makedeps(makedeps)
                .build_key()
        }
    };

    // Before the build root, deliberately. The build cache is the single
    // largest speed win in the system, and assembling several hundred megabytes
    // of `base-devel` to then discover there was nothing to do would give most
    // of it back.
    if let kiln_build::cache::Lookup::Hit(files) = builder.cache.lookup(&key) {
        let _ = std::fs::remove_dir_all(&scratch);
        return Ok(Produced {
            files,
            from_cache: true,
            kind,
        });
    }

    let root_dir = opts.work.join("roots").join(job.name());
    let root = BuildRoot::assemble(&root_dir, &from_repos, &prebuilt, sources)
        .map_err(|e| e.to_string())?;

    let realized = builder.realize(&recipe, &key, &root.dir, sandbox);
    match realized {
        Ok(realized) => {
            root.discard();
            let _ = std::fs::remove_dir_all(&scratch);
            Ok(Produced {
                files: realized.artifacts,
                from_cache: realized.from_cache,
                kind,
            })
        }
        Err(e) => {
            // The sandbox root is the only place the half-built tree
            // exists, and deleting it is the difference between a build that
            // can be debugged and one that can only be re-run.
            let mut why = e.to_string();
            if opts.keep_failed {
                // Kept by *not* discarding it — `BuildRoot` has no destructor,
                // because a root that vanished when a value went out of scope
                // would be a build root that disappeared on the one path anyone
                // wants to look at it.
                why.push_str(&format!(
                    "\n\nthe build root has been kept:\n  {root}\n\n\
                     walk back into it with:\n  \
                     sudo bwrap --bind {root} / --proc /proc --dev /dev --tmpfs /tmp \
                     --tmpfs /run --ro-bind {recipe} /build/recipe \
                     --unshare-user --uid 1000 --gid 1000 --chdir /build/recipe -- /bin/bash",
                    root = root.dir.display(),
                    recipe = dir.display(),
                ));
            } else {
                root.discard();
                why.push_str("\n\n`kiln build --keep-failed` keeps the build root to look in.");
            }
            Err(why)
        }
    }
}

/// The recipe's own hash, as `build_key` uses it.
///
/// A `packages.build` entry and a module already have one — the frontend hashed
/// the directory into `local_digests` and it is part of `config_id`. An AUR
/// clone has none, and does not need one: its key is built from the commit.
fn tree_hash(job: &Job, digests: &BTreeMap<String, Hash>) -> Hash {
    match job {
        Job::Recipe { path, .. } => digests.get(path).cloned(),
        Job::Module { source, .. } => digests.get(source).cloned(),
        Job::Aur { .. } => None,
    }
    .unwrap_or_else(|| aur_recipe_identity(job))
}

/// identity is the AUR git commit. Two clones of the same commit are the
/// same recipe, which is exactly what a build key needs to be told.
fn aur_recipe_identity(job: &Job) -> Hash {
    match job {
        Job::Aur {
            pkgbase, commit, ..
        } => Hash::of(format!("aur:{pkgbase}@{commit}").as_bytes()),
        _ => Hash::of(job.name().as_bytes()),
    }
}

/// `name-evr` for the whole build-time dependency closure, resolved
/// against the same repositories as the image.
fn closure_evrs(session: &mut Session, wanted: &[String]) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = wanted.to_vec();
    names.push(kiln_build::root::BASE_DEVEL.to_string());
    session
        .solve(&Request::new(names))
        .map(|closure| {
            closure
                .packages
                .iter()
                .map(|p| format!("{}-{}", p.name, p.version))
                .collect()
        })
        .map_err(|e| e.to_string())
}

fn describe(name: &str, produced: &Produced) {
    let n = produced.files.len();
    let kind = produced.kind;
    if produced.from_cache {
        println!(
            "    {kind} {name}: {n} package{} from the build cache",
            plural(n)
        );
    } else {
        println!("    {kind} {name}: built {n} package{}", plural(n));
    }
}

/// *the last 40 lines are shown inline, with the `==> ERROR:` line
/// highlighted*. That line is makepkg's own summary of what went wrong and is
/// buried in a wall of compiler output without it.
fn highlight(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("==> ERROR:") {
                format!("\x1b[1;31m{line}\x1b[0m")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let out = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .output()
        .map_err(|e| format!("running cp: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "copying {}: {}",
        from.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_resolve::{ImageRef, Provenance, UidMap};

    fn plan(inputs: Vec<ResolvedInput>) -> BuildPlan {
        let mut plan = BuildPlan {
            config_id: Hash("b3:test".into()),
            image: ImageRef {
                name: "test".into(),
                arch: "x86_64".into(),
            },
            inputs,
            volatile: Vec::new(),
            uid_map: UidMap::new(),
            provenance: Provenance {
                resolved_at: "2026-09-01T00:00:00Z".into(),
                snapshot: "2026-09-01".into(),
                repos: Vec::new(),
                libalpm: "0".into(),
            },
        };
        plan.canonicalize();
        plan
    }

    fn aur(name: &str, pulled_in_by: Option<&str>) -> ResolvedInput {
        ResolvedInput::AurPackage {
            name: name.into(),
            pkgbase: name.into(),
            evr: "1-1".into(),
            aur_commit: "3f1a9c8e".into(),
            srcinfo_hash: Hash("b3:aa".into()),
            pulled_in_by: pulled_in_by.map(str::to_string),
        }
    }

    /// `pulled_in_by` records what pulled each AUR package in, so a package's dependencies
    /// are strictly deeper than it is. Building from the bottom is what makes
    /// an AUR build-dependency an artifact by the time the package that needs
    /// it assembles its build root — the case that has no other answer, because
    /// no mirror has the dependency.
    #[test]
    fn aur_packages_build_from_the_bottom_of_the_closure_up() {
        // top → middle → bottom, declared in the order that would be wrong.
        let jobs = jobs(&plan(vec![
            aur("top", None),
            aur("bottom", Some("middle")),
            aur("middle", Some("top")),
        ]));
        let order: Vec<&str> = jobs.iter().map(Job::name).collect();
        assert_eq!(order, ["bottom", "middle", "top"]);
    }

    /// Two packages at the same depth build in an order that does not depend on
    /// how the plan happened to be walked — the same rule everything else in
    /// Kiln follows about order never mattering.
    #[test]
    fn packages_at_the_same_depth_build_in_name_order() {
        let jobs = jobs(&plan(vec![
            aur("zebra", Some("root-pkg")),
            aur("alpha", Some("root-pkg")),
            aur("root-pkg", None),
        ]));
        let order: Vec<&str> = jobs.iter().map(Job::name).collect();
        assert_eq!(order, ["alpha", "zebra", "root-pkg"]);
    }

    /// a module needs the kernel's headers and nothing else, so it goes
    /// last — where a failure has already cost the least.
    #[test]
    fn recipes_follow_the_aur_and_modules_come_last() {
        let jobs = jobs(&plan(vec![
            ResolvedInput::KernelModule {
                name: "v4l2loopback".into(),
                source: "modules/v4l2loopback".into(),
                build_key: Hash("b3:ee".into()),
                recipe: Hash("b3:ff".into()),
                kernel_evr: "6.19.2-1".into(),
            },
            ResolvedInput::BuiltPackage {
                name: "my-tool".into(),
                path: "pkgbuilds/my-tool".into(),
                build_key: Hash("b3:cc".into()),
                recipe: Hash("b3:dd".into()),
                sources: Vec::new(),
            },
            aur("zen-browser-bin", None),
        ]));
        let order: Vec<&str> = jobs.iter().map(Job::name).collect();
        assert_eq!(order, ["zen-browser-bin", "my-tool", "v4l2loopback"]);
    }

    /// A repository package and a `[[file]]` are not built. Realization's job
    /// list is exactly what puts on the left of the artifact store.
    #[test]
    fn nothing_that_is_merely_downloaded_reaches_the_builder() {
        let jobs = jobs(&plan(vec![
            ResolvedInput::RepoPackage {
                name: "linux".into(),
                evr: "6.19.2-1".into(),
                filename: "linux-6.19.2-1-x86_64.pkg.tar.zst".into(),
                sha256: "3c9f".into(),
                repo: "core".into(),
                explicit: true,
            },
            ResolvedInput::FilePackage {
                path: "packages/myapp.pkg.tar.zst".into(),
                sha256: "9f2c".into(),
            },
        ]));
        assert!(jobs.is_empty());
    }

    /// *one package failing reports all packages that failed.* A
    /// dependent of something that failed is not an independent branch — it is
    /// skipped, rather than attempted and failed a second time for a reason
    /// that is not its own.
    #[test]
    fn a_dependent_of_a_failed_package_is_skipped_rather_than_blamed() {
        let failed = BTreeSet::from(["bottom".to_string()]);
        let jobs = jobs(&plan(vec![aur("top", Some("bottom")), aur("bottom", None)]));
        let top = jobs.iter().find(|j| j.name() == "top").unwrap();
        let bottom = jobs.iter().find(|j| j.name() == "bottom").unwrap();
        assert_eq!(top.blocked_by(&failed).as_deref(), Some("bottom"));
        assert_eq!(bottom.blocked_by(&failed), None);
    }

    /// identity is the AUR git commit, so two clones of the same commit
    /// are the same recipe — and a force-push at the same version is a
    /// different one, which is the whole reason commits are tracked.
    #[test]
    fn an_aur_recipes_identity_is_its_commit() {
        let one = aur_recipe_identity(&Job::Aur {
            name: "zen-git".into(),
            pkgbase: "zen-git".into(),
            commit: "3f1a9c8e".into(),
            evr: "1-1".into(),
            pulled_in_by: None,
        });
        let same = aur_recipe_identity(&Job::Aur {
            name: "zen-git".into(),
            pkgbase: "zen-git".into(),
            commit: "3f1a9c8e".into(),
            evr: "9-9".into(),
            pulled_in_by: Some("something".into()),
        });
        let forced = aur_recipe_identity(&Job::Aur {
            name: "zen-git".into(),
            pkgbase: "zen-git".into(),
            commit: "0000000".into(),
            evr: "1-1".into(),
            pulled_in_by: None,
        });
        assert_eq!(one, same, "the version is not the identity");
        assert_ne!(one, forced, "the commit is");
    }

    /// makepkg's own `==> ERROR:` line is the one sentence that says
    /// what went wrong, and it is buried in a wall of compiler output.
    #[test]
    fn the_error_line_is_the_one_that_is_highlighted() {
        let text = "gcc: warning\n==> ERROR: A failure occurred in build().\ncc1: note";
        let out = highlight(text);
        assert!(out.contains("\x1b[1;31m==> ERROR: A failure occurred in build()."));
        assert!(!out.contains("\x1b[1;31mgcc: warning"));
    }
}
