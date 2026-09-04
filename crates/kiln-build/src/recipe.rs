//! A recipe: a PKGBUILD directory, and what it declares.
//!
//! Reading a recipe's metadata **never needs the network**, and prefers not to
//! execute anything. `.SRCINFO` is the declared form and is used when present;
//! otherwise `makepkg --printsrcinfo` runs in a sandbox with `CLONE_NEWNET` and
//! no interfaces, because sourcing a PKGBUILD is running a stranger's bash.

use crate::srcinfo::{self, Srcinfo};
use kiln_manifest::Hash;
use kiln_sandbox::{Bind, Sandbox, SandboxSpec, SandboxUser};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Recipe {
    /// Where the PKGBUILD directory lives on disk.
    pub dir: PathBuf,
    /// Config-root-relative, as the manifest wrote it — the name a diagnostic
    /// should use and the key into `local_digests`.
    pub name: String,
    /// blake3 of the directory, VCS metadata excluded.
    pub tree: Hash,
    pub meta: Srcinfo,
}

impl Recipe {
    /// Read a recipe, using `.SRCINFO` if the directory ships one.
    ///
    /// `sandbox` is only touched on the fallback path, so a recipe with a
    /// `.SRCINFO` — which every AUR clone has — costs one file read.
    pub fn read(
        dir: &Path,
        name: impl Into<String>,
        tree: Hash,
        arch: &str,
        sandbox: &dyn Sandbox,
    ) -> Result<Recipe, Error> {
        let name = name.into();
        if !dir.join("PKGBUILD").is_file() {
            return Err(Error::NotARecipe {
                dir: dir.to_path_buf(),
            });
        }

        let text = match std::fs::read_to_string(dir.join(".SRCINFO")) {
            Ok(text) => text,
            Err(_) => generate_srcinfo(dir, sandbox)?,
        };
        let meta = srcinfo::parse(&text, arch).map_err(|source| Error::Srcinfo {
            recipe: name.clone(),
            source,
        })?;

        Ok(Recipe {
            dir: dir.to_path_buf(),
            name,
            tree,
            meta,
        })
    }

    /// The sources `makepkg` will have to fetch — everything that is not
    /// already a file in the recipe directory.
    pub fn remote_sources(&self) -> Vec<&crate::srcinfo::Source> {
        self.meta.sources.iter().filter(|s| !s.is_local()).collect()
    }

    /// what cannot be resolved without fetching, in words a user can act
    /// on. Empty for the ordinary case.
    pub fn volatile_reasons(&self) -> Vec<String> {
        self.meta
            .volatile_sources()
            .iter()
            .map(|s| {
                let what = if s.sha256.is_none() {
                    "its checksum is SKIP"
                } else {
                    "it is a VCS source"
                };
                format!(
                    "{}: {what}, so its contents are only known after fetching",
                    s.spec
                )
            })
            .collect()
    }
}

/// `makepkg --printsrcinfo`, in a sandbox.
///
/// This sources the PKGBUILD, which is arbitrary bash. It runs with no network
/// and as an unprivileged user — `makepkg` refuses to run as root anyway, which
/// for once is the behaviour we want.
fn generate_srcinfo(dir: &Path, sandbox: &dyn Sandbox) -> Result<String, Error> {
    let spec = SandboxSpec::in_root("/", ["makepkg".to_string(), "--printsrcinfo".to_string()])
        .with_bind(Bind::ro(dir, "/recipe"))
        .with_user(SandboxUser::Unprivileged {
            uid: 1000,
            gid: 1000,
        });
    let spec = SandboxSpec {
        workdir: Some(PathBuf::from("/recipe")),
        ..spec
    };

    let outcome = sandbox.run(&spec).map_err(|source| Error::Generate {
        dir: dir.to_path_buf(),
        source,
    })?;
    Ok(outcome.stdout)
}

#[derive(Debug)]
pub enum Error {
    NotARecipe {
        dir: PathBuf,
    },
    Srcinfo {
        recipe: String,
        source: srcinfo::Error,
    },
    Generate {
        dir: PathBuf,
        source: kiln_sandbox::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotARecipe { dir } => write!(
                f,
                "{} has no PKGBUILD, so there is nothing to build there",
                dir.display()
            ),
            Error::Srcinfo { recipe, source } => {
                write!(f, "could not read the metadata of `{recipe}`: {source}")
            }
            Error::Generate { dir, source } => write!(
                f,
                "could not read the metadata of {}: {source}\n\
                 The directory ships no .SRCINFO, so Kiln ran `makepkg --printsrcinfo` \
                 to produce one.",
                dir.display()
            ),
        }
    }
}

impl std::error::Error for Error {}
