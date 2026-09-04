//! The shipped module library's own rules.
//! one decision per file, a 25-line cap, only profiles may compose, all
//! CI-enforced. A cap that is not enforced is a suggestion.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const LINE_CAP: usize = 25;

fn modules_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("modules")
}

fn all_modules() -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                out.push(p);
            }
        }
    }
    walk(&modules_dir(), &mut out);
    out.sort();
    out
}

#[test]
fn every_module_is_under_the_line_cap() {
    let root = modules_dir();
    let over: Vec<String> = all_modules()
        .into_iter()
        .filter_map(|p| {
            let n = std::fs::read_to_string(&p).unwrap().lines().count();
            (n > LINE_CAP)
                .then(|| format!("  {} — {n} lines", p.strip_prefix(&root).unwrap().display()))
        })
        .collect();
    assert!(
        over.is_empty(),
        "\nmodules over the {LINE_CAP}-line cap:\n{}\n\nA module that needs more than \
         {LINE_CAP} lines is making more than one decision. Split it, or make it a profile.\n",
        over.join("\n")
    );
}

/// only profiles may compose. A hardware or desktop module that includes
/// another is a profile wearing the wrong hat.
#[test]
fn only_profiles_compose() {
    let root = modules_dir();
    let mut bad = Vec::new();
    for p in all_modules() {
        let rel = p.strip_prefix(&root).unwrap().to_path_buf();
        let text = std::fs::read_to_string(&p).unwrap();
        let composes = text.lines().any(|l| l.trim_start().starts_with("include"));
        if composes && !rel.starts_with("profiles") {
            bad.push(format!("  {}", rel.display()));
        }
    }
    assert!(
        bad.is_empty(),
        "\nnon-profile modules that include others:\n{}\n\nOnly `profiles/` may compose.\n",
        bad.join("\n")
    );
}

/// `@kiln/<namespace>/<name>` for a module file.
fn reference(path: &Path) -> String {
    format!(
        "@kiln/{}",
        path.strip_prefix(modules_dir())
            .unwrap()
            .with_extension("")
            .display()
    )
}

/// Load one module the way a user's configuration would: a one-line file that
/// includes it and nothing else.
fn load(reference: &str, at: &Path) -> Result<kiln_config::Frontend, kiln_diag::Errors> {
    std::fs::create_dir_all(at).unwrap();
    std::fs::write(
        at.join("system.toml"),
        format!("kiln = 1\ninclude = [\"{reference}\"]\n"),
    )
    .unwrap();
    kiln_config::load(
        Some(at),
        &kiln_config::Options {
            allow_external_sources: false,
            module_root: Some(modules_dir()),
        },
    )
}

/// Every shipped module must itself be a valid Kiln file, or the library ships
/// errors to users.
#[test]
fn every_module_parses_and_validates() {
    let tmp = tempfile::tempdir().unwrap();
    for p in all_modules() {
        let reference = reference(&p);
        let dir = tmp.path().join(reference.replace(['/', '@'], "_"));
        if let Err(errs) = load(&reference, &dir) {
            panic!(
                "shipped module {reference} does not validate:\n{}",
                kiln_diag::render_all(&errs)
            );
        }
    }
}

/// naming a unit nothing in the image provides is a **hard error** at
/// assembly — for `disable` and `mask` exactly as much as for `enable`. So a
/// module may only name a unit that comes from a package it installs itself, or
/// from `systemd`, which every bootable image has.
///
/// This is not a style rule. It caught two real bugs in one afternoon:
/// `@kiln/net/sshd` enabled `sshd.socket`, which current `openssh` no longer
/// ships, and `@kiln/net/systemd-networkd` disabled `NetworkManager.service`,
/// which would have failed every build that chose networkd — that is, every
/// build that included the module at all.
///
/// It reads the host's pacman **file database**, which is metadata already on
/// disk: no network, and nothing is installed or downloaded. On a machine that
/// has none — a non-Arch host, or one where `pacman -Fy` has never run — it
/// says so and skips, the same way the privileged tests do.
#[test]
fn every_unit_a_module_names_comes_from_a_package_it_installs() {
    // `systemd` is the one package a module may lean on without listing it:
    // an image with no systemd has no init and fails the bootability check
    // long before a unit matters.
    const ALWAYS_PRESENT: &str = "systemd";

    let tmp = tempfile::tempdir().unwrap();

    // Every module's units, gathered first so the file database is asked once
    // rather than once per unit. `pacman -F` decompresses it on every call, and
    // at a second a call that is the difference between a test people run and
    // one they skip.
    let mut named: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    for p in all_modules() {
        let reference = reference(&p);
        let dir = tmp.path().join(reference.replace(['/', '@'], "_"));
        let fe = load(&reference, &dir).expect("every module validates; see the test above");
        let installs: Vec<&str> = fe
            .manifest
            .packages
            .repo
            .iter()
            .map(String::as_str)
            .collect();

        let systemd = &fe.manifest.systemd;
        let units: Vec<String> = systemd
            .enable
            .iter()
            .chain(&systemd.disable)
            .chain(&systemd.mask)
            // A unit the module ships itself needs no package at all.
            .filter(|u| !systemd.units.contains_key(*u))
            .cloned()
            .collect();
        named.push((
            reference,
            installs.into_iter().map(str::to_string).collect(),
            units,
        ));
    }

    let mut wanted: Vec<String> = named
        .iter()
        .flat_map(|(_, _, units)| units.iter().cloned())
        .collect();
    // The probe: a unit that certainly exists, so a wholly empty answer means
    // "no file database here" rather than "the library is broken".
    wanted.push("systemd-timesyncd.service".into());
    wanted.sort();
    wanted.dedup();

    let owners = owners_of(&wanted);
    if owners.is_empty() {
        eprintln!(
            "skipped: no pacman file database on this host — run `pacman -Fy` to check \
             the shipped module library's unit names"
        );
        return;
    }

    let mut wrong = Vec::new();
    for (reference, installs, units) in &named {
        for unit in units {
            match owners.get(unit) {
                None => wrong.push(format!("  {reference}\n    {unit} — no package ships it")),
                Some(owner) if owner != ALWAYS_PRESENT && !installs.contains(owner) => {
                    wrong.push(format!(
                        "  {reference}\n    {unit} — comes from `{owner}`, which this module \
                         does not install"
                    ))
                }
                Some(_) => {}
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "\nunits a shipped module names but no package in that module provides:\n\n{}\n\n\
Kiln makes this a hard error at assembly, so each of these is a module that \
         cannot be built. Add the package that ships the unit, ship the unit with \
         `[[systemd.unit]]`, or stop naming it.\n",
        wrong.join("\n")
    );
}

/// unit name → the package that owns it, from the host's pacman file database.
///
/// One invocation for the whole set. A unit nothing owns is simply absent from
/// the map — `pacman -F` says nothing about it and exits non-zero, which is a
/// fact about that one path and not about the query.
fn owners_of(units: &[String]) -> BTreeMap<String, String> {
    let paths: Vec<String> = units
        .iter()
        .map(|u| format!("usr/lib/systemd/system/{u}"))
        .collect();
    let Ok(out) = std::process::Command::new("pacman")
        .arg("-F")
        .args(&paths)
        .output()
    else {
        return BTreeMap::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        // `usr/lib/systemd/system/gdm.service is owned by extra/gdm 50.2-1`
        .filter_map(|line| {
            let (path, rest) = line.split_once(" is owned by ")?;
            let unit = path.rsplit('/').next()?.to_string();
            let repo_pkg = rest.split_whitespace().next()?;
            Some((unit, repo_pkg.rsplit('/').next()?.to_string()))
        })
        .collect()
}
