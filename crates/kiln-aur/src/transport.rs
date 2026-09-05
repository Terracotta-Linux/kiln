//! Where the network actually happens — and the seam that keeps it out of the
//! tests. *AUR uses recorded HTTP fixtures.*
//!
//! Three operations, all trivial, all isolated behind a trait: fetch a URL, ask
//! a git remote for a ref, and — once a build is actually happening — take a
//! copy of the recipe at a known commit. Everything else interesting —
//! batching, the dependency closure, cycle detection, volatile marking — is on
//! the other side of this boundary and is tested without touching any of them.
//!
//! The third is deliberately here and not in a builder. *building is
//! exactly the PKGBUILD path; there is no separate AUR builder.* Cloning
//! is not building, it is the last piece of *fetching* — and fetching is what
//! this file is.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

pub trait Transport {
    /// GET, returning the body.
    fn get(&self, url: &str) -> Result<String, Error>;

    /// The object id `HEAD` points at in an AUR package's git repository.
    ///
    /// **identity is the AUR git commit**, not the version string. This is
    /// what makes "the maintainer force-pushed a different PKGBUILD with the
    /// same pkgver" a detected change rather than an invisible one.
    fn head_of(&self, repository: &str) -> Result<String, Error>;

    /// Put the recipe at `commit` into `into`, which must not already exist.
    ///
    /// Realization only, never resolution: this promises a check downloads
    /// nothing and unpacks nothing, and `head_of` above is the whole of what
    /// resolution is allowed to ask a git remote.
    ///
    /// The commit is checked out **by object id**, never by ref. A pin exists
    /// so that HEAD moving does not matter, and a clone that resolved
    /// `HEAD` a second time would build something other than what the plan
    /// says — silently, and only when a maintainer happened to push between
    /// the two.
    fn clone_at(&self, repository: &str, commit: &str, into: &Path) -> Result<(), Error>;

    /// GET a URL's raw bytes into `dest`, which must not already exist.
    ///
    /// Separate from `get`: a `packages.file` URL is a `.pkg.tar.zst`, and
    /// `get` decodes its response as UTF-8 text, which would corrupt it.
    /// Realization only, for the same reason `clone_at` is — resolution
    /// carries the URL and its declared `sha256` through untouched.
    fn download(&self, url: &str, dest: &Path) -> Result<(), Error>;
}

/// The real one.
pub struct Network {
    pub agent: ureq::Agent,
}

impl Default for Network {
    fn default() -> Network {
        Network {
            agent: ureq::Agent::new_with_defaults(),
        }
    }
}

impl Transport for Network {
    fn get(&self, url: &str) -> Result<String, Error> {
        self.agent
            .get(url)
            .call()
            .map_err(|e| Error::Http {
                url: url.to_string(),
                why: e.to_string(),
            })?
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Http {
                url: url.to_string(),
                why: e.to_string(),
            })
    }

    fn head_of(&self, repository: &str) -> Result<String, Error> {
        // `git ls-remote` rather than a clone: resolution is metadata-only,
        // and cloning to learn one object id would make `kiln check`
        // as expensive as a build.
        let out = std::process::Command::new("git")
            .args(["ls-remote", repository, "HEAD"])
            .output()
            .map_err(|e| Error::Git {
                repository: repository.to_string(),
                why: format!("{e} (is `git` installed?)"),
            })?;
        if !out.status.success() {
            return Err(Error::Git {
                repository: repository.to_string(),
                why: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        parse_ls_remote(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| Error::Git {
            repository: repository.to_string(),
            why: "the remote reported no HEAD".into(),
        })
    }

    fn clone_at(&self, repository: &str, commit: &str, into: &Path) -> Result<(), Error> {
        let git = |args: &[&str]| -> Result<(), Error> {
            let out = std::process::Command::new("git")
                .args(args)
                .output()
                .map_err(|e| Error::Git {
                    repository: repository.to_string(),
                    why: format!("{e} (is `git` installed?)"),
                })?;
            if out.status.success() {
                return Ok(());
            }
            Err(Error::Git {
                repository: repository.to_string(),
                why: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        };

        let at = into.to_string_lossy().into_owned();
        // A full clone rather than `--depth 1`: the commit being asked for is
        // very often not the tip — a pin, or a HEAD that moved while the plan
        // was being resolved — and a shallow clone cannot check out a commit it
        // did not fetch. AUR repositories are a PKGBUILD and a .SRCINFO, so the
        // whole history is measured in kilobytes.
        git(&["clone", "--quiet", repository, &at])?;
        git(&["-C", &at, "checkout", "--quiet", "--detach", commit])
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), Error> {
        let mut response = self.agent.get(url).call().map_err(|e| Error::Http {
            url: url.to_string(),
            why: e.to_string(),
        })?;
        let mut file = std::fs::File::create(dest).map_err(|e| Error::Http {
            url: url.to_string(),
            why: format!("could not create {}: {e}", dest.display()),
        })?;
        std::io::copy(&mut response.body_mut().as_reader(), &mut file).map_err(|e| {
            Error::Http {
                url: url.to_string(),
                why: e.to_string(),
            }
        })?;
        Ok(())
    }
}

/// `<oid>\tHEAD` — the first field of the first line.
pub fn parse_ls_remote(output: &str) -> Option<String> {
    let oid = output.lines().next()?.split_whitespace().next()?;
    // A 40-character hex sha1, or 64 for sha256 repositories. Checking rather
    // than trusting means a proxy's HTML error page cannot become a commit id
    // that ends up in a build record.
    let plausible =
        (oid.len() == 40 || oid.len() == 64) && oid.bytes().all(|b| b.is_ascii_hexdigit());
    plausible.then(|| oid.to_string())
}

/// A recorded transport — the "recorded HTTP fixtures" mechanism, and the only thing the
/// tests ever use.
#[derive(Debug, Default)]
pub struct Recorded {
    pub bodies: BTreeMap<String, String>,
    pub heads: BTreeMap<String, String>,
    /// repository → a directory on disk that stands in for its clone.
    pub recipes: BTreeMap<String, std::path::PathBuf>,
    /// url → the bytes `download` hands back, standing in for a `.pkg.tar.zst`
    /// fetched over the network.
    pub blobs: BTreeMap<String, Vec<u8>>,
    /// Every URL asked for, in order. This promises the RPC is *batched*, and
    /// this is how a test proves one request was made rather than forty.
    pub requests: std::cell::RefCell<Vec<String>>,
}

impl Recorded {
    pub fn new() -> Recorded {
        Recorded::default()
    }

    /// Answer any RPC query with this body, whatever the arguments.
    pub fn with_rpc(mut self, body: impl Into<String>) -> Recorded {
        self.bodies.insert("*".into(), body.into());
        self
    }

    pub fn with_head(mut self, pkgbase: &str, oid: &str) -> Recorded {
        self.heads.insert(
            format!("https://aur.archlinux.org/{pkgbase}.git"),
            oid.to_string(),
        );
        self
    }

    /// A directory to hand back as the clone of `pkgbase`'s repository. What a
    /// test needs in order to exercise realization without the AUR.
    pub fn with_recipe(mut self, pkgbase: &str, dir: impl Into<std::path::PathBuf>) -> Recorded {
        self.recipes.insert(
            format!("https://aur.archlinux.org/{pkgbase}.git"),
            dir.into(),
        );
        self
    }

    pub fn request_count(&self) -> usize {
        self.requests.borrow().len()
    }

    /// A blob to hand back as the download of `url`.
    pub fn with_blob(mut self, url: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Recorded {
        self.blobs.insert(url.into(), bytes.into());
        self
    }
}

impl Transport for Recorded {
    fn get(&self, url: &str) -> Result<String, Error> {
        self.requests.borrow_mut().push(url.to_string());
        self.bodies
            .get(url)
            .or_else(|| self.bodies.get("*"))
            .cloned()
            .ok_or_else(|| Error::Http {
                url: url.to_string(),
                why: "no recorded response".into(),
            })
    }

    fn head_of(&self, repository: &str) -> Result<String, Error> {
        self.heads
            .get(repository)
            .cloned()
            .ok_or_else(|| Error::Git {
                repository: repository.to_string(),
                why: "no recorded HEAD".into(),
            })
    }

    fn clone_at(&self, repository: &str, _commit: &str, into: &Path) -> Result<(), Error> {
        let from = self.recipes.get(repository).ok_or_else(|| Error::Git {
            repository: repository.to_string(),
            why: "no recorded recipe".into(),
        })?;
        let out = std::process::Command::new("cp")
            .arg("-a")
            .arg(from)
            .arg(into)
            .output()
            .map_err(|e| Error::Git {
                repository: repository.to_string(),
                why: e.to_string(),
            })?;
        out.status
            .success()
            .then_some(())
            .ok_or_else(|| Error::Git {
                repository: repository.to_string(),
                why: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), Error> {
        self.requests.borrow_mut().push(url.to_string());
        let bytes = self.blobs.get(url).ok_or_else(|| Error::Http {
            url: url.to_string(),
            why: "no recorded blob".into(),
        })?;
        std::fs::write(dest, bytes).map_err(|e| Error::Http {
            url: url.to_string(),
            why: e.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Http { url: String, why: String },
    Git { repository: String, why: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http { url, why } => write!(f, "could not reach {url}: {why}"),
            Error::Git { repository, why } => {
                write!(f, "could not read {repository}: {why}")
            }
        }
    }
}

impl std::error::Error for Error {}
