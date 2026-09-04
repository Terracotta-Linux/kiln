//! Recipes in the configuration tree, and out-of-tree kernel modules.
//!
//!
//! Resolution reads a recipe's **declared** metadata and computes its
//! `build_key`; it builds nothing. is emphatic that a check downloads
//! nothing, builds nothing and unpacks nothing, and executing a PKGBUILD to
//! find out what it declares would quietly break that — a `.SRCINFO` is
//! required instead, with a diagnostic that gives the command to produce one.

use kiln_build::key::Ingredients;
use kiln_build::srcinfo::{self, Srcinfo};
use kiln_diag::{Diag, Errors};
use kiln_manifest::{Hash, Manifest};
use std::path::Path;

/// What a recipe contributes to the plan, before the build key is computed.
pub struct Declared {
    /// Config-root-relative, as the manifest wrote it.
    pub path: String,
    pub meta: Srcinfo,
    pub tree: Hash,
}

impl Declared {
    /// the build-time dependency closure, whose exact versions make the
    /// cache correct rather than merely fast.
    pub fn makedepends(&self) -> Vec<String> {
        self.meta
            .makedepends
            .iter()
            .chain(&self.meta.checkdepends)
            .map(|d| kiln_aur::closure::bare_name(d).to_string())
            .collect()
    }

    pub fn ingredients(&self, arch: &str) -> Ingredients {
        Ingredients::new(self.tree.clone(), arch)
    }
}

/// Read every `packages.build` recipe. Reports all the unreadable ones rather
/// than the first.
pub fn read_all(manifest: &Manifest, config_root: &Path, problems: &mut Errors) -> Vec<Declared> {
    let mut out = Vec::new();
    for path in &manifest.packages.build {
        let dir = config_root.join(path);
        // The frontend already proved the directory exists and hashed it.
        let Some(tree) = manifest.local_digests.get(path).cloned() else {
            continue;
        };

        if !dir.join("PKGBUILD").is_file() {
            problems.push(at(
                manifest,
                path,
                Diag::error(
                    "kiln::resolution",
                    format!("`{path}` has no PKGBUILD, so there is nothing to build there"),
                ),
            ));
            continue;
        }

        let text = match std::fs::read_to_string(dir.join(".SRCINFO")) {
            Ok(text) => text,
            Err(_) => {
                // Deliberately not "run makepkg for them". resolution
                // downloads nothing, builds nothing and unpacks nothing —
                // sourcing a PKGBUILD to find out what it declares is running
                // a shell script during what the user was told is a cheap
                // metadata check.
                problems.push(
                    at(
                        manifest,
                        path,
                        Diag::error("kiln::resolution", format!("`{path}` has no .SRCINFO")),
                    )
                    // The command goes last: miette re-wraps help text, and
                    // anything after a pre-formatted line comes out indented
                    // to match it.
                    .help(format!(
                        "Kiln reads what a recipe declares rather than running it, so that \
                         `kiln check` stays a metadata query. Commit one beside the \
                         PKGBUILD:\n\n        cd {path} && makepkg --printsrcinfo > .SRCINFO"
                    )),
                );
                continue;
            }
        };

        match srcinfo::parse(&text, &manifest.image.arch) {
            Ok(meta) => out.push(Declared {
                path: path.clone(),
                meta,
                tree,
            }),
            Err(e) => problems.push(at(
                manifest,
                path,
                Diag::error(
                    "kiln::resolution",
                    format!("could not read `{path}/.SRCINFO`: {e}"),
                ),
            )),
        }
    }
    out
}

/// an out-of-tree module compiled against the exact kernel in the image.
///
/// Kiln synthesizes the recipe rather than asking for a PKGBUILD, so a module
/// is a source directory and nothing else. The build key carries the resolved
/// kernel EVR, which is what makes "rebuild modules when the kernel changes"
/// fall out of the cache instead of being a special case.
pub struct Module {
    pub name: String,
    pub source: String,
    pub tree: Hash,
}

pub fn modules(manifest: &Manifest, problems: &mut Errors) -> Vec<Module> {
    let mut out = Vec::new();
    for (name, module) in &manifest.kernel.out_of_tree {
        let Some(tree) = manifest.local_digests.get(&module.source).cloned() else {
            problems.push(
                Diag::error(
                    "kiln::resolution",
                    format!("`{name}` names a source tree Kiln did not hash"),
                )
                .help("this is a bug in Kiln, not in your configuration"),
            );
            continue;
        };
        out.push(Module {
            name: name.clone(),
            source: module.source.clone(),
            tree,
        });
    }
    out
}

fn at(manifest: &Manifest, path: &str, diag: Diag) -> Diag {
    match crate::diag::origin_of(manifest, "packages.build", path) {
        Some(origin) => diag.label(origin, "declared here"),
        None => diag,
    }
}
