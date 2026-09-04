//! Commit metadata and generation numbers.
//!
//! Raw OSTree deployment indices renumber as deployments come and go: today's
//! index 1 is tomorrow's index 0, which makes `kiln remove 1` a footgun. Kiln
//! assigns a **monotonic** generation at commit time and stores it in the
//! commit, so the number a user types today means the same thing next week.

use crate::{Error, Result};
use kiln_manifest::Manifest;
use kiln_record::Record;
use kiln_resolve::BuildPlan;

/// Everything Kiln puts in a commit's metadata is namespaced. `ostree.bootable`
/// is not — it is libostree's own, and setting it is what makes the deployment
/// get a BLS entry.
pub const KEY_PREFIX: &str = "kiln.";

/// The metadata schema version, independent of `HASH_EPOCH` and of the record's
/// own format. It answers one question: can this Kiln read this commit's
/// metadata at all.
pub const METADATA_VERSION: &str = "1";

/// What a commit says about itself, without checking out its tree.
///
/// the record is in the metadata *and* in the tree on purpose. Metadata is
/// readable without a checkout, which is what makes `kiln list` and `kiln
/// check` fast; the in-tree copy survives an export to a tarball or an
/// inspection mount, where metadata does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub version: String,
    pub plan_id: String,
    pub config_id: String,
    pub generation: u64,
    pub image: String,
    pub arch: String,
    pub built_at: String,
    /// The machine that built it. `kiln diff` between two generations built on
    /// different machines is a question people ask.
    pub built_by: String,
    /// zstd-compressed JSON. Absent on a commit whose record could not be
    /// compressed, which is a bug rather than a state to design for — but a
    /// missing record must not make `kiln list` fail.
    pub record: Option<Record>,
    /// The merged manifest the generation was built from, zstd-compressed JSON,
    /// beside the record and for the same reason.
    ///
    /// The record pins what the image was *made of*; the manifest is what it
    /// was *asked to be*, and `kiln rebuild <gen>` needs both. A record names a
    /// package and its checksum, and says nothing about the `[[file]]` targets,
    /// the unit states or the build scripts that the tree also has to be
    /// assembled from — so a rebuild from the record alone would reproduce the
    /// packages and quietly drop everything else.
    ///
    /// Optional in exactly the way `record` is: a generation built by an older
    /// Kiln has none, and that must degrade to "this one cannot be rebuilt"
    /// rather than to `kiln list` failing.
    pub manifest: Option<Manifest>,
}

impl Metadata {
    pub fn of(
        plan: &BuildPlan,
        generation: u64,
        record: &Record,
        manifest: &Manifest,
        built_by: &str,
    ) -> Metadata {
        Metadata {
            version: METADATA_VERSION.to_string(),
            plan_id: plan.plan_id().to_string(),
            config_id: plan.config_id.to_string(),
            generation,
            image: plan.image.name.clone(),
            arch: plan.image.arch.clone(),
            built_at: plan.provenance.resolved_at.clone(),
            built_by: built_by.to_string(),
            record: Some(record.clone()),
            manifest: Some(manifest.clone()),
        }
    }

    /// Build the GVariant libostree stores. `a{sv}`, which is what
    /// `write_commit` takes.
    pub fn to_variant(&self) -> Result<glib::Variant> {
        let mut dict = glib::VariantDict::new(None);
        insert(&mut dict, "version", &self.version);
        insert(&mut dict, "plan-id", &self.plan_id);
        insert(&mut dict, "config-id", &self.config_id);
        insert(&mut dict, "image", &self.image);
        insert(&mut dict, "arch", &self.arch);
        insert(&mut dict, "built-at", &self.built_at);
        insert(&mut dict, "built-by", &self.built_by);
        dict.insert(&format!("{KEY_PREFIX}generation"), self.generation);

        if let Some(record) = &self.record {
            let json = record.to_json();
            let packed = compress("kiln.record", json.as_bytes())?;
            dict.insert(&format!("{KEY_PREFIX}record"), packed.as_slice());
        }
        if let Some(manifest) = &self.manifest {
            let json = serde_json::to_string(manifest).map_err(|source| Error::Io {
                doing: "serializing the manifest for",
                path: "kiln.manifest".into(),
                source: source.into(),
            })?;
            let packed = compress("kiln.manifest", json.as_bytes())?;
            dict.insert(&format!("{KEY_PREFIX}manifest"), packed.as_slice());
        }

        // libostree's own, not Kiln's: without it the deployment gets no BLS
        // entry and the machine boots the previous generation, silently.
        dict.insert("ostree.bootable", true);
        Ok(dict.end())
    }

    /// Read it back. `checksum` is only for the error messages — a commit that
    /// is not Kiln's should say which commit it was.
    pub fn from_variant(variant: &glib::Variant, checksum: &str) -> Result<Metadata> {
        let dict = glib::VariantDict::new(Some(variant));
        let get = |key: &str| -> Option<String> {
            dict.lookup_value(&format!("{KEY_PREFIX}{key}"), None)?
                .str()
                .map(str::to_string)
        };

        let Some(version) = get("version") else {
            return Err(Error::NotOurs {
                checksum: checksum.to_string(),
                why: "has no `kiln.version` in its metadata: it was not built by Kiln".into(),
            });
        };
        if version != METADATA_VERSION {
            return Err(Error::NotOurs {
                checksum: checksum.to_string(),
                why: format!(
                    "carries metadata version {version}; this Kiln understands \
                     {METADATA_VERSION}. Build a new generation rather than reading this one"
                ),
            });
        }

        let required = |key: &'static str| -> Result<String> {
            get(key).ok_or_else(|| Error::NotOurs {
                checksum: checksum.to_string(),
                why: format!("is missing `{KEY_PREFIX}{key}`, which every Kiln commit has"),
            })
        };

        // A record that will not decompress is a broken commit, not a reason
        // for `kiln list` to fail — the generation, the ids and the date are
        // all still there, and those are what the listing shows.
        let record = dict
            .lookup_value(&format!("{KEY_PREFIX}record"), None)
            .and_then(|v| v.get::<Vec<u8>>())
            .and_then(|packed| decompress(&packed).ok())
            .and_then(|json| Record::parse(&json).ok());

        // Same tolerance, same reason: a generation built by a Kiln that did
        // not write one is not a broken commit, it is a commit `kiln rebuild`
        // has to decline rather than one `kiln list` has to fail on.
        let manifest = dict
            .lookup_value(&format!("{KEY_PREFIX}manifest"), None)
            .and_then(|v| v.get::<Vec<u8>>())
            .and_then(|packed| decompress(&packed).ok())
            .and_then(|json| serde_json::from_str::<Manifest>(&json).ok());

        Ok(Metadata {
            version,
            plan_id: required("plan-id")?,
            config_id: required("config-id")?,
            generation: dict
                .lookup_value(&format!("{KEY_PREFIX}generation"), None)
                .and_then(|v| v.get::<u64>())
                .ok_or_else(|| Error::NotOurs {
                    checksum: checksum.to_string(),
                    why: "is missing `kiln.generation`, which every Kiln commit has".into(),
                })?,
            image: required("image")?,
            arch: required("arch")?,
            built_at: required("built-at")?,
            built_by: get("built-by").unwrap_or_default(),
            record,
            manifest,
        })
    }
}

fn insert(dict: &mut glib::VariantDict, key: &str, value: &str) {
    dict.insert(&format!("{KEY_PREFIX}{key}"), value);
}

/// The record and the manifest go in compressed because commit metadata
/// is read on every `kiln list`, and a few hundred kilobytes of JSON per commit
/// is not that.
///
/// `key` names which of the two, so a compression failure says which blob it
/// was rather than always naming the record.
pub fn compress(key: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(bytes, 3).map_err(|source| Error::Io {
        doing: "compressing the metadata blob for",
        path: key.into(),
        source,
    })
}

pub fn decompress(bytes: &[u8]) -> Result<String> {
    let out = zstd::decode_all(bytes).map_err(|source| Error::Io {
        doing: "decompressing the build record from",
        path: "kiln.record".into(),
        source,
    })?;
    String::from_utf8(out).map_err(|e| Error::Io {
        doing: "decoding the build record from",
        path: "kiln.record".into(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    })
}

/// The next generation number. `parent.generation + 1`, monotonic and
/// stable forever.
pub fn next(parent: Option<&Metadata>) -> u64 {
    parent.map(|m| m.generation + 1).unwrap_or(1)
}
