//! The AUR, as seen from resolution.
//!
//! `kiln-aur` does the work; this decides *which* packages it is asked about,
//! answers its question about what the official repositories already provide,
//! and turns its answer into plan inputs and diagnostics.

use kiln_aur::closure::Request;
use kiln_diag::{Diag, Errors};
use kiln_manifest::Manifest;

pub struct Resolved {
    pub inputs: Vec<crate::ResolvedInput>,
    pub volatile: Vec<crate::VolatileInput>,
}

pub fn resolve(
    manifest: &Manifest,
    transport: &dyn kiln_aur::Transport,
    session: &kiln_alpm::Session,
    problems: &mut Errors,
) -> Resolved {
    let mut out = Resolved {
        inputs: Vec::new(),
        volatile: Vec::new(),
    };
    if manifest.packages.aur.is_empty() {
        return out;
    }

    let request = Request::new(
        manifest
            .packages
            .aur
            .values()
            .map(|p| (p.name.clone(), p.commit.clone())),
    );

    // recursion stops wherever the official repositories can satisfy a
    // dependency. That is libalpm's question, not the AUR's, which is why
    // `kiln-aur` takes it as a closure and stays free of libalpm entirely.
    //
    // The question is deliberately "do the *repositories* provide this", not
    // "is this in the image". A build-time dependency of an AUR package is
    // usually an official package the image itself does not contain, and
    // asking about the image would send every one of them to the AUR — where
    // they do not exist, so resolution would fail with a baffling message
    // about a package nobody mentioned.
    let in_official_repos = |name: &str| session.provides(name);

    let closure = match kiln_aur::resolve(&request, transport, &in_official_repos) {
        Ok(closure) => closure,
        Err(e) => {
            problems.push(to_diag(manifest, &e));
            return out;
        }
    };

    for package in &closure.packages {
        if package.out_of_date {
            // A warning, not an error: plenty of working packages are flagged,
            // and refusing to build one would be Kiln overruling the user about
            // their own machine.
            problems.push(
                Diag::warning(
                    "kiln::aur",
                    format!("`{}` is flagged out of date in the AUR", package.name),
                )
                .help("it may still build and work; the flag is a note from other users"),
            );
        }
        // The one fact about an AUR package that `realize`'s per-job line
        // cannot carry: it prints the name, pkgbase and commit from the plan,
        // and the plan has no room for a maintainer without changing what
        // `plan_id` hashes. An orphan is exactly the case a trust line exists
        // for, so it is said here, where the RPC's answer is still in hand.
        if package.maintainer.is_none() {
            problems.push(
                Diag::warning(
                    "kiln::aur",
                    format!("`{}` has no maintainer in the AUR", package.name),
                )
                .help(
                    "an orphaned package is nobody's responsibility; read its PKGBUILD \
                     before letting a build run it",
                ),
            );
        }
        out.inputs.push(crate::ResolvedInput::AurPackage {
            name: package.name.clone(),
            pkgbase: package.pkgbase.clone(),
            evr: package.version.clone(),
            aur_commit: package.commit.clone(),
            srcinfo_hash: package.srcinfo_hash.clone(),
            pulled_in_by: package.pulled_in_by.clone(),
        });
    }

    out.volatile = closure
        .volatile
        .iter()
        .map(|(name, reason)| crate::VolatileInput {
            input: name.clone(),
            reason: reason.clone(),
            what: crate::Volatile::AurPackage { name: name.clone() },
        })
        .collect();
    out
}

fn to_diag(manifest: &Manifest, err: &kiln_aur::Error) -> Diag {
    let diag = Diag::error("kiln::resolution", err.to_string());
    match err {
        kiln_aur::Error::NotFound { name, pulled_in_by } => {
            let diag = match crate::diag::origin_of(manifest, "packages.aur", name) {
                Some(origin) => diag.label(origin, "no such package in the AUR"),
                None => diag,
            };
            match pulled_in_by {
                Some(by) => diag.help(format!(
                    "`{by}` lists it as a dependency. Nothing enters a Kiln image \
                     anonymously, so a dependency Kiln cannot find is a failure \
                     rather than something to skip."
                )),
                None => diag.help("check the spelling against aur.archlinux.org"),
            }
        }
        kiln_aur::Error::TooDeep { .. } => diag.help(
            "Kiln caps the AUR dependency chain because a hostile or broken recipe could \
             otherwise make resolution walk forever",
        ),
        _ => diag,
    }
}
