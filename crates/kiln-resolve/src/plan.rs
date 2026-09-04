//! `BuildPlan` — resolved.
//!
//! The plan is the output of the cheap, networked, metadata-only half of the
//! build and the complete input list for the expensive half. Nothing here has
//! been downloaded, built, or unpacked; that is what makes `kiln check`
//! possible without building, and what lets `kiln build` refuse a no-op.

use kiln_manifest::{Canon, Canonical, Hash, ScriptPhase, HASH_EPOCH};
use std::collections::BTreeMap;

/// Which image, on which architecture. Determines the OSTree ref
/// `kiln/<image>/<arch>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub name: String,
    pub arch: String,
}

impl ImageRef {
    /// one ref per image, linear history.
    pub fn ostree_ref(&self) -> String {
        format!("kiln/{}/{}", self.name, self.arch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    pub config_id: Hash,
    pub image: ImageRef,
    /// Canonically ordered — by variant, then by identity within it. Order is
    /// content-determined so that two resolutions of the same configuration
    /// hash the same regardless of what order libalpm answered in.
    pub inputs: Vec<ResolvedInput>,
    /// inputs that genuinely cannot be resolved without fetching.
    /// Excluded from `plan_id`, reported separately, resolvable with `--deep`.
    pub volatile: Vec<VolatileInput>,
    /// replayed from the previous generation so package-created service
    /// accounts keep the IDs they had. The *seed*, not the result: a new
    /// account is allocated during assembly and lands in the next record.
    pub uid_map: UidMap,
    /// Facts about *this* resolution rather than about its result. Excluded
    /// from `plan_id` — see `Provenance`.
    pub provenance: Provenance,
}

/// When and against what this plan was resolved.
///
/// Deliberately outside the hash. In rolling mode (the default) every
/// build resolves on a different date; if the date were part of `plan_id`,
/// `kiln build` would never be able to say "nothing to do". The date still
/// matters — it is what makes a past image reconstructible without anyone
/// having pinned anything — so it is recorded, just not hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// RFC 3339, second resolution, UTC.
    pub resolved_at: String,
    /// The snapshot the packages actually came from: the configured date, or
    /// today's date when tracking live mirrors.
    pub snapshot: String,
    /// The repositories consulted, in priority order, with their servers.
    pub repos: Vec<(String, Vec<String>)>,
    pub libalpm: String,
}

/// The whole input taxonomy:
///
/// Every variant's encoding is tagged by name, so a plan containing none of a
/// given kind encodes exactly as it would have before that kind existed —
/// which is what let phase 3 add four variants without disturbing a single
/// `plan_id`. There is a frozen test for precisely that claim.
///
/// `BuildScript` is absent: a script's effect is an overlayfs changeset over
/// the staging root, so it has nothing to resolve *to* until assembly
/// exists.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolvedInput {
    RepoPackage {
        name: String,
        /// epoch:version-rel, as pacman writes it.
        evr: String,
        filename: String,
        sha256: String,
        repo: String,
        /// Named in the configuration rather than pulled in. Drives the install
        /// reason recorded in the image's package database, which is what makes
        /// `pacman -Qe` on a booted image mean something.
        explicit: bool,
    },
    File {
        target: String,
        content: ContentRef,
        mode: Option<u32>,
    },
    Unit {
        name: String,
        content: ContentRef,
        enable: EnableState,
    },
    /// **Identity is the AUR git commit**, not the version string — that
    /// is what makes "the maintainer force-pushed a different PKGBUILD with the
    /// same pkgver" a detected change rather than an invisible one.
    AurPackage {
        name: String,
        /// The package base, which is what names the git repository realization
        /// clones. A split AUR package's `name` is not its `pkgbase`, so
        /// without this the plan would not be the complete input list it claims
        /// to be — realization would have to go back to the RPC to find out
        /// where to fetch from.
        ///
        /// **Not hashed**, and that is not an oversight: `srcinfo_hash` below
        /// fingerprints the RPC's whole description of the package, `pkgbase`
        /// included, so hashing it here would be hashing the same fact twice.
        pkgbase: String,
        evr: String,
        /// The resolved `HEAD` of `https://aur.archlinux.org/<pkgbase>.git`, or
        /// the pinned commit.
        aur_commit: String,
        /// blake3 of the `.SRCINFO` the RPC reported, so a change to the
        /// recipe's *metadata* is visible even at the same commit.
        srcinfo_hash: Hash,
        /// `None` when the configuration named it. every transitively
        /// pulled AUR package is marked with what required it, because nothing
        /// enters the image anonymously.
        pulled_in_by: Option<String>,
    },
    /// A PKGBUILD from the configuration tree, built in a sandbox.
    BuiltPackage {
        name: String,
        /// The recipe directory, config-root-relative, as `packages.build`
        /// wrote it. Realization needs it to know what to build; **not hashed**,
        /// because `recipe` below already says what the directory *contains*
        /// and renaming one is not a reason to rebuild — the same rule
        /// `ContentRef` follows for a file's path.
        path: String,
        /// The build key's cache identity. This is what decides whether anything is
        /// built at all.
        build_key: Hash,
        /// The PKGBUILD directory's own hash. Carried *alongside* `build_key`
        /// rather than folded away, so `kiln check` can say whether something
        /// rebuilds because the recipe changed or because its build-time
        /// dependencies moved — which are very different pieces of news.
        recipe: Hash,
        sources: Vec<SourcePin>,
    },
    /// A `.pkg.tar.zst` sitting in the configuration tree.
    FilePackage {
        path: String,
        /// Verified against the file on disk during resolution. an
        /// optional integrity guarantee is not a guarantee.
        sha256: String,
    },
    /// An out-of-tree module compiled against the exact kernel in the
    /// image, and packaged like anything else.
    KernelModule {
        name: String,
        /// The module's source tree, config-root-relative. Not hashed, for the
        /// same reason as `BuiltPackage::path`.
        source: String,
        build_key: Hash,
        recipe: Hash,
        /// including this in the build key is what makes "rebuild
        /// modules when the kernel changes" automatic rather than a special
        /// case.
        kernel_evr: String,
    },
    /// A build script, pinned by the text that will run.
    ///
    /// There is nothing to *fetch*, so this variant carries no more than the
    /// frontend already knew. It is in the plan anyway, because a script is an
    /// input whose effect is arbitrary: leaving it out would mean `plan_id`
    /// moved on an edited script only through `config_id`, and would leave
    /// `kiln check` with no way to say *which* script changed — which is
    /// exactly what promises it says.
    BuildScript {
        name: String,
        /// Which of the two assembly slots it runs in. Part of the identity
        /// because moving a script from `packages` to `files` changes the tree
        /// it sees, and therefore what it produces.
        phase: ScriptPhase,
        content: ContentRef,
    },
}

// `SourcePin` lives in `kiln-build`: it describes one input to a *build*, and
// putting it here made the two crates depend on each other. Re-exported so the
// plan still reads as one vocabulary.
pub use kiln_build::SourcePin;

impl ResolvedInput {
    /// A stable sort key: variant first, then identity. Never the order
    /// libalpm or the manifest happened to produce.
    fn sort_key(&self) -> (u8, &str) {
        match self {
            ResolvedInput::RepoPackage { name, .. } => (0, name),
            ResolvedInput::AurPackage { name, .. } => (1, name),
            ResolvedInput::BuiltPackage { name, .. } => (2, name),
            ResolvedInput::FilePackage { path, .. } => (3, path),
            ResolvedInput::KernelModule { name, .. } => (4, name),
            ResolvedInput::File { target, .. } => (5, target),
            ResolvedInput::Unit { name, .. } => (6, name),
            ResolvedInput::BuildScript { name, .. } => (7, name),
        }
    }

    /// The name this input contributes to the image's package set, if it
    /// contributes one. everything packaged goes through pacman, so these
    /// are exactly the inputs the transaction will be given.
    pub fn package_name(&self) -> Option<&str> {
        match self {
            ResolvedInput::RepoPackage { name, .. }
            | ResolvedInput::AurPackage { name, .. }
            | ResolvedInput::BuiltPackage { name, .. }
            | ResolvedInput::KernelModule { name, .. } => Some(name),
            ResolvedInput::FilePackage { path, .. } => Some(path),
            ResolvedInput::File { .. }
            | ResolvedInput::Unit { .. }
            | ResolvedInput::BuildScript { .. } => None,
        }
    }

    /// The cache identity of an input that has to be built, if it has to be
    /// built. package builds are minutes to hours, so this is the key the
    /// whole build cache turns on.
    pub fn build_key(&self) -> Option<&Hash> {
        match self {
            ResolvedInput::BuiltPackage { build_key, .. }
            | ResolvedInput::KernelModule { build_key, .. } => Some(build_key),
            _ => None,
        }
    }
}

/// Where a piece of unpackaged content came from, identified by content rather
/// than by path. Both cases hash the same way, so moving an inline string into
/// a file — or the reverse — changes nothing but the diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContentRef {
    /// Written in the TOML.
    Inline { digest: Hash },
    /// A file or tree under the config root, already hashed into the
    /// Manifest's `local_digests`.
    Local { path: String, digest: Hash },
}

impl ContentRef {
    pub fn digest(&self) -> &Hash {
        match self {
            ContentRef::Inline { digest } => digest,
            ContentRef::Local { digest, .. } => digest,
        }
    }
}

/// A unit can be shipped without being enabled, which is a different
/// thing from being masked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnableState {
    Enabled,
    Disabled,
    Masked,
    /// Shipped; whatever the vendor preset says applies.
    Unset,
}

impl EnableState {
    fn tag(self) -> &'static str {
        match self {
            EnableState::Enabled => "enabled",
            EnableState::Disabled => "disabled",
            EnableState::Masked => "masked",
            EnableState::Unset => "unset",
        }
    }
}

/// Kiln does not guess: an untrustworthy `kiln check` is worse than no
/// `kiln check`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VolatileInput {
    pub input: String,
    /// What would have to be fetched, in words a user can act on.
    pub reason: String,
    /// What to fetch, for `--deep`.
    ///
    /// `input` and `reason` are prose for a person; this is the same fact in a
    /// form the resolver can act on. They were prose only, once, and `--deep`
    /// had to parse `"recipes/foo: git+https://…"` back apart to know what to
    /// do — which works until a recipe path contains `": "`.
    pub what: Volatile,
}

/// The two things names, and the only two there are.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Volatile {
    /// A `source=()` entry in a recipe under the config root: a `SKIP`
    /// checksum, or a VCS source whose revision is decided by whatever
    /// upstream's branch points at today.
    RecipeSource {
        /// Config-root-relative, as the manifest wrote it.
        recipe: String,
        /// The `source=()` entry, as written.
        spec: String,
    },
    /// An AUR package whose `pkgver()` runs upstream code, so its version is
    /// not what the RPC reports.
    AurPackage { name: String },
}

/// Service accounts a previous generation allocated, so this one lands
/// on the same numbers.
///
/// Users and groups are separate maps rather than one map of optional ids.
/// Flattening them cannot express the question the seed has to answer: is there
/// a group named `http`, or does the user `http` merely have someone else's
/// group as its primary? Both are real — `http` owns group `http`, while
/// `games` has `users` as its primary group and no group of its own — and
/// getting it backwards either invents a group or writes a `u` line naming a
/// gid that no line creates.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UidMap {
    /// name → gid.
    pub groups: BTreeMap<String, u32>,
    pub users: BTreeMap<String, IdEntry>,
}

impl UidMap {
    pub fn new() -> UidMap {
        UidMap::default()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.users.is_empty()
    }

    pub fn len(&self) -> usize {
        self.groups.len() + self.users.len()
    }

    /// The group at `gid`, by name. What a `u` line needs to refer to a group
    /// without asserting that one of the user's own name exists.
    pub fn group_at(&self, gid: u32) -> Option<&str> {
        self.groups
            .iter()
            .find(|(_, g)| **g == gid)
            .map(|(n, _)| n.as_str())
    }
}

/// One replayed user account.
///
/// The numbers are the point, but they are not the whole entry. The seed is
/// materialized as a `sysusers.d` fragment processed *before* any package's
/// own, and systemd-sysusers takes the first declaration of a name and ignores
/// every later one — so whatever the seed does not say, the seed decides by
/// omission. An entry carrying only the ids would recreate `http` next
/// generation with a home of `/` instead of `/srv/http`, silently, and then
/// record that as the truth to replay forever. Carrying `home` and `shell` is
/// what keeps the replay a replay.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct IdEntry {
    pub uid: u32,
    pub gid: u32,
    /// As the previous generation's `passwd` recorded it. Empty means "let
    /// sysusers choose", which is what an empty passwd field already means.
    pub home: String,
    pub shell: String,
}

impl BuildPlan {
    /// **The build identity**. Change detection compares this against
    /// the booted deployment's recorded `plan_id`.
    ///
    /// It covers `config_id` — and therefore every local file digest — plus
    /// every resolved external input and the UID seed. It excludes `volatile`
    /// (unresolvable without fetching) and `provenance` (the date, which
    /// moves every day and would defeat the no-op check).
    pub fn plan_id(&self) -> Hash {
        Hash::of(&self.canon().to_bytes())
    }

    /// Sort `inputs` into canonical order. Called by the resolver; idempotent.
    pub fn canonicalize(&mut self) {
        self.inputs.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        self.volatile.sort();
    }

    pub fn packages(&self) -> impl Iterator<Item = &ResolvedInput> {
        self.inputs
            .iter()
            .filter(|i| matches!(i, ResolvedInput::RepoPackage { .. }))
    }
}

impl Canonical for BuildPlan {
    fn canon(&self) -> Canon {
        Canon::map([
            ("hash_epoch", Canon::Int(HASH_EPOCH as i64)),
            ("config_id", Canon::str(self.config_id.to_string())),
            ("image", self.image.canon()),
            (
                "inputs",
                Canon::list(self.inputs.iter().map(Canonical::canon)),
            ),
            ("uid_map", self.uid_map.canon()),
        ])
    }
}

impl Canonical for ImageRef {
    fn canon(&self) -> Canon {
        Canon::map([
            ("name", Canon::str(&self.name)),
            ("arch", Canon::str(&self.arch)),
        ])
    }
}

impl Canonical for ResolvedInput {
    fn canon(&self) -> Canon {
        match self {
            ResolvedInput::RepoPackage {
                name,
                evr,
                filename,
                sha256,
                repo,
                explicit,
            } => Canon::map([
                ("kind", Canon::str("repo-package")),
                ("name", Canon::str(name)),
                ("evr", Canon::str(evr)),
                ("filename", Canon::str(filename)),
                ("sha256", Canon::str(sha256)),
                ("repo", Canon::str(repo)),
                ("explicit", Canon::Bool(*explicit)),
            ]),
            ResolvedInput::File {
                target,
                content,
                mode,
            } => Canon::map([
                ("kind", Canon::str("file")),
                ("target", Canon::str(target)),
                ("content", content.canon()),
                ("mode", Canon::opt(mode.map(|m| Canon::Int(m as i64)))),
            ]),
            ResolvedInput::BuildScript {
                name,
                phase,
                content,
            } => Canon::map([
                ("kind", Canon::str("build-script")),
                ("name", Canon::str(name)),
                (
                    "phase",
                    Canon::str(match phase {
                        ScriptPhase::Packages => "packages",
                        ScriptPhase::Files => "files",
                    }),
                ),
                ("content", content.canon()),
            ]),
            ResolvedInput::Unit {
                name,
                content,
                enable,
            } => Canon::map([
                ("kind", Canon::str("unit")),
                ("name", Canon::str(name)),
                ("content", content.canon()),
                ("enable", Canon::str(enable.tag())),
            ]),
            ResolvedInput::AurPackage {
                name,
                // See the field: covered by `srcinfo_hash`, which fingerprints
                // the RPC's answer and includes it.
                pkgbase: _,
                evr,
                aur_commit,
                srcinfo_hash,
                pulled_in_by,
            } => Canon::map([
                ("kind", Canon::str("aur-package")),
                ("name", Canon::str(name)),
                ("evr", Canon::str(evr)),
                ("aur_commit", Canon::str(aur_commit)),
                ("srcinfo_hash", srcinfo_hash.canon()),
                // Hashed: which package dragged something in is part of what
                // the image is, and a dependency that changes owner is a change
                // worth noticing.
                (
                    "pulled_in_by",
                    Canon::opt(pulled_in_by.as_ref().map(Canon::str)),
                ),
            ]),
            ResolvedInput::BuiltPackage {
                name,
                // See the field: `recipe` is the directory's content, which is
                // what a build key is allowed to depend on.
                path: _,
                build_key,
                recipe,
                sources,
            } => Canon::map([
                ("kind", Canon::str("built-package")),
                ("name", Canon::str(name)),
                ("build_key", build_key.canon()),
                ("recipe", recipe.canon()),
                ("sources", sources.canon()),
            ]),
            ResolvedInput::FilePackage { path, sha256 } => Canon::map([
                ("kind", Canon::str("file-package")),
                ("path", Canon::str(path)),
                ("sha256", Canon::str(sha256)),
            ]),
            ResolvedInput::KernelModule {
                name,
                source: _,
                build_key,
                recipe,
                kernel_evr,
            } => Canon::map([
                ("kind", Canon::str("kernel-module")),
                ("name", Canon::str(name)),
                ("build_key", build_key.canon()),
                ("recipe", recipe.canon()),
                ("kernel_evr", Canon::str(kernel_evr)),
            ]),
        }
    }
}

impl Canonical for ContentRef {
    fn canon(&self) -> Canon {
        // The path is *not* hashed: two configurations that ship the same bytes
        // to the same target produce the same image, and renaming a source file
        // is not a reason to rebuild.
        Canon::map([("digest", Canon::str(self.digest().to_string()))])
    }
}

impl Canonical for UidMap {
    fn canon(&self) -> Canon {
        Canon::map([
            (
                "groups",
                Canon::Map(
                    self.groups
                        .iter()
                        .map(|(n, g)| (n.clone(), Canon::Int(*g as i64)))
                        .collect(),
                ),
            ),
            ("users", self.users.canon()),
        ])
    }
}

impl Canonical for IdEntry {
    fn canon(&self) -> Canon {
        Canon::map([
            ("uid", Canon::Int(self.uid as i64)),
            ("gid", Canon::Int(self.gid as i64)),
            ("home", Canon::str(&self.home)),
            ("shell", Canon::str(&self.shell)),
        ])
    }
}
