//! The assembler.
//!
//! Eleven steps, in an order where every one depends on the one before it. The
//! individual steps live in their own modules and are tested there; this file
//! is the sequence, and the sequence is where the findings are.
//!
//! ```text
//!  1 skeleton          the database directory and the mountpoints, nothing else
//!  2 base transaction  `filesystem` alone, so step 3 has an /etc/passwd to seed
//!  3 UID seed          replay the previous generation's ids            (U1)
//!  4 transaction       everything else, package hooks shadowed         (H1–H4)
//!  5 scripts           `after = "packages"` — an overlayfs changeset
//!  6 overlay           [[file]], checked against the pacman file database
//!  7 unit state        presets, masks, `systemctl preset-all --root`
//!  8 scripts           `after = "files"` — an overlayfs changeset
//!  9 kernel            depmod, initramfs, /boot cleared                (K1, K3)
//! 10 normalize         /etc, the /var drain, the top level          (N1–N7, D1–D3)
//! 11 self-description  usr/lib/kiln/{manifest.json,record.json}
//! ```
//!
//! Steps 5 and 8 run the same code with a different phase. They are two slots
//! rather than one because a script that needs `[[file]]` content already in
//! place and a script that has to run before it are different jobs, and
//! makes the user say which — `after = "packages"` or `after = "files"`.

use crate::tree::{self, Result};
use crate::{
    bootcount, drain, hooks, kernel, normalize, overlay, scripts, skeleton, uid, units, verify,
};
use kiln_alpm::{RepoSpec, Session, Transaction};
use kiln_manifest::{Manifest, ScriptPhase};
use kiln_record::Record;
use kiln_resolve::{BuildPlan, EnableState, ResolvedInput};
use kiln_sandbox::{Sandbox, SandboxSpec, Shim};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where Kiln describes itself inside the image. step 11.
pub const SELF_DIR: &str = "usr/lib/kiln";

/// Where the shims go *inside the staging root* for the duration of the
/// transaction.
///
/// libalpm runs a package's `.INSTALL` scriptlet chrooted into the install
/// root, with pacman's own `PATH` — which begins with `/usr/local/sbin` and
/// `/usr/local/bin`. That is the shadowing lever for scriptlets, the same shape
/// as the `HookDir` lever for hooks: the directory is empty in a fresh
/// Arch image, so nothing is being displaced, and it is emptied again before
/// the commit.
pub const SHIM_DIR_IN_ROOT: &str = "usr/local/bin";

/// The package `filesystem` is installed alone, first. It owns
/// `/etc/passwd` and `/etc/group`, so seeding pinned IDs before it makes pacman
/// abort on a file conflict, and `--overwrite` would let the stock files
/// clobber the pins.
pub const BASE_PACKAGE: &str = "filesystem";

pub struct Options {
    /// `/var/lib/kiln/build/<plan_id>/root`.
    pub root: PathBuf,
    pub config_root: PathBuf,
    /// `/var/lib/kiln/build/<plan_id>`. Holds the shadow hooks and the shim
    /// log — facts about the build, which must not end up in the image.
    pub work: PathBuf,
    /// The artifact store. Everything is already here; step 4 does no
    /// network.
    pub cache: PathBuf,
    /// The `.pkg.tar.zst` files realization produced or was handed: AUR
    /// packages, `packages.build` output, out-of-tree modules, and
    /// `packages.file` blobs.
    ///
    /// They arrive as paths rather than as names because no database anywhere
    /// knows them. Assembly is handed the files, not asked to find them —
    /// Assembly step 4 installs "from the artifact store", and this is the half of
    /// that store no mirror could have supplied.
    pub artifacts: Vec<PathBuf>,
    pub repos: Vec<RepoSpec>,
    /// The resolution session's database directory, whose `sync/` holds the
    /// repository metadata already downloaded during resolution.
    ///
    /// Assembly copies it in rather than refreshing. step 4 does no
    /// network, and libalpm cannot find a package by name without the sync
    /// databases — so without this, either assembly goes online or the
    /// transaction cannot resolve `filesystem`. Normalization drops them again
    /// before the commit: they are a snapshot of what the mirrors held at build
    /// time, not image content.
    pub syncdb_from: PathBuf,
    /// Kiln's own pacman keyring. The transaction verifies every
    /// package's signature, and `Config::for_root` sets no gpgdir of its own —
    /// so without this the install fails on the first signed package, having
    /// already downloaded all of them.
    pub gpgdir: PathBuf,
    pub generation: u64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub installed: Vec<String>,
    /// Hooks that fired, split by whether they were the package's own or
    /// Kiln's no-op shadow.
    pub hooks_fired: Vec<String>,
    pub hooks_shadowed: Vec<String>,
    /// `shimmed: systemctl daemon-reload`. A scriptlet quietly failing to
    /// do what it thinks it did is worth knowing about.
    pub shimmed: Vec<String>,
    pub uid_drift: Vec<uid::Drift>,
    /// Each script's record fills both slots. One value rather than two: a reader wants to know
    /// what the scripts did, and which of the two phases a given one ran in is
    /// already on every entry.
    pub scripts: scripts::Applied,
    pub files: overlay::Applied,
    pub units: units::Applied,
    pub kernel: Option<kernel::Kernel>,
    pub normalize: normalize::Report,
    pub record: Option<Record>,
}

/// Build the image tree. The staging root must not exist or must be empty —
/// assembly builds from nothing (step 1).
pub fn assemble(
    plan: &BuildPlan,
    manifest: &Manifest,
    opts: &Options,
    sandbox: &dyn Sandbox,
) -> Result<Report> {
    let root = opts.root.as_path();
    let mut report = Report::default();

    // 1 ─────────────────────────────────────────────────────────────────────
    skeleton::create(root)?;

    // 2 ─────────────────────────────────────────────────────────────────────
    import_sync_databases(root, &opts.syncdb_from)?;
    let hookdir = hooks::materialize(&opts.work.join("hooks"))?;
    let mut session = Session::open(
        kiln_alpm::Config::for_root(root, &plan.image.arch)
            .with_repos(opts.repos.clone())
            .with_cache(&opts.cache)
            .with_gpgdir(&opts.gpgdir)
            .with_hookdir(&hookdir),
    )?;

    let main = main_transaction(plan, &opts.artifacts);
    // an empty configuration produces an empty image. There is nothing to
    // install, nothing to seed against, and no reason for either transaction.
    if !main.is_empty() {
        // libalpm runs scriptlets and hooks chrooted into the staging root, and
        // systemd's tools need /proc there — see `mounts` for the failure this
        // otherwise produces, which names a file that is present and readable.
        // The guard unmounts on the error path too.
        let _mounted = kiln_alpm::Mounts::setup(root)?;

        if !session.provides(BASE_PACKAGE) {
            return Err(tree::shape(format!(
                "no package provides `{BASE_PACKAGE}`, which owns /etc/passwd. Kiln installs                  it alone and first so that the UID seed has account files to write into                  (step 2); without it the seed would either abort the transaction on a                  file conflict or be clobbered by the stock files"
            )));
        }
        let base = session.install(&Transaction::new([BASE_PACKAGE.to_string()]))?;
        absorb(&mut report, &base);

        // 3 ─────────────────────────────────────────────────────────────────
        uid::seed(root, &plan.uid_map, &uid::HostSysusers)?;
        provide_hook_directories(root)?;

        // 4 ─────────────────────────────────────────────────────────────────
        // Whatever step 2 installed is excluded by *what is in the database*,
        // not by name. Reinstalling it here would re-extract the stock
        // `/etc/passwd` and `/etc/group` over the seed — arriving
        // through the back door, with the seed silently undone and the drift
        // only visible a generation later. A name check misses it the moment
        // the package satisfying `filesystem` is called something else.
        let main = without_installed(&main, &session);
        let shims = place_shims(root)?;
        let outcome = session.install(&main);
        report.shimmed = collect_shim_log(root);
        remove_shims(root, &shims)?;
        absorb(&mut report, &outcome?);
    }

    let script_opts = scripts::Options {
        root,
        work: &opts.work.join("scripts"),
        config_root: &opts.config_root,
        image: &plan.image.name,
        generation: opts.generation,
        arch: &plan.image.arch,
    };

    // 5 ─────────────────────────────────────────────────────────────────────
    report.scripts = scripts::run(
        ScriptPhase::Packages,
        &manifest.scripts,
        &script_opts,
        &session,
        sandbox,
    )?;

    // 6 ─────────────────────────────────────────────────────────────────────
    report.files = overlay::apply(root, &opts.config_root, &manifest.files, &session)?;

    // The bootcount install goes between steps 6 and 7 because that is where each half belongs: the
    // grub.d fragment and the boot-success script are ordinary image content,
    // and the unit that runs the script has to be in the map step 7 presets.
    // Written for every image — `boot.loader` takes one value — and
    // inert where GRUB is absent.
    bootcount::install(root, bootcount::TRIES)?;

    // 7 ─────────────────────────────────────────────────────────────────────
    let mut units = manifest.systemd.units.clone();
    units.insert(bootcount::UNIT.to_string(), bootcount::unit());
    let mut states = unit_states(plan);
    states.insert(bootcount::UNIT.to_string(), EnableState::Enabled);
    report.units = units::apply(
        root,
        &opts.config_root,
        &states,
        &units,
        &session,
        &units::HostSystemctl,
    )?;

    // 8 ─────────────────────────────────────────────────────────────────────
    // After step 6, so a script can read the `[[file]]` content the
    // configuration put in the image — which is Kiln's answer to "a script
    // needs data": put it in with `[[file]]`, where it gets hashed, rather than
    // letting the script reach outside for it.
    report.scripts.absorb(scripts::run(
        ScriptPhase::Files,
        &manifest.scripts,
        &script_opts,
        &session,
        sandbox,
    )?);

    // The alpm session holds an open database inside the tree that is about to
    // be rewritten. Nothing after this point asks it anything.
    drop(session);

    // 9 ─────────────────────────────────────────────────────────────────────
    report.kernel = Some(build_kernel(root, sandbox)?);

    // 10 ────────────────────────────────────────────────────────────────────
    // Captured *before* normalization: this reads `etc/passwd`, and after step
    // 10 there is no `/etc`.
    let captured = uid::capture(root);
    report.uid_drift = uid::drift(&plan.uid_map, &captured);
    report.normalize = normalize::run(root)?;

    // 11 ────────────────────────────────────────────────────────────────────
    let mut record = Record::of(plan, opts.generation, captured);
    // The changeset is the output side of a script, which `Record::of`
    // cannot know because it did not exist until the script produced it. This
    // is what `kiln rebuild` compares to name the one script in a
    // configuration that is not reproducible.
    record.script_effects = report.scripts.effects();
    describe_self(root, manifest, &record)?;
    report.record = Some(record);

    let problems = verify::check(root);
    if !problems.is_empty() {
        return Err(tree::shape(verify::describe(&problems)));
    }
    Ok(report)
}

/// Everything the plan contributes to the image's package set, with the names
/// the configuration asked for marked explicit — which is what makes `pacman
/// -Qe` on a booted image mean something.
pub fn main_transaction(plan: &BuildPlan, artifacts: &[PathBuf]) -> Transaction {
    let mut packages = Vec::new();
    let mut explicit = Vec::new();
    for input in &plan.inputs {
        // Repository packages only. Everything else the plan calls a package —
        // AUR, `packages.build`, a kernel module, a local blob — has no entry
        // in any sync database, and arrives as a file in `artifacts` instead.
        // Naming them here is how they used to reach libalpm as "no package
        // named `pkgbuilds/mytool`".
        let ResolvedInput::RepoPackage {
            name, explicit: e, ..
        } = input
        else {
            continue;
        };
        packages.push(name.clone());
        if *e {
            explicit.push(name.clone());
        }
    }
    Transaction::new(packages)
        .explicitly(explicit)
        .with_locals(artifacts.to_vec())
}

/// Directories under `/var` that a package hook writes a *generated cache*
/// into, and that nothing in the tree creates.
///
/// This list exists because Kiln shadows `21-systemd-tmpfiles.hook`
/// — which is right, since it would materialize `/root/.ssh` and
/// similar machine state into the image — and that hook is also what would
/// otherwise have created these. Shadowing it takes the directories away from
/// the hooks Kiln deliberately *keeps*.
///
/// `journalctl --update-catalog` is the one that made this visible. It fails
/// with `Failed to open file /usr/lib/systemd/catalog/<x>.catalog: No such file
/// or directory` — naming its *input*, which is present and readable, when what
/// is missing is the directory it writes the database to. The message sends you
/// looking in exactly the wrong place, which is why the reason is written down
/// here rather than left to be rediscovered.
///
/// What lands in them is drained to `/usr/share/factory` like any other `/var`
/// content, so the cache is restored on a machine whose `/var` is
/// empty and left alone on one that has its own.
const HOOK_OUTPUT_DIRS: &[&str] = &["var/lib/systemd/catalog"];

fn provide_hook_directories(root: &Path) -> Result<()> {
    for dir in HOOK_OUTPUT_DIRS {
        tree::mkdir(&root.join(dir))?;
    }
    Ok(())
}

/// Drop from a transaction anything the root already has. See the call site:
/// re-extracting `filesystem` over a seeded `/etc/passwd` is the same problem
/// as installing it before the seed, just with the order right and the effect
/// the same.
fn without_installed(transaction: &Transaction, session: &Session) -> Transaction {
    let installed: std::collections::BTreeSet<String> =
        session.installed().into_iter().map(|(n, _)| n).collect();
    Transaction {
        packages: transaction
            .packages
            .iter()
            .filter(|p| !installed.contains(*p))
            .cloned()
            .collect(),
        explicit: transaction.explicit.clone(),
        // Untouched: an artifact is identified by a path, and step 2 installed
        // `filesystem` from a repository. Nothing realization built can already
        // be in the root.
        locals: transaction.locals.clone(),
    }
}

/// The state the plan settled on for each unit. This leaves `enable`,
/// `disable` and `mask` as independent lists and resolution has already decided
/// which wins, so assembly asks the plan rather than the manifest.
pub fn unit_states(plan: &BuildPlan) -> BTreeMap<String, EnableState> {
    plan.inputs
        .iter()
        .filter_map(|i| match i {
            ResolvedInput::Unit { name, enable, .. } => Some((name.clone(), *enable)),
            _ => None,
        })
        .collect()
}

/// Kernel assembly, steps 1–6.
fn build_kernel(root: &Path, sandbox: &dyn Sandbox) -> Result<kernel::Kernel> {
    let found = kernel::find(root)?;
    kernel::place_vmlinuz(root, &found)?;

    run(sandbox, &kernel::depmod_spec(root, &found))?;
    run(sandbox, &kernel::dracut_spec(root, &found))?;

    let listing = run(sandbox, &kernel::verify_spec(root, &found))?;
    kernel::initramfs_is_bootable(&listing).map_err(tree::shape)?;

    kernel::clear_boot(root)?;
    Ok(found)
}

fn run(sandbox: &dyn Sandbox, spec: &SandboxSpec) -> Result<String> {
    let outcome = sandbox.run(spec)?;
    if !outcome.ok() {
        return Err(tree::shape(format!(
            "`{}` failed with exit status {} during assembly:\n{}",
            spec.command.join(" "),
            outcome.status,
            tail(&outcome.stderr)
        )));
    }
    Ok(outcome.stdout)
}

/// Assembly step 11. The image describes itself, so `kiln status`, `kiln diff` and
/// `kiln why` work on a booted machine whose configuration has since been
/// edited or deleted — which is the normal case when debugging why the
/// generation you rolled back to behaves differently.
/// The last few lines of a failure, not all of it. dracut's stderr on a bad run
/// is thousands of lines and the useful part is at the end.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

fn describe_self(root: &Path, manifest: &Manifest, record: &Record) -> Result<()> {
    // Both halves of step 11. The record says what the image was *made
    // of*; the manifest says what it was *asked to be*, and they answer
    // different questions. `kiln rebuild <gen>` needs the manifest — the record
    // pins a package's checksum but knows nothing about the `[[file]]` targets,
    // the unit states or the scripts that the tree also has to be assembled
    // from — and `kiln show <gen>` needs it to print a configuration that has
    // since been edited or deleted, which calls the normal case.
    let manifest_json = serde_json::to_string_pretty(manifest)
        .map_err(|e| tree::shape(format!("the manifest could not be serialized: {e}")))?;
    for (name, body) in [
        ("record.json", record.to_json()),
        ("manifest.json", manifest_json),
    ] {
        let at = root.join(SELF_DIR).join(name);
        tree::write(&at, &body)?;
        tree::set_mode(&at, 0o644)?;
    }
    Ok(())
}

/// Copy the resolution session's `sync/` into the staging root's database
/// directory. Returns how many database files were copied.
pub fn import_sync_databases(root: &Path, from: &Path) -> Result<usize> {
    let source = from.join("sync");
    let dest = root.join(kiln_alpm::session::DB_PATH).join("sync");
    tree::mkdir(&dest)?;

    let mut copied = 0;
    for entry in tree::entries(&source)? {
        let Some(name) = entry.file_name() else {
            continue;
        };
        if entry.is_file() {
            std::fs::copy(&entry, dest.join(name)).map_err(tree::io("copying", &entry))?;
            copied += 1;
        }
    }
    if copied == 0 {
        return Err(tree::shape(format!(
            "no repository databases in {}: resolution has not refreshed them, and assembly              does no network of its own",
            source.display()
        )));
    }
    Ok(copied)
}

/// Write the shims into the staging root and return their names.
pub fn place_shims(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(SHIM_DIR_IN_ROOT);
    tree::mkdir(&dir)?;
    // The shim script appends to an absolute path, which inside the chroot is
    // inside the staging root.
    let log = root.join(kiln_sandbox::SHIM_LOG.trim_start_matches('/'));
    tree::mkdir(log.parent().expect("the shim log has a directory"))?;

    let mut names = Vec::new();
    for shim in Shim::hostile_to_images() {
        let at = dir.join(&shim.name);
        tree::write(&at, &shim.script())?;
        tree::set_mode(&at, 0o755)?;
        names.push(shim.name);
    }
    Ok(names)
}

/// The shims are build machinery, not image content. A `systemctl` in
/// `/usr/local/bin` that exits 0 would be a very confusing thing to find on a
/// booted system.
pub fn remove_shims(root: &Path, names: &[String]) -> Result<()> {
    for name in names {
        tree::remove(&root.join(SHIM_DIR_IN_ROOT).join(name))?;
    }
    // The whole directory, not just the log: `/run` is a mountpoint and must be
    // empty in the commit, and the contract verifier would rather catch this
    // than libostree.
    let log = Path::new(kiln_sandbox::SHIM_LOG.trim_start_matches('/'));
    tree::remove(&root.join(log.parent().unwrap_or(log)))
}

/// What the shims recorded, one line per call. `kiln build -v` should say
/// `shimmed: systemctl daemon-reload` rather than leaving the user to wonder
/// what a scriptlet tried to do.
pub fn collect_shim_log(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(kiln_sandbox::SHIM_LOG.trim_start_matches('/')))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn absorb(report: &mut Report, alpm: &kiln_alpm::Report) {
    report.installed.extend(alpm.installed.iter().cloned());
    report.installed.sort();
    report.installed.dedup();
    for hook in &alpm.hooks {
        if hooks::is_shadowed(hook) {
            report.hooks_shadowed.push(hook.clone());
        } else {
            report.hooks_fired.push(hook.clone());
        }
    }
}

/// Re-exported for the report: the drain's plan is part of what a build says it
/// did.
pub use drain::Plan as DrainPlan;
