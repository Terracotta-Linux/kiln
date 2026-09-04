//! The record itself.

use crate::{Error, Result};
use kiln_manifest::Hash;
use kiln_resolve::{BuildPlan, IdEntry, ResolvedInput, UidMap};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The record format. Bumped when a *reader* would get something wrong, not
/// whenever a field is added: serde ignores fields it does not know and fills
/// missing ones from `Default`, so growing the record is not a format change.
///
/// Deliberately not `HASH_EPOCH` and not Kiln's version. An epoch bump
/// invalidates identities; this says whether the bytes can be parsed.
pub const FORMAT: u32 = 1;

/// Where the record lives inside the image. Assembly step 11.
pub const IN_IMAGE: &str = "usr/lib/kiln/record.json";

/// The commit metadata key.
pub const METADATA_KEY: &str = "kiln.record";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(rename = "kiln")]
    pub format: u32,
    pub plan_id: String,
    pub config_id: String,
    pub generation: u64,
    /// RFC 3339, UTC. From the plan's provenance, so a record and the plan it
    /// came from agree about when.
    pub built_at: String,
    pub image: String,
    pub arch: String,

    pub repos: RepoSnapshot,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_packages: Vec<RepoEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aur_packages: Vec<AurEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub built_packages: Vec<BuiltEntry>,
    /// A `.pkg.tar.zst` from the configuration tree, pinned by sha256 —
    /// the checksum pacman itself verifies. Kept apart from `local_files`,
    /// which are content-addressed by blake3: one list with a field named
    /// `blake3` holding a sha256 is the kind of thing that is read wrong once
    /// and then trusted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_packages: Vec<LocalPackage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_files: Vec<LocalFile>,
    /// Each build script's *text*, by name — the input side. This is
    /// what `kiln check` compares to report `scripts: 20-locale changed`
    /// rather than falling back to "config_id moved", which names nothing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, String>,
    /// The hash of each build script's changeset, so a script whose
    /// output stopped being a pure function of its inputs is visible.
    ///
    /// The output side, and the reason both are here: comparing texts answers
    /// *should* this rebuild, comparing changesets answers *did two builds of
    /// the same text agree* — which is the determinism audit `kiln rebuild`
    /// performs, and the only way to find the one script in a configuration
    /// that is not reproducible.
    ///
    /// Written by the assembler after the scripts have run, not by `Record::of`
    /// — the effect does not exist until the script has produced it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub script_effects: BTreeMap<String, String>,

    /// What the *finished* tree ended up with, not what was seeded —
    /// this is the map the next generation replays, so it has to describe
    /// reality rather than intent.
    pub uid_map: RecordedIds,
    /// What this build *seeded* with, which is what went into its `plan_id`.
    ///
    /// Both maps are needed and they answer different questions. `uid_map` is
    /// what the next generation will replay; `uid_seed` is what this one
    /// replayed, and without it `kiln check` cannot explain the one case where
    /// two builds of an unchanged configuration legitimately differ: generation
    /// 1 has nothing to seed from, so generation 2 pins ids that generation 1
    /// merely allocated. With only `uid_map` the report says "update available"
    /// and then lists nothing, which reads like a bug in `kiln check`.
    #[serde(default, skip_serializing_if = "RecordedIds::is_empty")]
    pub uid_seed: RecordedIds,
}

/// The record's own shape for `UidMap`, converted at the boundary rather than
/// derived on `kiln_resolve::UidMap` directly.
///
/// The record is a *persisted* format read by a Kiln that may be older or newer
/// than the one that wrote it. Deriving `Serialize` on the in-memory type would
/// mean an innocent refactor there silently changed the on-disk format of every
/// image ever built — and the symptom would be a UID map that failed to replay,
/// which is the failure this whole mechanism exists to prevent. The duplication
/// is the point: the two are allowed to disagree, and the conversion is where
/// that gets noticed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedIds {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub users: BTreeMap<String, RecordedUser>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedUser {
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub home: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shell: String,
}

impl RecordedIds {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.users.is_empty()
    }

    pub fn len(&self) -> usize {
        self.groups.len() + self.users.len()
    }
}

impl From<&UidMap> for RecordedIds {
    fn from(map: &UidMap) -> RecordedIds {
        RecordedIds {
            groups: map.groups.clone(),
            users: map
                .users
                .iter()
                .map(|(name, e)| {
                    (
                        name.clone(),
                        RecordedUser {
                            uid: e.uid,
                            gid: e.gid,
                            home: e.home.clone(),
                            shell: e.shell.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}

impl From<&RecordedIds> for UidMap {
    fn from(ids: &RecordedIds) -> UidMap {
        UidMap {
            groups: ids.groups.clone(),
            users: ids
                .users
                .iter()
                .map(|(name, u)| {
                    (
                        name.clone(),
                        IdEntry {
                            uid: u.uid,
                            gid: u.gid,
                            home: u.home.clone(),
                            shell: u.shell.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// recorded even in rolling mode, because it captures the date the
    /// build resolved on. This one field is what makes a past image
    /// reconstructible without anyone having pinned anything in advance.
    pub snapshot: String,
    pub mirrors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    pub evr: String,
    pub repo: String,
    pub filename: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AurEntry {
    pub name: String,
    pub evr: String,
    /// identity is the git commit, not the version string. A maintainer
    /// force-pushing a different PKGBUILD at the same `pkgver` is a change, and
    /// recording the version alone would hide it.
    pub aur_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulled_in_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltEntry {
    pub name: String,
    pub build_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_evr: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    pub url: String,
    pub sha256: String,
}

/// A file or tree under the configuration root, identified by the digest that
/// is already part of `config_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFile {
    pub path: String,
    pub blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPackage {
    pub path: String,
    pub sha256: String,
}

impl Record {
    /// Build a record from the plan that produced the image and the facts the
    /// build discovered.
    ///
    /// `uid_map` is the *captured* one, not `plan.uid_map`. The plan carries
    /// the seed — what the previous generation asked for — and the record has
    /// to carry what this generation actually has, or a new service account
    /// allocated during the transaction would never be pinned and would move
    /// again next time.
    pub fn of(plan: &BuildPlan, generation: u64, uid_map: UidMap) -> Record {
        let mut record = Record {
            format: FORMAT,
            plan_id: plan.plan_id().to_string(),
            config_id: plan.config_id.to_string(),
            generation,
            built_at: plan.provenance.resolved_at.clone(),
            image: plan.image.name.clone(),
            arch: plan.image.arch.clone(),
            repos: RepoSnapshot {
                snapshot: plan.provenance.snapshot.clone(),
                mirrors: plan
                    .provenance
                    .repos
                    .iter()
                    .flat_map(|(_, servers)| servers.iter().cloned())
                    .collect(),
            },
            repo_packages: Vec::new(),
            aur_packages: Vec::new(),
            built_packages: Vec::new(),
            local_packages: Vec::new(),
            local_files: Vec::new(),
            scripts: BTreeMap::new(),
            script_effects: BTreeMap::new(),
            uid_map: RecordedIds::from(&uid_map),
            uid_seed: RecordedIds::from(&plan.uid_map),
        };

        for input in &plan.inputs {
            match input {
                ResolvedInput::RepoPackage {
                    name,
                    evr,
                    filename,
                    sha256,
                    repo,
                    ..
                } => record.repo_packages.push(RepoEntry {
                    name: name.clone(),
                    evr: evr.clone(),
                    repo: repo.clone(),
                    filename: filename.clone(),
                    sha256: sha256.clone(),
                }),
                ResolvedInput::AurPackage {
                    name,
                    evr,
                    aur_commit,
                    pulled_in_by,
                    ..
                } => record.aur_packages.push(AurEntry {
                    name: name.clone(),
                    evr: evr.clone(),
                    aur_commit: aur_commit.clone(),
                    pulled_in_by: pulled_in_by.clone(),
                }),
                ResolvedInput::BuiltPackage {
                    name,
                    build_key,
                    sources,
                    ..
                } => record.built_packages.push(BuiltEntry {
                    name: name.clone(),
                    build_key: build_key.to_string(),
                    kernel_evr: None,
                    sources: sources
                        .iter()
                        .map(|s| SourceEntry {
                            url: s.url.clone(),
                            sha256: s.sha256.clone(),
                        })
                        .collect(),
                }),
                ResolvedInput::KernelModule {
                    name,
                    build_key,
                    kernel_evr,
                    ..
                } => record.built_packages.push(BuiltEntry {
                    name: name.clone(),
                    build_key: build_key.to_string(),
                    kernel_evr: Some(kernel_evr.clone()),
                    sources: Vec::new(),
                }),
                ResolvedInput::FilePackage { path, sha256 } => {
                    record.local_packages.push(LocalPackage {
                        path: path.clone(),
                        sha256: sha256.clone(),
                    })
                }
                // A file or a unit whose bytes came from the configuration
                // tree. Inline content is already covered by `config_id`; a
                // local path is worth naming, because "which file on disk was
                // this" is the question `kiln diff` gets asked.
                ResolvedInput::BuildScript {
                    name,
                    content,
                    phase,
                } => {
                    // The phase rides along in the value: a script moved from
                    // `packages` to `files` sees a different tree, so it is a
                    // change even when the text is byte-identical.
                    record.scripts.insert(
                        name.clone(),
                        format!(
                            "{} after {}",
                            content.digest(),
                            match phase {
                                kiln_manifest::ScriptPhase::Packages => "packages",
                                kiln_manifest::ScriptPhase::Files => "files",
                            }
                        ),
                    );
                    if let kiln_resolve::ContentRef::Local { path, digest } = content {
                        record.local_files.push(LocalFile {
                            path: path.clone(),
                            blake3: digest.to_string(),
                        });
                    }
                }
                ResolvedInput::File { content, .. } | ResolvedInput::Unit { content, .. } => {
                    if let kiln_resolve::ContentRef::Local { path, digest } = content {
                        record.local_files.push(LocalFile {
                            path: path.clone(),
                            blake3: digest.to_string(),
                        });
                    }
                }
            }
        }

        // The plan is canonically ordered, so these come out ordered; a local
        // file can be named by two entries, though, and the record should not
        // say so twice.
        record.local_files.dedup();
        record
    }

    /// The `plan_id` this record was built from, as change detection compares
    /// it.
    pub fn plan_id(&self) -> Hash {
        Hash(self.plan_id.clone())
    }

    /// The map the next generation seeds from: what this image
    /// actually has, not what it was asked to have.
    pub fn next_seed(&self) -> UidMap {
        UidMap::from(&self.uid_map)
    }

    /// What this build seeded with, for `kiln check` to compare against.
    pub fn seeded_with(&self) -> UidMap {
        UidMap::from(&self.uid_seed)
    }

    pub fn to_json(&self) -> String {
        // Pretty-printed even though nobody opens it: `kiln diff` and a bug
        // report both get read by a person eventually, and a 400 KB single line
        // is where that stops being possible.
        serde_json::to_string_pretty(self).expect("a Record always serializes")
    }

    pub fn parse(text: &str) -> Result<Record> {
        // The format is read before the rest, so a record from a newer Kiln
        // produces "built by a newer Kiln" rather than a complaint about a
        // field name.
        #[derive(Deserialize)]
        struct JustTheFormat {
            #[serde(rename = "kiln")]
            format: u32,
        }
        let probe: JustTheFormat = serde_json::from_str(text).map_err(Error::Malformed)?;
        if probe.format > FORMAT {
            return Err(Error::Unsupported {
                found: probe.format,
                understood: FORMAT,
            });
        }
        serde_json::from_str(text).map_err(Error::Malformed)
    }

    pub fn read(path: &Path) -> Result<Record> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            doing: "reading the build record at",
            path: path.to_path_buf(),
            source,
        })?;
        Record::parse(&text)
    }
}
