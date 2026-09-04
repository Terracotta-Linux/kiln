//! Step 6: `[[file]]` entries.
//!
//! One concept covers a config file, a script, a drop-in and a whole tree, and
//! the only thing that varies is where it is allowed to land. That table is the
//! interesting part of this module: it is where a lot of user pain gets
//! prevented, because every wrong answer here is a file that silently is not in
//! the image, or is in it and does nothing.
//!
//! Two rules are worth stating before the code:
//!
//! - **Kiln owns the `/usr/etc` translation.** A user writes `/etc/motd`. This
//!   module writes `etc/motd` in the staging root and normalization moves the
//!   whole directory in step 10, so a configuration file lands in `/usr/etc`
//!   the same way a package's does, and OSTree 3-way-merges it at deploy.
//! - **`/var` is not written, it is seeded.** `/var` does not exist in the
//!   commit at all, so a file targeting it becomes a factory copy plus
//!   a tmpfiles `C` line — a *default*, restored on a machine that has none and
//!   left alone on a machine that has its own.

use crate::tree::{self, Error, Result};
use kiln_manifest::FileEntry;
use std::collections::BTreeMap;
use std::path::Path;

/// Kiln's own tmpfiles fragment for content the *configuration* put under
/// `/var`, `/opt` or `/srv`.
///
/// Deliberately not `kiln-var.conf`, which the drain owns. The two say the same
/// kind of thing about different things, and keeping them apart is what lets
/// `kiln owns /var/lib/myapp/seed.db` answer "your configuration" rather than
/// "some package".
pub const TMPFILES_PATH: &str = "usr/lib/tmpfiles.d/kiln.conf";

/// Where a target is allowed to land, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Straight into the commit, at this staging-root-relative path.
    Direct(String),
    /// Into `/usr/share/factory`, recreated by systemd-tmpfiles at boot. The
    /// string is the image path the `C` line restores.
    Factory { at: String, restores: String },
}

impl Route {
    /// The staging-root-relative path the bytes are written to.
    pub fn at(&self) -> &str {
        match self {
            Route::Direct(at) => at,
            Route::Factory { at, .. } => at,
        }
    }
}

/// One entry that cannot be realized. Collected rather than returned one at a
/// time: this reports every error in a phase, so a config with four impossible
/// targets is four lines of output and one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub target: String,
    pub why: String,
    pub hint: Option<String>,
}

/// Decide where a target goes, or why it cannot go anywhere. Pure — the whole
/// table is readable, and testable, without a filesystem.
pub fn route(target: &str) -> std::result::Result<Route, Refusal> {
    let refuse = |why: &str, hint: Option<&str>| Refusal {
        target: target.to_string(),
        why: why.to_string(),
        hint: hint.map(str::to_string),
    };

    if !target.starts_with('/') {
        return Err(refuse("a target must be an absolute path", None));
    }
    if target.ends_with('/') {
        return Err(refuse(
            "a target directory needs a name for the file inside it",
            None,
        ));
    }
    if target.split('/').any(|c| c == "." || c == "..") {
        return Err(refuse(
            "a target must be normalized: no `.` or `..` components",
            None,
        ));
    }

    let rest = &target[1..];
    let (top, tail) = match rest.split_once('/') {
        Some((top, tail)) if !tail.is_empty() => (top, tail),
        // `/foo` with nothing under it, or a trailing-slash form already
        // rejected above. Nothing Kiln ships lives at the top level.
        _ => {
            return Err(refuse(
                "nothing is written at the top level of the image",
                None,
            ))
        }
    };

    match top {
        // The one path Kiln reserves inside /usr: /usr/etc is the *result* of
        // normalization, not somewhere to write. Accepting it would work by
        // accident today and collide with the /etc move tomorrow.
        "usr" if tail == "etc" || tail.starts_with("etc/") => Err(refuse(
            "/usr/etc is where Kiln puts /etc, not somewhere to write",
            Some(&format!("write `/{tail}` instead")),
        )),
        "usr" => Ok(Route::Direct(format!("usr/{tail}"))),

        // Written to /etc in the staging root; normalization moves the whole
        // directory to /usr/etc in step 10.
        "etc" => Ok(Route::Direct(format!("etc/{tail}"))),

        // Normalization relocates /opt and /srv into /var before draining, so a file
        // targeting them is /var content and takes the same route.
        "var" | "opt" | "srv" => {
            let under = match top {
                "var" => tail.to_string(),
                other => format!("{other}/{tail}"),
            };
            Ok(Route::Factory {
                at: format!("usr/share/factory/var/{under}"),
                restores: format!("/var/{under}"),
            })
        }

        "boot" => Err(refuse(
            "OSTree owns /boot; the bootloader and kernels are placed by Kiln",
            None,
        )),
        "home" | "root" => Err(refuse(
            "home directories are machine state, not image content",
            Some("Kiln does not manage login accounts or their files"),
        )),
        "tmp" | "run" | "proc" | "sys" | "dev" => Err(refuse(
            "this is a runtime filesystem; nothing written here would survive a boot",
            None,
        )),
        other => Err(refuse(
            &format!("/{other} is not a directory the image has"),
            None,
        )),
    }
}

/// Who owns a path in the assembled tree, so that overwriting a package's file
/// can be refused rather than silently done.
///
/// A trait because the answer comes from libalpm's file database, and the
/// routing table above is worth testing without a pacman root.
pub trait Owners {
    fn owner_of(&self, image_path: &str) -> Option<String>;
}

impl Owners for kiln_alpm::Session {
    fn owner_of(&self, image_path: &str) -> Option<String> {
        self.owns(image_path)
    }
}

/// Nothing owns anything. For the first assembly of a tree with no package
/// database, and for tests about routing rather than about conflicts.
pub struct NoOwners;

impl Owners for NoOwners {
    fn owner_of(&self, _: &str) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub target: String,
    /// Staging-root-relative.
    pub at: String,
    pub mode: u32,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct Applied {
    pub placed: Vec<Placed>,
    /// The `C` lines written to `usr/lib/tmpfiles.d/kiln.conf`, in order.
    pub tmpfiles: Vec<String>,
    /// a `/var` target is realized as a default rather than as a file,
    /// which is a surprise worth saying out loud rather than a problem.
    pub notes: Vec<String>,
}

/// Realize every `[[file]]` entry into the staging root.
///
/// `config_root` resolves `source`, which the frontend has already checked
/// resolves inside it (the config root is a security boundary) and already
/// hashed into `config_id`.
pub fn apply(
    root: &Path,
    config_root: &Path,
    entries: &BTreeMap<String, FileEntry>,
    owners: &dyn Owners,
) -> Result<Applied> {
    let mut applied = Applied::default();
    let mut refusals = Vec::new();
    let mut written: BTreeMap<String, String> = BTreeMap::new();

    for (target, entry) in entries {
        let route = match route(target) {
            Ok(route) => route,
            Err(refusal) => {
                refusals.push(refusal);
                continue;
            }
        };

        if let Route::Factory { restores, .. } = &route {
            applied.tmpfiles.push(format!("C {restores} - - - -"));
            applied.notes.push(format!(
                "{target} is under /var, which is not in the image: it becomes a \
                 default restored on a machine that has none"
            ));
        }

        let items = match materialize(config_root, entry, route.at()) {
            Ok(items) => items,
            Err(Error::Shape { what }) => {
                refusals.push(Refusal {
                    target: target.clone(),
                    why: what,
                    hint: None,
                });
                continue;
            }
            Err(other) => return Err(other),
        };

        for item in items {
            // The image path a conflict is really about. For a factory copy it
            // is the /var path the C line restores, not the factory path, which
            // no package has ever heard of.
            let image_path = match &route {
                Route::Direct(_) => format!("/{}", item.at),
                Route::Factory { at, restores } => {
                    let suffix = item.at.strip_prefix(at.as_str()).unwrap_or_default();
                    format!("{restores}{suffix}")
                }
            };

            if let Some(earlier) = written.get(&item.at) {
                refusals.push(Refusal {
                    target: target.clone(),
                    why: format!("{image_path} was already written by `{earlier}`"),
                    hint: Some(
                        "two entries writing the same path is ambiguous, and Kiln does not \
                         pick a winner"
                            .into(),
                    ),
                });
                continue;
            }
            if let Some(owner) = owners.owner_of(&image_path) {
                refusals.push(Refusal {
                    target: target.clone(),
                    why: format!("{image_path} is owned by the package `{owner}`"),
                    hint: Some(format!(
                        "most packages read a drop-in directory; shipping a file there \
                         survives an update to {owner}, and overwriting its own file does not"
                    )),
                });
                continue;
            }

            written.insert(item.at.clone(), target.clone());
            applied.placed.push(item);
        }
    }

    if !refusals.is_empty() {
        return Err(Error::Refused {
            noun: ("[[file]] entry", "[[file]] entries"),
            problems: refusals,
        });
    }

    for item in &applied.placed {
        write_one(root, item, config_root, entries)?;
    }
    if !applied.tmpfiles.is_empty() {
        let mut body = String::from(
            "# Generated by Kiln from [[file]] entries targeting /var, /opt or /srv.\n\
             # Those paths are not in the commit; these lines restore the\n\
             # configured defaults on a machine that does not have them yet.\n",
        );
        for line in &applied.tmpfiles {
            body.push_str(line);
            body.push('\n');
        }
        tree::write(&root.join(TMPFILES_PATH), &body)?;
    }

    Ok(applied)
}

/// Expand one entry into the concrete files it produces: one for a `source`
/// file or inline `content`, many for a `source` tree.
fn materialize(config_root: &Path, entry: &FileEntry, at: &str) -> Result<Vec<Placed>> {
    if let Some(content) = &entry.content {
        return Ok(vec![Placed {
            target: entry.target.clone(),
            at: at.to_string(),
            mode: entry.mode.unwrap_or(0o644),
            bytes: content.len() as u64,
        }]);
    }

    let Some(source) = &entry.source else {
        return Err(tree::shape("neither `source` nor `content` is set"));
    };
    let from = config_root.join(source.trim_end_matches('/'));

    if source.ends_with('/') {
        let mut out = Vec::new();
        collect_tree(&from, &from, at, entry, &mut out)?;
        if out.is_empty() {
            return Err(tree::shape(format!("the source tree `{source}` is empty")));
        }
        return Ok(out);
    }

    let md = from
        .symlink_metadata()
        .map_err(tree::io("reading the source file", &from))?;
    if md.is_dir() {
        return Err(tree::shape(format!(
            "`{source}` is a directory; add a trailing slash to copy it as a tree"
        )));
    }
    Ok(vec![Placed {
        target: entry.target.clone(),
        at: at.to_string(),
        mode: entry.mode.unwrap_or_else(|| default_mode(&md)),
        bytes: md.len(),
    }])
}

fn collect_tree(
    base: &Path,
    at_dir: &Path,
    target_dir: &str,
    entry: &FileEntry,
    out: &mut Vec<Placed>,
) -> Result<()> {
    for path in tree::entries(at_dir)? {
        let md = path
            .symlink_metadata()
            .map_err(tree::io("reading the source tree", &path))?;
        let rel = path.strip_prefix(base).expect("walked from base");
        let at = format!("{target_dir}/{}", rel.display());
        if md.is_dir() {
            collect_tree(base, &path, target_dir, entry, out)?;
        } else {
            out.push(Placed {
                target: entry.target.clone(),
                at,
                // An explicit `mode` on a tree entry applies to every file in
                // it. That is blunt, and it is what "one mode" can mean.
                mode: entry.mode.unwrap_or_else(|| default_mode(&md)),
                bytes: md.len(),
            });
        }
    }
    Ok(())
}

/// An omitted `mode` preserves the source's mode, "masked to 0755/0644".
///
/// Read as *quantized*, not as a bitwise mask: the executable bit decides, and
/// nothing else carries over. A source file that happens to be 0664 because of
/// the builder's umask must not put 0664 in the image — that would make the
/// commit depend on the machine that built it.
fn default_mode(md: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if md.permissions().mode() & 0o111 != 0 {
        0o755
    } else {
        0o644
    }
}

fn write_one(
    root: &Path,
    item: &Placed,
    config_root: &Path,
    entries: &BTreeMap<String, FileEntry>,
) -> Result<()> {
    let entry = entries.get(&item.target).expect("placed from an entry");
    let dest = root.join(&item.at);
    if let Some(parent) = dest.parent() {
        tree::mkdir(parent)?;
    }

    if let Some(content) = &entry.content {
        tree::write(&dest, content)?;
    } else {
        let source = entry.source.as_deref().unwrap_or_default();
        let from = if source.ends_with('/') {
            let base = config_root.join(source.trim_end_matches('/'));
            let route_dir = route(&entry.target).expect("routed").at().to_string();
            base.join(item.at.strip_prefix(&format!("{route_dir}/")).unwrap_or(""))
        } else {
            config_root.join(source)
        };
        let md = from
            .symlink_metadata()
            .map_err(tree::io("reading the source", &from))?;
        if md.file_type().is_symlink() {
            // Copied as a symlink. Its target is a path *in the image*, so
            // following it here would resolve it against the builder's
            // filesystem and copy the wrong bytes — or none.
            let to = std::fs::read_link(&from).map_err(tree::io("reading the link", &from))?;
            tree::symlink(&to.to_string_lossy(), &dest)?;
            return Ok(());
        }
        std::fs::copy(&from, &dest).map_err(tree::io("copying", &from))?;
    }
    tree::set_mode(&dest, item.mode)
}
