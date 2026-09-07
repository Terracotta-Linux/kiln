//! Committing an assembled tree.

use crate::generation::{self, Metadata};
use crate::{Error, Result};
use kiln_manifest::Manifest;
use kiln_record::Record;
use kiln_resolve::BuildPlan;
use ostree::gio;
use ostree::prelude::*;
use ostree::{
    ObjectType, Repo, RepoCommitFilterResult, RepoCommitModifier, RepoCommitModifierFlags, RepoMode,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Paths that must not be in a commit, checked at the moment of commit.
///
/// Normalization already emptied `/var` and `/boot` and the
/// contract verifier already said so. This is the third check, and it is not
/// redundant: it is the only one that runs on the bytes actually being
/// committed, and the cost of being wrong is a deployment that will not boot.
const REJECTED: &[&str] = &["/var/", "/boot/"];

#[derive(Debug, Clone)]
pub struct CommitOptions {
    /// The OSTree repository. `<sysroot>/ostree/repo` in a real system.
    pub repo: PathBuf,
    /// The generation this commit is, from `next_generation`.
    ///
    /// Passed in rather than computed here, because the tree being committed
    /// already contains a record that names it (step 11) and the two copies
    /// must be the same bytes. Computing it twice is how they came to disagree:
    /// a commit whose metadata said 4 while its own `/usr/lib/kiln/record.json`
    /// said 1, which would have made `kiln rebuild` reconstruct the wrong
    /// generation and `kiln diff` compare the wrong pair.
    pub generation: u64,
    /// Hostname, for `kiln.built-by`.
    pub built_by: String,
    /// A one-line subject. Shown by `ostree log` and nothing else — there is no
    /// `kiln log` command, and nothing parses this.
    pub subject: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Committed {
    pub checksum: String,
    /// libostree's checksum over the commit's *content* — the root tree and its
    /// file objects — with the parent, the timestamp and the metadata excluded.
    ///
    /// This, not `checksum`, is what "did two builds produce the same image"
    /// means. A commit checksum covers the parent, so a rebuild of generation 4
    /// parented on generation 9 has a different commit checksum from the
    /// original however byte-identical the tree is — and comparing those would
    /// report every rebuild as a determinism failure.
    pub content_checksum: String,
    pub generation: u64,
    /// `kiln/<image>/<arch>`.
    pub reference: String,
    pub parent: Option<String>,
}

/// Commit `root` onto `kiln/<image>/<arch>`, parented on whatever that ref
/// points at.
///
/// The generation is `parent + 1`, read from the parent commit's own metadata
/// rather than from a counter anywhere else — the number has to survive a
/// machine that was rolled back, reimaged, or had `/var/lib/kiln` deleted.
pub fn commit(
    root: &Path,
    plan: &BuildPlan,
    record: &Record,
    manifest: &Manifest,
    opts: &CommitOptions,
) -> Result<Committed> {
    let repo = open_or_create(&opts.repo)?;
    let reference = plan.image.ostree_ref();

    let parent = match repo.resolve_rev(&reference, true) {
        Ok(Some(rev)) => Some(rev.to_string()),
        _ => None,
    };
    let generation = opts.generation;
    let metadata = Metadata::of(plan, generation, record, manifest, &opts.built_by);
    let variant = metadata.to_variant()?;

    repo.prepare_transaction(gio::Cancellable::NONE)
        .map_err(Error::of("starting the commit transaction"))?;

    let outcome = write(&repo, root, parent.as_deref(), &variant, opts);
    let checksum = match outcome {
        Ok(checksum) => checksum,
        Err(e) => {
            let _ = repo.abort_transaction(gio::Cancellable::NONE);
            return Err(e);
        }
    };

    repo.transaction_set_ref(None, &reference, Some(&checksum));
    repo.commit_transaction(gio::Cancellable::NONE)
        .map_err(Error::of("committing the transaction"))?;

    Ok(Committed {
        content_checksum: content_checksum(&repo, &checksum)?,
        checksum,
        generation,
        reference,
        parent,
    })
}

/// libostree's checksum over a commit's content alone. See `Committed`: this is
/// the one that answers "is this the same image", because the commit checksum
/// covers the parent and every rebuild has a different one.
pub fn content_checksum(repo: &Repo, checksum: &str) -> Result<String> {
    let (commit, _) = repo
        .load_commit(checksum)
        .map_err(Error::of("reading a commit"))?;
    ostree::commit_get_content_checksum(&commit)
        .map(|c| c.to_string())
        .ok_or_else(|| Error::NotOurs {
            checksum: checksum.to_string(),
            why: "has no content checksum, which every OSTree commit has".into(),
        })
}

fn write(
    repo: &Repo,
    root: &Path,
    parent: Option<&str>,
    metadata: &glib::Variant,
    opts: &CommitOptions,
) -> Result<String> {
    // SKIP_XATTRS stays *off*: file capabilities live in xattrs, and an image
    // whose `ping` lost `cap_net_raw` is one where a handful of programs fail
    // in ways that look nothing like a missing xattr.
    let modifier = RepoCommitModifier::new(
        RepoCommitModifierFlags::NONE,
        Some(Box::new(
            |_repo: &Repo, path: &str, _info: &gio::FileInfo| {
                if REJECTED.iter().any(|bad| path.starts_with(bad)) {
                    RepoCommitFilterResult::Skip
                } else {
                    RepoCommitFilterResult::Allow
                }
            },
        )),
    );

    let mtree = ostree::MutableTree::new();
    let dir = gio::File::for_path(root);
    repo.write_directory_to_mtree(&dir, &mtree, Some(&modifier), gio::Cancellable::NONE)
        .map_err(Error::of("reading the assembled tree"))?;
    let tree = repo
        .write_mtree(&mtree, gio::Cancellable::NONE)
        .map_err(Error::of("writing the tree"))?;
    let tree = tree
        .downcast::<ostree::RepoFile>()
        .expect("write_mtree returns a RepoFile");

    let subject = opts.subject.clone().unwrap_or_else(|| "kiln".to_string());
    // `write_commit_with_time` with 0 rather than `write_commit`: the commit
    // timestamp is wall clock, and two builds of the same plan would otherwise
    // differ in it. The date that matters is `kiln.built-at`, which comes from
    // the plan's provenance and is recorded, not measured here.
    let checksum = repo
        .write_commit_with_time(
            parent,
            Some(&subject),
            None,
            Some(metadata),
            &tree,
            0,
            gio::Cancellable::NONE,
        )
        .map_err(Error::of("writing the commit"))?;
    Ok(checksum.to_string())
}

/// The generation the next commit on `reference` will be: `parent + 1`, read
/// from the parent commit's own metadata rather than from a counter anywhere
/// else. The number has to survive a machine that was rolled back,
/// reimaged, or had `/var/lib/kiln` deleted.
///
/// Returns an error rather than falling back to 1. Defaulting quietly is how a
/// build ends up writing generation 1 over a machine that has forty of them.
pub fn next_generation(repo: &Repo, reference: &str) -> Result<u64> {
    let parent = match repo.resolve_rev(reference, true) {
        Ok(Some(rev)) => rev.to_string(),
        _ => return Ok(1),
    };
    Ok(generation::next(Some(&read_metadata(repo, &parent)?)))
}

/// Read a commit's Kiln metadata without checking out its tree.
pub fn read_metadata(repo: &Repo, checksum: &str) -> Result<Metadata> {
    let (commit, _) = repo
        .load_commit(checksum)
        .map_err(Error::of("loading the commit"))?;
    // The commit variant is `(a{sv}aya(say)sstayay)`; metadata is child 0.
    let metadata = commit.child_value(0);
    Metadata::from_variant(&metadata, checksum)
}

/// Open the repository, creating a bare one if it is not there. `Bare` rather
/// than `BareUser`: a system repository stores real ownership, and a deployment
/// checked out from a `BareUser` repo has every file owned by whoever built it.
pub fn open_or_create(path: &Path) -> Result<Repo> {
    let repo = Repo::new_for_path(path);
    if repo.open(gio::Cancellable::NONE).is_ok() {
        return Ok(repo);
    }
    std::fs::create_dir_all(path).map_err(|source| Error::Io {
        doing: "creating the repository at",
        path: path.to_path_buf(),
        source,
    })?;
    repo.create(RepoMode::Bare, gio::Cancellable::NONE)
        .map_err(Error::of("creating the repository"))?;
    Ok(repo)
}

/// Every ref this repository holds that Kiln wrote, in `kiln/<image>/<arch>`
/// form.
///
/// A repository can hold more than one image — `--sysroot` exists so an
/// installer can build several into one target — and it can also hold
/// refs that are not Kiln's at all on a machine that has rpm-ostree beside it.
pub fn images(repo: &Repo) -> Result<Vec<String>> {
    let refs = repo
        .list_refs(None, gio::Cancellable::NONE)
        .map_err(Error::of("listing the repository's refs"))?;
    let mut out: Vec<String> = refs
        .into_keys()
        .map(|k| k.to_string())
        .filter(|r| r.starts_with("kiln/"))
        .collect();
    out.sort();
    Ok(out)
}

/// Find a generation by number, across every image in the repository.
///
/// Deliberately not `Sysroot::generations()`, which lists what is *deployed*: a
/// generation that was committed and never deployed, or one whose deployment
/// `kiln clean` has since removed, is still a generation you can rebuild from
/// — the commit is what carries the record, not the deployment.
pub fn find_generation(repo: &Repo, generation: u64) -> Result<(String, Metadata)> {
    let mut available = Vec::new();
    for reference in images(repo)? {
        for (checksum, metadata) in history(repo, &reference)? {
            if metadata.generation == generation {
                return Ok((checksum, metadata));
            }
            available.push(metadata.generation);
        }
    }
    available.sort_unstable();
    available.dedup();
    Err(Error::NoSuchGeneration {
        wanted: generation,
        available,
    })
}

/// Every commit this repository still holds for `reference`'s image, newest
/// generation first.
///
/// **Not simply the parent chain from the tip.** `kiln rm` and `kiln clean`
/// remove generations from the *middle* of that chain — `Removal::budget`
/// keeps the newest few and the baseline — and the prune that follows deletes
/// those commit objects while the surviving newer commit still names one of
/// them as its parent. So a walk from the tip both dead-ends early, at the
/// first pruned ancestor, and skips generations that are still here: on a
/// machine holding generations 8, 7 and a pinned 1, gen 7's parent is gone, so
/// the walk died before it ever reached gen 1 and *every* `kiln show`, `kiln
/// diff` and `kiln rebuild` failed with `No such metadata object` — naming a
/// commit the user never asked about, for generations that were sitting right
/// there.
///
/// What survives a prune is exactly what a ref keeps alive: Kiln's own
/// `kiln/<image>/<arch>`, plus the `ostree/<n>/<n>/<n>` ref libostree writes
/// for each deployment. Walking from all of them, and stopping wherever a
/// chain runs off the edge of what is still stored, is the whole set — and
/// then generation order, not chain order, is what puts it in sequence.
pub fn history(repo: &Repo, reference: &str) -> Result<Vec<(String, Metadata)>> {
    let mut refs: Vec<String> = repo
        .list_refs(None, gio::Cancellable::NONE)
        .map_err(Error::of("listing the repository's refs"))?
        .into_keys()
        .map(|k| k.to_string())
        .filter(|r| r != reference)
        .collect();
    refs.sort();
    refs.insert(0, reference.to_string());

    let mut visited = BTreeSet::new();
    let mut found: Vec<(String, Metadata)> = Vec::new();
    for reference_to_walk in refs {
        let mut at = match repo.resolve_rev(&reference_to_walk, true) {
            Ok(Some(rev)) => Some(rev.to_string()),
            _ => continue,
        };
        while let Some(checksum) = at {
            if !visited.insert(checksum.clone()) {
                // Already walked, and so was everything behind it.
                break;
            }
            // The edge of the repository, not a corrupt one: ask before
            // loading, because `load_commit` on a pruned parent is an error
            // that would fail the whole command.
            let present = repo
                .has_object(ObjectType::Commit, &checksum, gio::Cancellable::NONE)
                .map_err(Error::of("looking for a commit in the repository"))?;
            if !present {
                break;
            }
            let (commit, _) = repo
                .load_commit(&checksum)
                .map_err(Error::of("loading the commit"))?;
            at = ostree::commit_get_parent(&commit).map(|p| p.to_string());
            match Metadata::from_variant(&commit.child_value(0), &checksum) {
                Ok(metadata) => {
                    if format!("kiln/{}/{}", metadata.image, metadata.arch) == reference {
                        found.push((checksum, metadata));
                    }
                }
                // A commit another tool wrote — a repository can hold
                // rpm-ostree's beside Kiln's — or one whose metadata version
                // this Kiln cannot read. Neither is this image's history, and
                // neither is a reason to fail a command about it.
                Err(Error::NotOurs { .. }) => {}
                Err(e) => return Err(e),
            }
        }
    }
    found.sort_by_key(|(_, m)| std::cmp::Reverse(m.generation));
    Ok(found)
}
