//! `kiln check` — what would change, without building.
//!
//! The `checkupdates` of a Kiln system, except that it covers every input
//! rather than only official packages: your files, your PKGBUILDs, your AUR
//! packages and your configuration all report in the same place, and the fix is
//! always the same single command.
//!
//! The categories match the input taxonomy, so every declared kind of input has
//! a defined place in the report and nothing can change invisibly.

use kiln_record::Record;
use kiln_resolve::{BuildPlan, ResolvedInput};
use std::collections::BTreeMap;

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added {
        name: String,
        to: String,
    },
    Removed {
        name: String,
        from: String,
    },
    Updated {
        name: String,
        from: String,
        to: String,
        /// e.g. `(commit 3f1a9c → 88bd02)` for an AUR package whose version did
        /// not move but whose recipe did.
        note: Option<String>,
    },
    /// Something that must be rebuilt even though its own identity is
    /// unchanged, because an input to its build key moved.
    Rebuild {
        name: String,
        why: String,
    },
}

impl Change {
    pub fn name(&self) -> &str {
        match self {
            Change::Added { name, .. }
            | Change::Removed { name, .. }
            | Change::Updated { name, .. }
            | Change::Rebuild { name, .. } => name,
        }
    }

    fn render(&self) -> String {
        match self {
            Change::Added { name, to } => format!("    {name:<22} {:<12} →  {to}", "—"),
            Change::Removed { name, from } => format!("    {name:<22} {from:<12} →  removed"),
            Change::Updated {
                name,
                from,
                to,
                note,
            } => {
                let line = format!("    {name:<22} {from:<12} →  {to}");
                match note {
                    Some(note) => format!("{line:<58}({note})"),
                    None => line,
                }
            }
            Change::Rebuild { name, why } => {
                // The same note column as an updated AUR package, so a report
                // with both reads as one table rather than two.
                format!("{:<58}({why})", format!("    {name:<22}"))
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Category → changes, in taxonomy order.
    pub categories: Vec<(&'static str, Vec<Change>)>,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.categories.iter().all(|(_, c)| c.is_empty())
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for (category, changes) in &self.categories {
            if changes.is_empty() {
                continue;
            }
            let rebuilds = changes.iter().all(|c| matches!(c, Change::Rebuild { .. }));
            out.push_str(&format!(
                "  {:<22} {} {}\n",
                category,
                changes.len(),
                if rebuilds { "rebuild" } else { "changed" }
            ));
            for change in changes {
                out.push_str(&change.render());
                out.push('\n');
            }
        }
        out
    }
}

/// One side of a comparison, reduced to the maps the categories are compared
/// on.
///
/// Both `kiln check` and `kiln diff` ask the same question — what is different
/// between two package sets — of different pairs: a record against a fresh
/// plan, or a record against another record. Extracting each side once means
/// the *comparison* is written once, and a category added for one command
/// cannot go missing from the other.
#[derive(Debug, Default)]
pub struct Side {
    /// name → epoch:version-rel.
    repo: BTreeMap<String, String>,
    /// name → (evr, aur commit). identity is the commit.
    aur: BTreeMap<String, (String, String)>,
    /// name → (build key, kernel evr for a module).
    built: BTreeMap<String, (String, Option<String>)>,
    local_packages: BTreeMap<String, String>,
    files: BTreeMap<String, String>,
    scripts: BTreeMap<String, String>,
    config_id: String,
    /// The service-account ids, and which sense of them: a plan carries the
    /// seed it will build with, a record carries both that and what the
    /// finished image ended up with. See `diff` and `compare_ids`.
    ids: kiln_resolve::UidMap,
    /// `None` for a plan, which is not a generation yet.
    generation: Option<u64>,
}

impl Side {
    /// A generation, as its own record describes it.
    ///
    /// `ids` is the *captured* map — what the image actually has. That is the
    /// right sense for `kiln diff`, which compares two finished images; `diff`
    /// below overrides it, because a plan has only a seed to offer and
    /// comparing a seed against a capture would report the accounts as changed
    /// on every build.
    pub fn of_record(record: &Record) -> Side {
        Side {
            repo: record
                .repo_packages
                .iter()
                .map(|p| (p.name.clone(), p.evr.clone()))
                .collect(),
            aur: record
                .aur_packages
                .iter()
                .map(|p| (p.name.clone(), (p.evr.clone(), p.aur_commit.clone())))
                .collect(),
            built: record
                .built_packages
                .iter()
                .map(|p| (p.name.clone(), (p.build_key.clone(), p.kernel_evr.clone())))
                .collect(),
            local_packages: record
                .local_packages
                .iter()
                .map(|p| (p.path.clone(), p.sha256.clone()))
                .collect(),
            files: record
                .local_files
                .iter()
                .map(|f| (f.path.clone(), f.blake3.clone()))
                .collect(),
            scripts: record.scripts.clone(),
            config_id: record.config_id.clone(),
            ids: record.next_seed(),
            generation: Some(record.generation),
        }
    }

    /// A plan, as resolution produced it. Nothing here has been built.
    pub fn of_plan(plan: &BuildPlan) -> Side {
        Side {
            repo: collect(plan, |i| match i {
                ResolvedInput::RepoPackage { name, evr, .. } => Some((name.clone(), evr.clone())),
                _ => None,
            }),
            aur: plan
                .inputs
                .iter()
                .filter_map(|i| match i {
                    ResolvedInput::AurPackage {
                        name,
                        evr,
                        aur_commit,
                        ..
                    } => Some((name.clone(), (evr.clone(), aur_commit.clone()))),
                    _ => None,
                })
                .collect(),
            built: plan
                .inputs
                .iter()
                .filter_map(|i| match i {
                    ResolvedInput::BuiltPackage {
                        name, build_key, ..
                    } => Some((name.clone(), (build_key.to_string(), None))),
                    ResolvedInput::KernelModule {
                        name,
                        build_key,
                        kernel_evr,
                        ..
                    } => Some((
                        name.clone(),
                        (build_key.to_string(), Some(kernel_evr.clone())),
                    )),
                    _ => None,
                })
                .collect(),
            local_packages: collect(plan, |i| match i {
                ResolvedInput::FilePackage { path, sha256 } => Some((path.clone(), sha256.clone())),
                _ => None,
            }),
            files: collect(plan, |i| match i {
                ResolvedInput::File {
                    target, content, ..
                } => Some((target.clone(), content.digest().to_string())),
                _ => None,
            }),
            scripts: collect(plan, |i| match i {
                ResolvedInput::BuildScript {
                    name,
                    phase,
                    content,
                } => Some((name.clone(), script_identity(content, phase))),
                _ => None,
            }),
            config_id: plan.config_id.to_string(),
            ids: plan.uid_map.clone(),
            generation: None,
        }
    }
}

/// A script's identity is its text *and* the slot it runs in: moving one
/// from `packages` to `files` changes the tree it sees, and therefore what it
/// produces, even when the bytes are identical. Written once here because the
/// record and the plan both have to spell it the same way, or every build would
/// report every script as changed.
fn script_identity(
    content: &kiln_resolve::ContentRef,
    phase: &kiln_manifest::ScriptPhase,
) -> String {
    format!(
        "{} after {}",
        content.digest(),
        match phase {
            kiln_manifest::ScriptPhase::Packages => "packages",
            kiln_manifest::ScriptPhase::Files => "files",
        }
    )
}

/// Diff a fresh plan against the record of what is deployed.
pub fn diff(record: &Record, plan: &BuildPlan) -> Report {
    let mut was = Side::of_record(record);
    // The one case where two builds of an unchanged configuration
    // legitimately differ: generation 1 has nothing to seed from, so it lets
    // packages allocate ids freely, and generation 2 pins them. Comparing the
    // plan's seed against the record's *capture* would instead report the
    // accounts as changed on every single build, so the record is asked what it
    // seeded with rather than what it ended up with.
    was.ids = record.seeded_with();
    compare_sides(&was, &Side::of_plan(plan))
}

/// Diff two generations against each other. `kiln diff <gen> <gen>`.
pub fn between(from: &Record, to: &Record) -> Report {
    compare_sides(&Side::of_record(from), &Side::of_record(to))
}

/// The comparison itself, over the input taxonomy so that every declared kind
/// of input has a defined place in the report and nothing can change invisibly.
pub fn compare_sides(was: &Side, now: &Side) -> Report {
    let mut report = Report::default();

    report
        .categories
        .push(("repo packages", compare(&was.repo, &now.repo)));

    // an AUR package's identity is its git commit, so a maintainer
    // force-pushing a different PKGBUILD at the same `pkgver` is a change. The
    // version is what a person recognizes, so the line shows both.
    report
        .categories
        .push(("aur", compare_aur(&was.aur, &now.aur)));

    // A build key that moved is a rebuild, and the interesting
    // part is *why* — a recipe that changed and a kernel that moved are very
    // different pieces of news.
    report
        .categories
        .push(("built packages", compare_built(&was.built, &now.built)));

    report.categories.push((
        "local packages",
        compare(&was.local_packages, &now.local_packages),
    ));
    report
        .categories
        .push(("files", compare(&was.files, &now.files)));

    // A script is the one input whose effect is arbitrary, so an edited
    // script has to be named rather than folded into "config_id moved" — which
    // is what the fallback below would otherwise say, about a build that
    // differs for a reason it cannot describe.
    report
        .categories
        .push(("scripts", compare(&was.scripts, &now.scripts)));

    report
        .categories
        .push(("service accounts", compare_ids(was, now)));

    // `config_id` covers the whole merged manifest, so a configuration
    // change that no other category explains still has to be reported —
    // otherwise `kiln check` says "nothing" about a build that would differ.
    if was.config_id != now.config_id && report.is_empty() {
        report.categories.push((
            "configuration",
            vec![Change::Updated {
                name: "config_id".into(),
                from: short(&was.config_id),
                to: short(&now.config_id),
                note: Some("settings changed with no change to any input".into()),
            }],
        ));
    }

    report
}

/// The service accounts one side pins, against the other's.
fn compare_ids(was: &Side, now: &Side) -> Vec<Change> {
    if was.ids == now.ids {
        return Vec::new();
    }
    let newly = now
        .ids
        .groups
        .keys()
        .filter(|n| !was.ids.groups.contains_key(*n))
        .count()
        + now
            .ids
            .users
            .keys()
            .filter(|n| !was.ids.users.contains_key(*n))
            .count();
    let previous = was
        .generation
        .map(|g| format!("generation {g}"))
        .unwrap_or_else(|| "the previous generation".into());
    vec![Change::Rebuild {
        name: "pinned ids".into(),
        why: match (was.ids.is_empty(), newly) {
            (true, n) => format!("{previous} allocated {n} of them freely; this build pins them"),
            (false, 0) => format!("{previous}'s ids changed"),
            (false, n) => format!("{n} allocated since {previous}"),
        },
    }]
}

fn collect<F>(plan: &BuildPlan, f: F) -> BTreeMap<String, String>
where
    F: Fn(&ResolvedInput) -> Option<(String, String)>,
{
    plan.inputs.iter().filter_map(f).collect()
}

fn compare(was: &BTreeMap<String, String>, now: &BTreeMap<String, String>) -> Vec<Change> {
    let mut out = Vec::new();
    for (name, to) in now {
        match was.get(name) {
            None => out.push(Change::Added {
                name: name.clone(),
                to: short(to),
            }),
            Some(from) if from != to => out.push(Change::Updated {
                name: name.clone(),
                from: short(from),
                to: short(to),
                note: None,
            }),
            Some(_) => {}
        }
    }
    for (name, from) in was {
        if !now.contains_key(name) {
            out.push(Change::Removed {
                name: name.clone(),
                from: short(from),
            });
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    out
}

fn compare_aur(
    was: &BTreeMap<String, (String, String)>,
    now: &BTreeMap<String, (String, String)>,
) -> Vec<Change> {
    let mut out = Vec::new();
    for (name, (evr, commit)) in now {
        match was.get(name) {
            None => out.push(Change::Added {
                name: name.clone(),
                to: evr.clone(),
            }),
            Some((old_evr, old_commit)) if old_commit != commit => out.push(Change::Updated {
                name: name.clone(),
                from: old_evr.clone(),
                to: evr.clone(),
                note: Some(format!(
                    "commit {} → {}",
                    short_commit(old_commit),
                    short_commit(commit)
                )),
            }),
            Some(_) => {}
        }
    }
    for (name, (evr, _)) in was {
        if !now.contains_key(name) {
            out.push(Change::Removed {
                name: name.clone(),
                from: evr.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    out
}

fn compare_built(
    was: &BTreeMap<String, (String, Option<String>)>,
    now: &BTreeMap<String, (String, Option<String>)>,
) -> Vec<Change> {
    let mut out = Vec::new();
    for (name, (key, kernel)) in now {
        match was.get(name) {
            None => out.push(Change::Added {
                name: name.clone(),
                to: short(key),
            }),
            Some((old_key, old_kernel)) if old_key != key => out.push(Change::Rebuild {
                name: name.clone(),
                why: match (old_kernel, kernel) {
                    (Some(a), Some(b)) if a != b => format!("kernel {a} → {b}"),
                    _ => "the recipe or a build-time dependency changed".into(),
                },
            }),
            Some(_) => {}
        }
    }
    for name in was.keys() {
        if !now.contains_key(name) {
            out.push(Change::Removed {
                name: name.clone(),
                from: String::new(),
            });
        }
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    out
}

fn short(s: &str) -> String {
    match s.strip_prefix("b3:") {
        Some(hex) => format!("b3:{}", &hex[..8.min(hex.len())]),
        None if s.len() > 16 && s.chars().all(|c| c.is_ascii_hexdigit()) => s[..12].to_string(),
        None => s.to_string(),
    }
}

fn short_commit(s: &str) -> String {
    s.chars().take(6).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_manifest::Hash;
    use kiln_record::{AurEntry, BuiltEntry, LocalFile, RepoEntry, RepoSnapshot};
    use kiln_resolve::{ContentRef, ImageRef, Provenance, UidMap};

    fn record() -> Record {
        Record {
            format: kiln_record::FORMAT,
            plan_id: "b3:7f2a4c1122334455".into(),
            config_id: "b3:11c4de".into(),
            generation: 42,
            built_at: "2026-08-30T19:04:11Z".into(),
            image: "workstation".into(),
            arch: "x86_64".into(),
            repos: RepoSnapshot {
                snapshot: "2026-08-30".into(),
                mirrors: Vec::new(),
            },
            repo_packages: vec![
                entry("linux", "6.19.2-1"),
                entry("mesa", "26.1.4-1"),
                entry("firefox", "145.0-1"),
                entry("vim", "9.1-1"),
            ],
            aur_packages: vec![AurEntry {
                name: "zen-browser-bin".into(),
                evr: "1.16.3".into(),
                aur_commit: "3f1a9c8e".into(),
                pulled_in_by: None,
            }],
            built_packages: vec![BuiltEntry {
                name: "v4l2loopback".into(),
                build_key: "b3:cc41".into(),
                kernel_evr: Some("6.19.2-1".into()),
                sources: Vec::new(),
            }],
            local_packages: Vec::new(),
            local_files: vec![LocalFile {
                path: "files/myapp.conf".into(),
                blake3: "b3:aaaa1111".into(),
            }],
            scripts: Default::default(),
            script_effects: Default::default(),
            uid_map: Default::default(),
            uid_seed: Default::default(),
        }
    }

    fn entry(name: &str, evr: &str) -> RepoEntry {
        RepoEntry {
            name: name.into(),
            evr: evr.into(),
            repo: "extra".into(),
            filename: format!("{name}-{evr}-x86_64.pkg.tar.zst"),
            sha256: "abcd".into(),
        }
    }

    fn plan(inputs: Vec<ResolvedInput>) -> BuildPlan {
        let mut plan = BuildPlan {
            config_id: Hash("b3:11c4de".into()),
            image: ImageRef {
                name: "workstation".into(),
                arch: "x86_64".into(),
            },
            inputs,
            volatile: Vec::new(),
            uid_map: UidMap::new(),
            provenance: Provenance {
                resolved_at: "2026-09-01T00:00:00Z".into(),
                snapshot: "2026-09-01".into(),
                repos: Vec::new(),
                libalpm: "16.0.1".into(),
            },
        };
        plan.canonicalize();
        plan
    }

    fn repo(name: &str, evr: &str) -> ResolvedInput {
        ResolvedInput::RepoPackage {
            name: name.into(),
            evr: evr.into(),
            filename: String::new(),
            sha256: String::new(),
            repo: "extra".into(),
            explicit: true,
        }
    }

    /// The check report, over one of every kind of change the taxonomy can
    /// produce: an upgrade, a removal, an addition, an AUR package whose commit
    /// moved, a module that rebuilds because the kernel did, and a file whose
    /// bytes changed.
    #[test]
    fn the_whole_report() {
        let report = diff(
            &record(),
            &plan(vec![
                repo("linux", "6.19.3-1"),
                repo("mesa", "26.1.5-1"),
                repo("firefox", "145.0-1"),
                repo("ripgrep", "14.1.1-1"),
                ResolvedInput::AurPackage {
                    name: "zen-browser-bin".into(),
                    pkgbase: "zen-browser-bin".into(),
                    evr: "1.17.0".into(),
                    aur_commit: "88bd0244".into(),
                    srcinfo_hash: Hash("b3:ffff".into()),
                    pulled_in_by: None,
                },
                ResolvedInput::KernelModule {
                    name: "v4l2loopback".into(),
                    source: "modules/x".into(),
                    build_key: Hash("b3:dd52".into()),
                    recipe: Hash("b3:ee63".into()),
                    kernel_evr: "6.19.3-1".into(),
                },
                ResolvedInput::File {
                    target: "files/myapp.conf".into(),
                    content: ContentRef::Local {
                        path: "files/myapp.conf".into(),
                        digest: Hash("b3:bbbb2222".into()),
                    },
                    mode: None,
                },
            ]),
        );
        insta::assert_snapshot!(report.render());
    }

    /// identity is the git commit, not the version string. A maintainer
    /// force-pushing a different PKGBUILD at the same `pkgver` is a change, and
    /// a report keyed on the version alone would show nothing at all.
    #[test]
    fn an_aur_package_reports_a_moved_commit_at_an_unchanged_version() {
        let report = diff(
            &record(),
            &plan(vec![ResolvedInput::AurPackage {
                name: "zen-browser-bin".into(),
                pkgbase: "zen-browser-bin".into(),
                evr: "1.16.3".into(),
                aur_commit: "88bd0244".into(),
                srcinfo_hash: Hash("b3:ffff".into()),
                pulled_in_by: None,
            }]),
        );
        let aur = &report
            .categories
            .iter()
            .find(|(c, _)| *c == "aur")
            .unwrap()
            .1;
        assert!(matches!(
            &aur[0],
            Change::Updated { note: Some(n), .. } if n == "commit 3f1a9c → 88bd02"
        ));
    }

    /// bumping the kernel rebuilds every out-of-tree module, and the
    /// report says *why* rather than showing a build key nobody can read.
    #[test]
    fn a_kernel_bump_reports_the_modules_it_rebuilds() {
        let report = diff(
            &record(),
            &plan(vec![ResolvedInput::KernelModule {
                name: "v4l2loopback".into(),
                source: "modules/x".into(),
                build_key: Hash("b3:dd52".into()),
                recipe: Hash("b3:cc41".into()),
                kernel_evr: "6.19.3-1".into(),
            }]),
        );
        let built = &report
            .categories
            .iter()
            .find(|(c, _)| *c == "built packages")
            .unwrap()
            .1;
        assert_eq!(
            built[0],
            Change::Rebuild {
                name: "v4l2loopback".into(),
                why: "kernel 6.19.2-1 → 6.19.3-1".into(),
            }
        );
    }

    /// `config_id` covers the whole merged manifest, so a change that no
    /// input category explains — a `boot.timeout`, a kernel karg — still has to
    /// be reported. Otherwise `kiln check` says "nothing" about a build that
    /// would genuinely differ, which is the one thing it must never do.
    #[test]
    fn a_settings_only_change_is_still_reported() {
        // Every input exactly as the record has it: nothing moved but the
        // configuration itself.
        let record = record();
        let mut inputs: Vec<ResolvedInput> = record
            .repo_packages
            .iter()
            .map(|p| repo(&p.name, &p.evr))
            .collect();
        inputs.push(ResolvedInput::AurPackage {
            name: "zen-browser-bin".into(),
            pkgbase: "zen-browser-bin".into(),
            evr: "1.16.3".into(),
            aur_commit: "3f1a9c8e".into(),
            srcinfo_hash: Hash("b3:ffff".into()),
            pulled_in_by: None,
        });
        inputs.push(ResolvedInput::KernelModule {
            name: "v4l2loopback".into(),
            source: "modules/x".into(),
            build_key: Hash("b3:cc41".into()),
            recipe: Hash("b3:ee63".into()),
            kernel_evr: "6.19.2-1".into(),
        });
        inputs.push(ResolvedInput::File {
            target: "files/myapp.conf".into(),
            content: ContentRef::Local {
                path: "files/myapp.conf".into(),
                digest: Hash("b3:aaaa1111".into()),
            },
            mode: None,
        });

        let mut plan = plan(inputs);
        plan.config_id = Hash("b3:99887766".into());

        let report = diff(&record, &plan);
        assert!(
            !report.is_empty(),
            "a build that would differ must never report nothing"
        );
        insta::assert_snapshot!(report.render());
    }
}
