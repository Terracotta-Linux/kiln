//! `kiln check --deep`.
//!
//! *"Some inputs genuinely cannot be resolved without fetching: a VCS package
//! whose `pkgver()` runs upstream code, or a `source=()` entry with `SKIP`
//! checksums. Kiln does not pretend otherwise. Such inputs are excluded from
//! `plan_id`, listed separately, and resolvable with `--deep`."*
//!
//! Ordinary resolution is metadata-only: a syncdb refresh, an AUR RPC call, a
//! `git ls-remote`. None of that can say what a `SKIP` source contains or what
//! `pkgver()` will print, because both answers are downstream of bytes nobody
//! has downloaded. `--deep` downloads them.
//!
//! **It is still resolution, not realization.** What runs is phase 1 of the
//! two-phase build — `makepkg --verifysource`, which fetches and checks
//! sources and runs `pkgver()` for VCS ones, and no other build code. No build
//! root is assembled, nothing is compiled, and the network is on for exactly
//! this step and no other. That is why `--deep` is a flag on `check` and not a
//! quiet half of `build`.
//!
//! **What it does not do is change `plan_id`.** excludes volatile inputs
//! from the identity unconditionally, and `--deep` does not smuggle them back
//! in: a `plan_id` that sometimes covered a VCS revision and sometimes did not
//! would mean two builds of one configuration disagreeing about their own
//! identity depending on which flag was typed. What `--deep` produces is a
//! *report* — precise answers to the questions `kiln check` otherwise has to
//! decline.

use crate::pipeline::Context;
use kiln_build::{Builder, Recipe};
use kiln_diag::ExitCode;
use kiln_manifest::{Hash, Manifest};
use kiln_resolve::{BuildPlan, ResolvedInput, Volatile};
use kiln_sandbox::{Bubblewrap, Sandbox};
use std::collections::BTreeMap;
use std::path::Path;

/// What one volatile input turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The input, named as `kiln check` names it.
    pub input: String,
    /// What it resolved to: a version, a revision, a checksum.
    pub value: String,
    /// Why that is the answer, in words — `pkgver()`, a fetched checksum.
    pub how: String,
}

/// A volatile input that could not be resolved even by fetching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub input: String,
    pub why: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub resolved: Vec<Resolved>,
    pub unresolved: Vec<Unresolved>,
}

impl Report {
    pub fn render(&self) -> String {
        let mut out = String::new();
        for r in &self.resolved {
            out.push_str(&format!("  {}\n    {} ({})\n", r.input, r.value, r.how));
        }
        for u in &self.unresolved {
            out.push_str(&format!(
                "  {}\n    could not be resolved: {}\n",
                u.input, u.why
            ));
        }
        out
    }
}

/// Fetch every volatile input the plan reported and say what it is.
///
/// Errors are collected rather than returned: a source server that is down
/// makes one input unresolvable, and answering precisely about the other four
/// is better than answering about none. The exit code still reflects it —
/// see the caller.
pub fn resolve(
    plan: &BuildPlan,
    manifest: &Manifest,
    ctx: &Context,
    transport: &dyn kiln_aur::Transport,
) -> Report {
    let mut report = Report::default();
    if plan.volatile.is_empty() {
        return report;
    }

    let builder = Builder::new(&ctx.state);
    let sandbox = Bubblewrap::new(ctx.state.join("build/deep-sandbox"));
    let scratch = ctx.state.join("build/deep");
    let _ = std::fs::remove_dir_all(&scratch);

    // One fetch per *recipe*, not per source: `makepkg --verifysource` fetches
    // every source the recipe names, so a recipe with three volatile entries
    // costs one download pass and not three.
    let mut by_recipe: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for v in &plan.volatile {
        match &v.what {
            Volatile::RecipeSource { recipe, spec } => by_recipe
                .entry(recipe.clone())
                .or_default()
                .push(spec.clone()),
            // Resolving this means running `pkgver()`, which means
            // having the PKGBUILD — and an AUR PKGBUILD is not in the
            // configuration tree, it is in a git repository. So `--deep` clones
            // it, at the exact commit the plan resolved, and runs phase 1
            // against it: the same `makepkg --verifysource` a build would run,
            // and nothing else. That is still resolution, not realization —
            // no build root is assembled and nothing is compiled.
            Volatile::AurPackage { name } => match aur_recipe(plan, name) {
                Some((pkgbase, commit)) => {
                    match resolve_aur(
                        name, pkgbase, commit, manifest, &builder, &scratch, &sandbox, transport,
                    ) {
                        Ok(found) => report.resolved.extend(found),
                        Err(why) => report.unresolved.push(Unresolved {
                            input: name.clone(),
                            why,
                        }),
                    }
                }
                // A volatile entry with no input beside it. Reported rather
                // than guessed at, which is the whole point of `--deep`.
                None => report.unresolved.push(Unresolved {
                    input: name.clone(),
                    why: "the plan lists it as volatile but carries no AUR commit for it, so \
                          there is no revision to fetch"
                        .into(),
                }),
            },
        }
    }

    for (path, specs) in by_recipe {
        match resolve_recipe(&path, &specs, manifest, ctx, &builder, &scratch, &sandbox) {
            Ok(found) => report.resolved.extend(found),
            Err(why) => report.unresolved.push(Unresolved {
                input: path.clone(),
                why,
            }),
        }
    }

    report.resolved.sort_by(|a, b| a.input.cmp(&b.input));
    report.unresolved.sort_by(|a, b| a.input.cmp(&b.input));
    report
}

/// The `pkgbase` and commit the plan recorded for an AUR package, which is
/// where its recipe lives and which version of it to read.
fn aur_recipe<'p>(plan: &'p BuildPlan, name: &str) -> Option<(&'p str, &'p str)> {
    plan.inputs.iter().find_map(|i| match i {
        ResolvedInput::AurPackage {
            name: n,
            pkgbase,
            aur_commit,
            ..
        } if n == name => Some((pkgbase.as_str(), aur_commit.as_str())),
        _ => None,
    })
}

/// Clone one AUR package at its resolved commit and run phase 1 against it.
///
/// The answer being looked for is `pkgver()`'s, which `makepkg --verifysource`
/// computes for a VCS source and writes back into the PKGBUILD. Nothing is
/// compiled and no build root is assembled — is a *report*, not a build.
#[allow(clippy::too_many_arguments)]
fn resolve_aur(
    name: &str,
    pkgbase: &str,
    commit: &str,
    manifest: &Manifest,
    builder: &Builder,
    scratch: &Path,
    sandbox: &dyn Sandbox,
    transport: &dyn kiln_aur::Transport,
) -> Result<Vec<Resolved>, String> {
    let work = scratch.join(format!("aur_{name}"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    transport
        .clone_at(&kiln_aur::repository(pkgbase), commit, &work)
        .map_err(|e| format!("cloning the recipe: {e}"))?;

    // The clone is the recipe's identity, so it is what the tree hash is
    // built from — the configuration tree has never seen this directory and
    // `local_digests` knows nothing about it.
    let tree = Hash::of(format!("aur:{pkgbase}@{commit}").as_bytes());
    let arch = &manifest.image.arch;
    let before = Recipe::read(&work, name, tree.clone(), arch, sandbox)
        .map_err(|e| format!("reading the recipe: {e}"))?;

    let outcome = sandbox
        .run(&builder.fetch_spec(&before))
        .map_err(|e| format!("fetching the sources failed: {e}"))?;
    if !outcome.ok() {
        return Err(format!(
            "`makepkg --verifysource` failed:\n{}",
            tail(&outcome.stderr, 12)
        ));
    }

    let after = Recipe::read(&work, name, tree, arch, sandbox)
        .map_err(|e| format!("reading the recipe: {e}"))?;
    if after.meta.evr() == before.meta.evr() {
        // Not an error: a package flagged volatile by the `-git` suffix
        // heuristic may simply have a static `pkgver`, and saying the
        // version is what the recipe already declared is a precise answer.
        return Ok(vec![Resolved {
            input: name.to_string(),
            value: after.meta.evr(),
            how: format!(
                "unchanged by pkgver() at commit {}",
                &commit[..commit.len().min(7)]
            ),
        }]);
    }
    Ok(vec![Resolved {
        input: format!("{name}: pkgver()"),
        value: after.meta.evr(),
        how: format!(
            "computed by pkgver() after fetching, at commit {}",
            &commit[..commit.len().min(7)]
        ),
    }])
}

/// Fetch one recipe's sources and read back what the volatile ones are.
fn resolve_recipe(
    path: &str,
    specs: &[String],
    manifest: &Manifest,
    ctx: &Context,
    builder: &Builder,
    scratch: &Path,
    sandbox: &dyn Sandbox,
) -> Result<Vec<Resolved>, String> {
    let source = ctx.config_root.join(path);
    // A *copy*, because `makepkg` writes to the recipe directory: for a VCS
    // source it rewrites the `pkgver=` line in place with what `pkgver()`
    // printed. That is exactly the answer being looked for — and the
    // configuration tree is not somewhere Kiln writes, so it happens
    // here instead.
    let work = scratch.join(path.replace('/', "_"));
    let _ = std::fs::remove_dir_all(&work);
    copy_tree(&source, &work)?;

    let before = read_recipe(&work, path, manifest, sandbox)?;
    let spec = builder.fetch_spec(&Recipe {
        dir: work.clone(),
        ..before.clone()
    });
    let outcome = sandbox
        .run(&spec)
        .map_err(|e| format!("fetching the sources failed: {e}"))?;
    if !outcome.ok() {
        return Err(format!(
            "`makepkg --verifysource` failed:\n{}",
            tail(&outcome.stderr, 12)
        ));
    }

    // Re-read after the fetch. For a VCS source this is where `pkgver()`'s
    // answer appears, because makepkg has rewritten the recipe.
    let after = read_recipe(&work, path, manifest, sandbox)?;

    let mut out = Vec::new();
    if after.meta.pkgver != before.meta.pkgver {
        out.push(Resolved {
            input: format!("{path}: pkgver()"),
            value: format!("{}-{}", after.meta.pkgver, after.meta.pkgrel),
            how: "computed by pkgver() after fetching".into(),
        });
    }

    for spec in specs {
        let entry = after
            .meta
            .sources
            .iter()
            .find(|s| &s.spec == spec)
            .ok_or_else(|| format!("`{spec}` is no longer a source of this recipe"))?;
        let file = builder.source_cache.join(entry.filename());
        out.push(match sha256(&file) {
            Some(sum) => Resolved {
                input: format!("{path}: {spec}"),
                value: sum,
                how: "sha256 of the bytes that were fetched".into(),
            },
            // A VCS source lands as a checkout, not a file, and its identity is
            // the revision rather than a checksum of a directory.
            None => match revision(&file) {
                Some(rev) => Resolved {
                    input: format!("{path}: {spec}"),
                    value: rev,
                    how: "the revision the source is checked out at".into(),
                },
                None => Resolved {
                    input: format!("{path}: {spec}"),
                    value: "fetched".into(),
                    how: "no checksum or revision could be read from it".into(),
                },
            },
        });
    }
    Ok(out)
}

/// Read the recipe as it stands right now.
///
/// Called twice per recipe — before the fetch and after it — because for a VCS
/// source the difference between the two *is* the answer: `makepkg` rewrites
/// the `pkgver=` line with what `pkgver()` printed.
fn read_recipe(
    dir: &Path,
    path: &str,
    manifest: &Manifest,
    sandbox: &dyn Sandbox,
) -> Result<Recipe, String> {
    // The digest the frontend computed for the *original* directory. It is the
    // recipe's identity for the build cache and is not affected by makepkg
    // rewriting a version in this copy, which is why it is carried in rather
    // than recomputed here.
    let tree = manifest
        .local_digests
        .get(path)
        .cloned()
        .unwrap_or_else(|| kiln_manifest::Hash::of(path.as_bytes()));
    Recipe::read(dir, path, tree, &manifest.image.arch, sandbox)
        .map_err(|e| format!("reading the recipe: {e}"))
}

/// The sha256 of a fetched file, or `None` when the path is not a file — a VCS
/// source is a checkout.
fn sha256(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let out = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

/// The revision a VCS checkout is at. Only git is read: it is what the AUR and
/// almost every `source=()` entry use, and reporting "fetched" for the rest is
/// honest where guessing would not be.
fn revision(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
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
    if !out.status.success() {
        return Err(format!(
            "copying {} : {}",
            from.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// What the caller does with the answer. `kiln check` exits 10 when
/// something would change, and `--deep` does not get its own code — a deep
/// check that found a moved revision found a change.
pub fn exit_code(report: &Report) -> Option<ExitCode> {
    (!report.unresolved.is_empty()).then_some(ExitCode::Resolution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_aur::Recorded;
    use kiln_manifest::Hash as ManifestHash;
    use kiln_resolve::{ImageRef, Provenance, UidMap, VolatileInput};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kiln-deep-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plan(volatile: Vec<VolatileInput>) -> BuildPlan {
        BuildPlan {
            config_id: kiln_manifest::Hash("b3:test".into()),
            image: ImageRef {
                name: "test".into(),
                arch: "x86_64".into(),
            },
            inputs: Vec::new(),
            volatile,
            uid_map: UidMap::new(),
            provenance: Provenance {
                resolved_at: "2026-09-01T00:00:00Z".into(),
                snapshot: "2026-09-01".into(),
                repos: Vec::new(),
                libalpm: "0".into(),
            },
        }
    }

    /// A plan with nothing volatile does no work and reaches for no network.
    /// `--deep` on an ordinary configuration must cost nothing.
    #[test]
    fn nothing_volatile_means_nothing_to_fetch() {
        let dir = scratch("empty");
        let ctx = Context {
            sysroot: dir.clone(),
            state: dir.join("state"),
            config_root: dir.join("config"),
            verbose: false,
        };
        let report = resolve(
            &plan(Vec::new()),
            &Manifest::default(),
            &ctx,
            &Recorded::new(),
        );
        assert!(report.resolved.is_empty());
        assert!(report.unresolved.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// This is the whole point, at the one place it would be tempting to cheat.
    ///
    /// The recipe cannot be fetched — the AUR is unreachable, or the commit is
    /// gone — so `--deep` reports the input as **unresolved**. What it must
    /// never do is fall back to the version the RPC happened to report, which
    /// is precisely the number `pkgver()` exists to overrule. An untrustworthy
    /// check is worse than no check.
    #[test]
    fn an_aur_package_that_cannot_be_fetched_says_so_rather_than_guessing() {
        let dir = scratch("aur");
        let ctx = Context {
            sysroot: dir.clone(),
            state: dir.join("state"),
            config_root: dir.join("config"),
            verbose: false,
        };
        let mut plan = plan(vec![VolatileInput {
            input: "zen-git".into(),
            reason: "pkgver() runs upstream code".into(),
            what: Volatile::AurPackage {
                name: "zen-git".into(),
            },
        }]);
        plan.inputs.push(ResolvedInput::AurPackage {
            name: "zen-git".into(),
            pkgbase: "zen-git".into(),
            evr: "1.0.r5.gabc-1".into(),
            aur_commit: "3f1a9c8e00000000000000000000000000000000".into(),
            srcinfo_hash: ManifestHash("b3:aa01".into()),
            pulled_in_by: None,
        });

        // A transport that knows the package but has no recipe to hand over.
        let report = resolve(&plan, &Manifest::default(), &ctx, &Recorded::new());

        assert!(report.resolved.is_empty(), "nothing may be invented");
        assert_eq!(report.unresolved.len(), 1);
        assert_eq!(report.unresolved[0].input, "zen-git");
        assert!(
            report.unresolved[0].why.contains("cloning"),
            "{}",
            report.unresolved[0].why
        );
        assert!(
            !report.unresolved[0].why.contains("1.0.r5"),
            "the RPC's version must not leak into the answer: {}",
            report.unresolved[0].why
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A volatile entry whose package is not in the plan has no commit to
    /// clone. Saying that is an answer; picking a revision would not be.
    #[test]
    fn a_volatile_aur_package_with_no_input_beside_it_is_reported_not_guessed() {
        let dir = scratch("aur-orphan");
        let ctx = Context {
            sysroot: dir.clone(),
            state: dir.join("state"),
            config_root: dir.join("config"),
            verbose: false,
        };
        let report = resolve(
            &plan(vec![VolatileInput {
                input: "zen-git".into(),
                reason: "pkgver() runs upstream code".into(),
                what: Volatile::AurPackage {
                    name: "zen-git".into(),
                },
            }]),
            &Manifest::default(),
            &ctx,
            &Recorded::new(),
        );
        assert!(report.resolved.is_empty());
        assert_eq!(report.unresolved.len(), 1);
        assert!(
            report.unresolved[0].why.contains("no AUR commit"),
            "{}",
            report.unresolved[0].why
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unresolved input must not let `kiln check` claim to be up to date.
    #[test]
    fn an_unresolved_input_is_a_resolution_failure_not_a_clean_bill() {
        let mut report = Report::default();
        assert_eq!(exit_code(&report), None);
        report.unresolved.push(Unresolved {
            input: "foo-git".into(),
            why: "upstream is unreachable".into(),
        });
        assert_eq!(exit_code(&report), Some(ExitCode::Resolution));
    }

    /// A fetched file is identified by its content, which is exactly what a
    /// `SKIP` checksum was refusing to state.
    #[test]
    fn a_fetched_file_resolves_to_its_checksum() {
        let dir = scratch("sha");
        let file = dir.join("payload.tar.gz");
        std::fs::write(&file, b"contents\n").unwrap();
        let sum = sha256(&file).expect("a file has a checksum");
        // sha256 of "contents\n", so the test fails if the wrong thing is
        // hashed rather than merely if hashing stops working.
        assert_eq!(
            sum, "bfe5ed57e6e323555b379c660aa8d35b70c2f8f07cf03ad6747266495ac13be0",
            "got {sum}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A VCS source is a checkout, not a file: it has no checksum and its
    /// identity is the revision. Reporting "no checksum" for one would be
    /// reporting a failure for the normal case.
    #[test]
    fn a_vcs_checkout_resolves_to_its_revision() {
        let dir = scratch("git");
        let repo = dir.join("upstream");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", &repo.to_string_lossy()])
                .args(args)
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "kiln@localhost"]);
        git(&["config", "user.name", "kiln"]);
        std::fs::write(repo.join("file"), "x\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "one"]);

        let head = git(&["rev-parse", "HEAD"]);
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();

        assert_eq!(revision(&repo).as_deref(), Some(head.as_str()));
        assert_eq!(
            revision(&dir.join("file")),
            None,
            "a file is not a checkout"
        );
        assert!(sha256(&repo).is_none(), "a directory has no checksum");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_names_both_what_resolved_and_what_did_not() {
        let report = Report {
            resolved: vec![Resolved {
                input: "recipes/tool: src.tar.gz".into(),
                value: "abc123".into(),
                how: "sha256 of the bytes that were fetched".into(),
            }],
            unresolved: vec![Unresolved {
                input: "foo-git".into(),
                why: "upstream is unreachable".into(),
            }],
        };
        let text = report.render();
        assert!(text.contains("recipes/tool: src.tar.gz"), "{text}");
        assert!(text.contains("abc123"), "{text}");
        assert!(text.contains("could not be resolved"), "{text}");
    }
}
