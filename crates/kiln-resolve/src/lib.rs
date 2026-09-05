//! `kiln-resolve` — Manifest + live repository metadata → `BuildPlan`.
//!
//!
//! This is the cheap half of the build. It refreshes sync databases, runs the
//! solver, and produces a plan; it downloads no package and unpacks nothing.
//! Keeping realization out of here is what makes `kiln check` possible without
//! building, and it is a boundary worth defending: the moment resolution needs
//! to fetch a tarball to answer a question, `kiln check` stops being cheap and
//! starts being a build.

pub mod aur;
pub mod diag;
pub mod plan;
pub mod recipes;
pub mod time;

use kiln_alpm::{mirrors, Config, RepoSpec, Request, Session, Trust};
use kiln_build::key::Ingredients;
use kiln_diag::{Diag, Errors};
use kiln_manifest::{Manifest, Snapshot};
use std::path::PathBuf;

pub use plan::*;

#[derive(Debug, Clone)]
pub struct Options {
    /// `/var/lib/kiln` normally; a scratch directory under test.
    pub state_dir: PathBuf,
    /// Resolve against whatever metadata is already cached, refreshing nothing.
    /// `kiln check --offline`.
    pub offline: bool,
    /// Replaces the repository set the manifest implies.
    ///
    /// The seam the fixture tests build on, and the only way to resolve against
    /// something that is not Arch. It is not a CLI flag: a user changes
    /// repositories by writing `repos` in their configuration, where the change
    /// is part of `config_id` and therefore part of the image's identity.
    pub repos: Option<Vec<RepoSpec>>,
    /// The UID seed replayed from the previous generation. Empty on a
    /// machine with no commits yet, which is the first build.
    pub uid_map: UidMap,
}

impl Options {
    pub fn new(state_dir: impl Into<PathBuf>) -> Options {
        Options {
            state_dir: state_dir.into(),
            offline: false,
            repos: None,
            uid_map: UidMap::new(),
        }
    }

    pub fn offline(mut self, yes: bool) -> Options {
        self.offline = yes;
        self
    }

    pub fn with_repos(mut self, repos: Vec<RepoSpec>) -> Options {
        self.repos = Some(repos);
        self
    }

    pub fn with_uid_map(mut self, uid_map: UidMap) -> Options {
        self.uid_map = uid_map;
        self
    }
}

/// The repositories a manifest implies, in priority order.
///
/// `core` and `extra` always, then whatever `repos.extra` declares. The servers
/// come from `repos.mirrors`, or the geo mirror when that is empty, or
/// the Archive when `repos.snapshot` names a date.
pub fn repositories(manifest: &Manifest) -> Vec<RepoSpec> {
    let arch = &manifest.image.arch;

    let templates: Vec<String> = match &manifest.repos.snapshot {
        // A pinned snapshot replaces the mirrors rather than adding to them:
        // mixing a live mirror with an archived one would resolve half the
        // image from each, which is precisely the partial upgrade exists
        // to prevent.
        Snapshot::Date(d) => mirrors::archive(d).into_iter().collect(),
        Snapshot::Latest if manifest.repos.mirrors.is_empty() => vec![mirrors::GEO.to_string()],
        Snapshot::Latest => manifest.repos.mirrors.iter().cloned().collect(),
    };

    let mut repos: Vec<RepoSpec> = mirrors::OFFICIAL
        .iter()
        .map(|name| {
            RepoSpec::new(
                *name,
                templates
                    .iter()
                    .map(|t| mirrors::expand(t, name, arch))
                    .collect(),
                Trust::Required,
            )
        })
        .collect();

    for (name, extra) in &manifest.repos.extra {
        repos.push(RepoSpec::new(
            name,
            vec![mirrors::expand(&extra.server, name, arch)],
            // a repository declared without a key is one Kiln cannot
            // verify. It is accepted — a user may genuinely have a local
            // unsigned repository — but it is named as unsigned rather than
            // silently trusted.
            match extra.key {
                Some(_) => Trust::Required,
                None => Trust::Unsigned,
            },
        ));
    }
    repos
}

/// The outside world resolution is allowed to touch.
///
/// Only the AUR is here. Everything else it needs — repository metadata — goes
/// through libalpm, which already has a downloader; and everything it must
/// *not* do, like running a PKGBUILD to see what it declares, is absent by
/// construction rather than by discipline.
///
/// A struct rather than a field on `Options` because it borrows, and a trait
/// object rather than a generic because there is exactly one of it and the
/// tests want to hand over a recorded transport without a turbofish.
pub struct Inputs<'a> {
    pub aur: &'a dyn kiln_aur::Transport,
}

impl<'a> Inputs<'a> {
    pub fn new(aur: &'a dyn kiln_aur::Transport) -> Inputs<'a> {
        Inputs { aur }
    }
}

/// Resolve a manifest into a plan.
///
/// `config_root` is a parameter rather than a field on `Options` because
/// resolution genuinely cannot proceed without it — `packages.file` and
/// `packages.build` name paths relative to it — and a required fact hidden
/// behind a builder is a fact somebody eventually forgets to supply.
pub fn resolve(
    manifest: &Manifest,
    config_root: &std::path::Path,
    opts: &Options,
    inputs: &Inputs<'_>,
) -> Result<BuildPlan, Errors> {
    let repos = opts.repos.clone().unwrap_or_else(|| repositories(manifest));

    let config =
        Config::for_resolution(&opts.state_dir, &manifest.image.arch).with_repos(repos.clone());

    let mut session = Session::open(config).map_err(|e| err(manifest, &e, &[]))?;

    if !opts.offline {
        // the official repositories are trusted *via GPG signatures*, and
        // a `SigLevel` that requires them requires keys to check them against.
        // Without this the first refresh on a fresh machine fails with libalpm's
        // "failed to retrieve some files", which says nothing about keys at all.
        //
        // Only when something actually needs it: a configuration whose
        // repositories are all unsigned — the test fixture, a local mirror —
        // has no use for a keyring and should not spend a minute building one.
        if repos.iter().any(|r| r.trust == Trust::Required) {
            if let Some(gpgdir) = &session.config().gpgdir {
                kiln_alpm::keyring::Keyring::at(gpgdir)
                    .ensure()
                    .map_err(|e| err(manifest, &e, &[]))?;
            }
        }
        session.refresh(false).map_err(|e| err(manifest, &e, &[]))?;
    }

    let request = Request::new(manifest.packages.repo.iter().cloned())
        .excluding(manifest.packages.exclude.iter().cloned());

    let solution = session.solve(&request).map_err(|e| {
        // The suggestion pool is every package name the repositories hold. It
        // is only built on the failing path, because it is a few thousand
        // strings and the succeeding path has no use for it.
        let known = session.package_names();
        err(manifest, &e, &known)
    })?;

    bootability(manifest, &solution)?;

    // everything wrong in one pass. A configuration with a stale
    // checksum, an unreadable recipe and a missing AUR package should report
    // three problems, not one per build.
    let mut problems = Errors::new();
    let locals = local_packages(manifest, config_root, &mut problems);
    let declared = recipes::read_all(manifest, config_root, &mut problems);
    let modules = recipes::modules(manifest, &mut problems);
    let aur = aur::resolve(manifest, inputs.aur, &session, &mut problems);

    // the exact versions of every build-time dependency. This is the
    // ingredient that makes the build cache correct rather than merely fast,
    // and it is the reason recipes are resolved *after* the image's own
    // solution — both come from the same repository snapshot.
    let build_inputs = build_keys(
        manifest,
        &mut session,
        &declared,
        &modules,
        &solution,
        &mut problems,
    );
    // One report for the whole phase. A recipe that failed to parse is
    // simply absent from `declared`, so the steps above compose without either
    // of them having to check whether the other found anything.
    problems.into_result(())?;

    let mut resolved: Vec<ResolvedInput> = solution
        .packages
        .iter()
        .map(|p| ResolvedInput::RepoPackage {
            name: p.name.clone(),
            evr: p.version.clone(),
            filename: p.filename.clone(),
            // Kiln records a checksum for every package so a rebuild can be
            // satisfied after mirrors have moved on. An unsigned local
            // repository can omit it; the empty string keeps the encoding
            // total rather than making every consumer handle an Option.
            sha256: p.sha256.clone().unwrap_or_default(),
            repo: p.repo.clone(),
            explicit: p.explicit,
        })
        .collect();

    resolved.extend(locals);
    resolved.extend(aur.inputs);
    resolved.extend(build_inputs);
    resolved.extend(files(manifest));
    resolved.extend(units(manifest));
    resolved.extend(scripts(manifest));

    let mut volatile = aur.volatile;
    volatile.extend(declared.iter().flat_map(volatile_sources));

    let now = time::now();
    let mut plan = BuildPlan {
        config_id: manifest.config_id(),
        image: ImageRef {
            name: manifest.image.name.clone(),
            arch: manifest.image.arch.clone(),
        },
        inputs: resolved,
        // reported separately and excluded from `plan_id`, because
        // guessing here would make `kiln check` untrustworthy and an
        // untrustworthy check is worse than none.
        volatile,
        uid_map: opts.uid_map.clone(),
        provenance: Provenance {
            resolved_at: time::rfc3339(now),
            // recorded even in rolling mode, because that single field
            // is what makes a past image reconstructible without anyone having
            // pinned anything in advance.
            snapshot: match &manifest.repos.snapshot {
                Snapshot::Date(d) => d.clone(),
                Snapshot::Latest => time::date(now),
            },
            repos: repos
                .iter()
                .map(|r| (r.name.clone(), r.servers.clone()))
                .collect(),
            libalpm: kiln_alpm::libalpm_version().to_string(),
        },
    };
    plan.canonicalize();
    Ok(plan)
}

/// an image with no kernel, or no init, fails rather than producing an
/// artifact that silently does not boot.
///
/// Checked here rather than at assembly. The design placed it at assembly, but
/// it is a fact about the *solution*, and the whole point of the plan/realize
/// split is that a failure which can be known cheaply should be reported
/// cheaply — this way `kiln check` catches it too, without building anything.
fn bootability(manifest: &Manifest, solution: &kiln_alpm::Solution) -> Result<(), Errors> {
    let mut errs = Errors::new();
    let suggest_minimal = "`@kiln/profiles/minimal` is the one-line answer — it stands for \
                           the handful of packages any bootable image needs";

    if solution.get(&manifest.kernel.package).is_none() {
        let mut d = Diag::error(
            "kiln::resolution",
            format!(
                "no kernel in this image: nothing installs `{}`",
                manifest.kernel.package
            ),
        );
        if let Some(o) = manifest.origins.get("kernel.package") {
            d = d.label(&o.effective, "named here");
        }
        errs.push(d.help(format!(
            "add `{}` to `packages.repo`. {suggest_minimal}",
            manifest.kernel.package
        )));
    }

    // Something has to be PID 1. On Arch that is `systemd`, reached through
    // whichever package provides `init` — asking for the virtual name rather
    // than for `systemd` keeps the check honest about what actually matters.
    if solution.providers_of("init").is_empty() && solution.get("systemd").is_none() {
        let mut d = Diag::error(
            "kiln::resolution",
            "no init in this image: nothing provides `init`, so it cannot boot",
        );
        // There is no one package to blame, so the label goes on the package
        // set itself — which is at least the place the fix belongs. An empty
        // configuration sets nothing at all, and then the help carries it alone.
        if let Some(o) = manifest.origins.get("packages.repo") {
            d = d.label(&o.effective, "this package set has no init");
        }
        errs.push(d.help(suggest_minimal));
    }

    errs.into_result(())
}

/// The reason recipes resolve *after* the image's own solution: the
/// build-time dependency closure has to come from the same repository snapshot
/// as the image, or a package is built against a toolchain the image does not
/// contain.
///
/// > Including `makedep_evrs` in the key is what makes the cache *correct*
/// > rather than merely fast — a package built against `gcc 15.1` is not the
/// > same artifact as one built against `gcc 15.2`.
fn build_keys(
    manifest: &Manifest,
    session: &mut Session,
    declared: &[recipes::Declared],
    modules: &[recipes::Module],
    solution: &kiln_alpm::Solution,
    problems: &mut Errors,
) -> Vec<ResolvedInput> {
    let arch = &manifest.image.arch;
    let mut out = Vec::new();

    for recipe in declared {
        let Some(makedeps) = closure_evrs(session, &recipe.makedepends(), &recipe.path, problems)
        else {
            continue;
        };
        let ingredients = recipe
            .ingredients(arch)
            .with_sources(source_pins(recipe))
            .with_makedeps(makedeps);
        out.push(ResolvedInput::BuiltPackage {
            // a recipe can be a split package. The first `pkgname` is the
            // one the plan is keyed on; the transaction gets all of them from
            // the artifacts the build produces.
            name: recipe.meta.pkgnames.first().cloned().unwrap_or_default(),
            path: recipe.path.clone(),
            build_key: ingredients.build_key(),
            recipe: recipe.tree.clone(),
            sources: ingredients.sources,
        });
    }

    // An out-of-tree module needs the kernel's headers to build and the
    // kernel's EVR in its key.
    if !modules.is_empty() {
        let kernel = &manifest.kernel.package;
        let Some(kernel_evr) = solution.get(kernel).map(|p| p.version.clone()) else {
            // `bootability` already reported a missing kernel; saying it twice
            // would be noise.
            return out;
        };
        let headers = format!("{kernel}-headers");
        for module in modules {
            let Some(makedeps) = closure_evrs(
                session,
                std::slice::from_ref(&headers),
                &module.source,
                problems,
            ) else {
                continue;
            };
            let ingredients = Ingredients::new(module.tree.clone(), arch)
                .with_makedeps(makedeps)
                .against_kernel(&kernel_evr);
            out.push(ResolvedInput::KernelModule {
                name: module.name.clone(),
                source: module.source.clone(),
                build_key: ingredients.build_key(),
                recipe: module.tree.clone(),
                kernel_evr: kernel_evr.clone(),
            });
        }
    }
    out
}

/// `name-evr` for a build-time dependency closure, resolved against the same
/// repositories as the image.
fn closure_evrs(
    session: &mut Session,
    wanted: &[String],
    recipe: &str,
    problems: &mut Errors,
) -> Option<Vec<String>> {
    // the build root holds `base-devel` plus the resolved makedepends,
    // so base-devel's own closure is part of what a package was built against
    // and belongs in the key.
    //
    // `base-devel` is a real package in current Arch. It was a package *group*
    // until 2022, which `find_satisfier` would not resolve — worth knowing
    // before debugging a "no package named base-devel" against an old snapshot.
    let mut names: Vec<String> = wanted.to_vec();
    names.push("base-devel".to_string());

    match session.solve(&Request::new(names)) {
        Ok(closure) => Some(
            closure
                .packages
                .iter()
                .map(|p| format!("{}-{}", p.name, p.version))
                .collect(),
        ),
        Err(e) => {
            problems.push(
                Diag::error(
                    "kiln::resolution",
                    format!("`{recipe}` cannot be built: {e}"),
                )
                .help(
                    "its `makedepends` must resolve against the same repositories as the                      image, so that the toolchain it is built against is the one recorded                      in its build key",
                ),
            );
            None
        }
    }
}

fn source_pins(recipe: &recipes::Declared) -> Vec<SourcePin> {
    let mut out: Vec<SourcePin> = recipe
        .meta
        .sources
        .iter()
        .filter_map(|s| {
            Some(SourcePin {
                url: s.spec.clone(),
                sha256: s.sha256.clone()?,
            })
        })
        .collect();
    out.sort();
    out
}

/// a `SKIP` checksum or a VCS source means the contents are only known
/// after fetching, so the recipe is reported rather than guessed at.
fn volatile_sources(recipe: &recipes::Declared) -> Vec<VolatileInput> {
    recipe
        .meta
        .volatile_sources()
        .iter()
        .map(|s| VolatileInput {
            input: format!("{}: {}", recipe.path, s.spec),
            reason: if s.sha256.is_none() {
                "its checksum is SKIP, so its contents are only known after fetching".into()
            } else {
                "it is a VCS source, so its revision is only known after fetching".into()
            },
            what: Volatile::RecipeSource {
                recipe: recipe.path.clone(),
                spec: s.spec.clone(),
            },
        })
        .collect()
}

/// A `.pkg.tar.zst` sitting in the configuration tree.
///
/// The checksum is **verified here**, against the bytes on disk, rather than
/// merely recorded. "an unhashed local blob that silently changes is
/// precisely the class of drift `kiln check` exists to catch; making the hash
/// optional makes the guarantee optional" — and recording a hash without
/// checking it makes it optional in a way that is harder to notice.
///
/// The frontend already hashed the file with blake3 into `local_digests`, so
/// `config_id` moves when it changes. That is a different job: `local_digests`
/// notices the change, and this says whether the change was *authorized*.
fn local_packages(
    manifest: &Manifest,
    config_root: &std::path::Path,
    problems: &mut Errors,
) -> Vec<ResolvedInput> {
    let mut out = Vec::new();
    for (path, package) in &manifest.packages.file {
        if kiln_manifest::is_url(path) {
            // Nothing to hash yet: the bytes are not on this machine. The
            // frontend already folded `path` + `sha256` into `config_id`
            // (there is no local digest to fold in instead), so this is
            // trusted through to realization, which downloads and verifies
            // it the same way the AUR closure fetches a pinned commit
            // without resolution touching its contents.
            out.push(ResolvedInput::FilePackage {
                path: path.clone(),
                sha256: package.sha256.clone(),
            });
            continue;
        }
        let at = config_root.join(path);
        let Some(actual) = kiln_alpm::sha256(&at) else {
            // The frontend already proved the path resolves, so reaching here
            // means it stopped being readable between then and now.
            problems.push(
                label(
                    manifest,
                    "packages.file",
                    path,
                    Diag::error(
                        "kiln::resolution",
                        format!("could not read `{path}` to check its checksum"),
                    ),
                    "declared here",
                )
                .help(format!("looked for {}", at.display())),
            );
            continue;
        };
        if actual != package.sha256 {
            problems.push(
                label(
                    manifest,
                    "packages.file",
                    path,
                    Diag::error(
                        "kiln::resolution",
                        format!("`{path}` is not the file its `sha256` describes"),
                    ),
                    "declared here",
                )
                // One flowing sentence: miette re-wraps help text, so embedded
                // line breaks and indentation come out mangled. The hash is
                // still the thing to paste over the old one.
                .help(format!(
                    "the file on disk hashes to sha256 = \"{actual}\" — update the line \
                     if that change was intended; if it was not, something replaced a \
                     package Kiln was about to install"
                )),
            );
            continue;
        }
        out.push(ResolvedInput::FilePackage {
            path: path.clone(),
            sha256: package.sha256.clone(),
        });
    }
    out
}

/// Attach the span of one element of a list-valued key, when the manifest has
/// one. See `Manifest::item_origins`.
fn label(manifest: &Manifest, list: &str, item: &str, diag: Diag, text: &str) -> Diag {
    match diag::origin_of(manifest, list, item) {
        Some(origin) => diag.label(origin, text),
        None => diag,
    }
}

/// files and executables are one concept. The plan carries their content
/// *identity*, not their bytes — assembly reads the bytes, and `kiln check`
/// only needs to know whether they changed.
fn files(manifest: &Manifest) -> Vec<ResolvedInput> {
    manifest
        .files
        .values()
        .filter_map(|f| {
            Some(ResolvedInput::File {
                target: f.target.clone(),
                content: content_ref(manifest, f.source.as_deref(), f.content.as_deref())?,
                mode: f.mode,
            })
        })
        .collect()
}

/// Nothing to resolve — the frontend already hashed a script's `source`
/// into `local_digests` and an inline `content` hashes here — so this is a
/// translation, not a resolution step. It earns its place by putting the script
/// *by name* into the plan, which is what lets `kiln check` say
/// `scripts: 20-locale changed` instead of falling back to `config_id`.
fn scripts(manifest: &Manifest) -> Vec<ResolvedInput> {
    manifest
        .scripts
        .values()
        .filter_map(|s| {
            Some(ResolvedInput::BuildScript {
                name: s.name.clone(),
                phase: s.after,
                content: content_ref(manifest, s.source.as_deref(), s.content.as_deref())?,
            })
        })
        .collect()
}

fn units(manifest: &Manifest) -> Vec<ResolvedInput> {
    let s = &manifest.systemd;
    let mut out: Vec<ResolvedInput> = s
        .units
        .values()
        .filter_map(|u| {
            Some(ResolvedInput::Unit {
                name: u.name.clone(),
                content: content_ref(manifest, u.source.as_deref(), u.content.as_deref())?,
                enable: state_of(manifest, &u.name, u.enable),
            })
        })
        .collect();

    // Enabling, disabling or masking a unit a *package* ships is a real input
    // even though no file comes with it: the image differs, so `plan_id` must.
    // Such a unit has no content, which is exactly what distinguishes it.
    for name in s.enable.iter().chain(&s.disable).chain(&s.mask) {
        if s.units.contains_key(name) {
            continue;
        }
        out.push(ResolvedInput::Unit {
            name: name.clone(),
            content: ContentRef::Inline {
                digest: kiln_manifest::Hash::of(b""),
            },
            enable: state_of(manifest, name, false),
        });
    }
    out
}

/// Kiln leaves `enable`/`disable`/`mask` as three independent lists, so a unit
/// can appear in more than one. Masking is the strongest statement and wins;
/// disabling beats enabling. Resolving it here rather than at assembly means
/// the plan says what the image will do, not what the config said.
fn state_of(manifest: &Manifest, name: &str, inline_enable: bool) -> EnableState {
    let s = &manifest.systemd;
    if s.mask.contains(name) {
        EnableState::Masked
    } else if s.disable.contains(name) {
        EnableState::Disabled
    } else if inline_enable || s.enable.contains(name) {
        EnableState::Enabled
    } else {
        EnableState::Unset
    }
}

fn content_ref(
    manifest: &Manifest,
    source: Option<&str>,
    content: Option<&str>,
) -> Option<ContentRef> {
    match (source, content) {
        (Some(path), _) => manifest
            .local_digests
            .get(path)
            .map(|digest| ContentRef::Local {
                path: path.to_string(),
                digest: digest.clone(),
            }),
        (None, Some(text)) => Some(ContentRef::Inline {
            digest: kiln_manifest::Hash::of(text.as_bytes()),
        }),
        // The semantic phase already rejected an entry with neither; there is
        // nothing left to report and nothing to plan.
        (None, None) => None,
    }
}

fn err(manifest: &Manifest, e: &kiln_alpm::Error, known: &[String]) -> Errors {
    diag::one(diag::to_diag(manifest, e, known))
}
