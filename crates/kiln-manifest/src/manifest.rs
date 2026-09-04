//! The `Manifest`: merged, validated, canonical.
//!
//! Properties that must hold, and are property-tested:
//!
//! - Every collection is a `BTree*` — iteration order is content-determined,
//!   not insertion-determined.
//! - No `Option` survives validation without an explicit default. The three
//!   that remain are genuinely optional in the schema, not missing defaults:
//!   `system.hostname` (says systemd's own default applies), and the
//!   `source`/`content` pair on files, units and scripts, which is an
//!   either-or the semantic phase enforces.
//! - All paths are absolute and normalized; all relative `source` paths have
//!   been resolved to config-root-relative form *and* hashed into `local_digests`.

use crate::canon::{Canon, Canonical, Hash};
use kiln_diag::{Origin, OriginMap};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Bumping this invalidates every cached identity on purpose. It is **not**
/// Kiln's version number: a point release must not force the world to rebuild.
/// A hash-freeze test failure is either a bug or a bump — never a shrug.
///
/// | epoch | why |
/// |---|---|
/// | 1 | the first frozen encoding |
/// | 2 | `boot.loader` defaults to `grub2` rather than `systemd-boot`. A different bootloader is a genuinely different image, so every identity moving is correct rather than incidental. |
/// | 3 | the UID seed became a users/groups pair carrying `home` and `shell`, not a flat map of numbers. Writing the assembler showed that the flat shape cannot say whether a user owns a group of its own name, and that a seed omitting home and shell decides them by omission. |
pub const HASH_EPOCH: u32 = 3;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub image: Image,
    pub repos: Repos,
    pub packages: PackageSet,
    pub kernel: Kernel,
    pub boot: Boot,
    pub systemd: SystemdState,
    /// Keyed by target path ⇒ collisions are impossible.
    pub files: BTreeMap<String, FileEntry>,
    /// Keyed by name ⇒ order is content-determined, not file-order-determined.
    pub scripts: BTreeMap<String, Script>,
    pub system: SystemDefaults,
    /// blake3 of every local file or tree referenced above. Putting this in the
    /// Manifest is why editing `files/motd` changes `config_id` even though no
    /// TOML changed.
    pub local_digests: BTreeMap<String, Hash>,
    /// Retained for diagnostics only, excluded from hashing — and excluded
    /// from the persisted form too. Spans point into files on the machine that
    /// built the image; a generation read back out of a commit has no such
    /// files, and an origin naming `desktop.toml:14` of a config that has since
    /// been edited is worse than no origin at all.
    #[serde(skip)]
    pub origins: OriginMap,
    /// Every element of a list-valued key, and the exact span it was written
    /// at. Keyed `"<dotted key>/<element>"`, e.g. `packages.repo/neovim`.
    ///
    /// `origins` answers "which file set this key". This answers "which file,
    /// and where exactly, asked for *this one package*" — and a resolution
    /// failure is always about one element of a list, so without it the best a
    /// diagnostic could do is underline the whole array. Diagnostics
    /// only, excluded from hashing, like `origins` — and not persisted, for
    /// the same reason.
    #[serde(skip)]
    pub item_origins: BTreeMap<String, Origin>,
}

/// Two manifests are the same manifest when their canonical encodings agree —
/// which is the same thing as their `config_id`s agreeing.
///
/// Written out rather than derived because `origins` and `item_origins` are
/// diagnostics: two runs that produced identical images from configurations
/// laid out across different files are not different manifests, and a derived
/// comparison would say they were. It also cannot be derived — `Origin` carries
/// a `NamedSource` and has no equality of its own — but that is the smaller
/// reason.
impl PartialEq for Manifest {
    fn eq(&self, other: &Manifest) -> bool {
        self.config_id() == other.config_id()
    }
}

impl Eq for Manifest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub name: String,
    pub arch: String,
}

impl Default for Image {
    fn default() -> Self {
        Image {
            name: "system".into(),
            arch: host_arch().into(),
        }
    }
}

/// What `image.arch` defaults to when unset. Named rather than inlined so that
/// the one place Kiln decides "what am I building for" is greppable.
pub fn host_arch() -> &'static str {
    std::env::consts::ARCH
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Snapshot {
    /// rolling, like Arch. The default.
    Latest,
    Date(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repos {
    pub snapshot: Snapshot,
    pub mirrors: BTreeSet<String>,
    pub extra: BTreeMap<String, ExtraRepo>,
}

impl Default for Repos {
    fn default() -> Self {
        Repos {
            snapshot: Snapshot::Latest,
            mirrors: BTreeSet::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraRepo {
    pub name: String,
    pub server: String,
    pub key: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSet {
    pub repo: BTreeSet<String>,
    pub aur: BTreeMap<String, AurPackage>,
    pub build: BTreeSet<String>,
    pub file: BTreeMap<String, LocalPackage>,
    /// must not appear, even as a dependency.
    pub exclude: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AurPackage {
    pub name: String,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPackage {
    pub path: String,
    /// Required, not optional: an optional integrity guarantee is not a
    /// guarantee.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kernel {
    pub package: String,
    /// headers are a *build-time* dependency installed inside the sandbox.
    /// Shipping ~150 MB of them in an immutable image is pure waste.
    pub headers: bool,
    pub cmdline: BTreeSet<String>,
    pub modules: KernelModules,
    pub out_of_tree: BTreeMap<String, OutOfTreeModule>,
}

impl Default for Kernel {
    fn default() -> Self {
        Kernel {
            package: "linux".into(),
            headers: false,
            cmdline: BTreeSet::new(),
            modules: KernelModules::default(),
            out_of_tree: BTreeMap::new(),
        }
    }
}

/// three different things, three different keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelModules {
    pub load: BTreeSet<String>,
    pub blacklist: BTreeSet<String>,
    pub options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutOfTreeModule {
    pub name: String,
    pub source: String,
}

/// GRUB2, through libostree's own `bootloader=grub2` backend.
///
/// Not systemd-boot, which an earlier draft defaulted to. ostree manages
/// `/boot/loader` as a symlink pair so that entry swaps are atomic, and UEFI
/// firmware reads only FAT — so `/boot` cannot be the ESP, and systemd-boot
/// cannot read an ext4 `/boot`. libostree 2026.4 has backends for `grub2`,
/// `syslinux`, `uboot` and `zipl`, and none for systemd-boot.
///
/// One variant, because there is exactly one supported answer. The enum stays
/// rather than collapsing into nothing: `boot.loader` is a real key a user can
/// write, and the day a second backend is supported this is where it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BootLoader {
    #[default]
    Grub2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// One value, for the same shape of reason `BootLoader` has one:
/// upstream `ostree` ships a dracut module that handles the sysroot pivot, and
/// mkinitcpio has no equivalent.
///
/// It stays an enum rather than collapsing into nothing because `boot.initramfs`
/// is a real key a user can write, and the value they are most likely to write
/// deserves a diagnostic that explains itself rather than an
/// unknown-key message.
pub enum Initramfs {
    Dracut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boot {
    pub loader: BootLoader,
    pub timeout: i64,
    pub initramfs: Initramfs,
}

impl Default for Boot {
    fn default() -> Self {
        Boot {
            loader: BootLoader::Grub2,
            timeout: 5,
            initramfs: Initramfs::Dracut,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemdState {
    pub enable: BTreeSet<String>,
    pub disable: BTreeSet<String>,
    pub mask: BTreeSet<String>,
    pub units: BTreeMap<String, UnitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitFile {
    pub name: String,
    pub source: Option<String>,
    pub content: Option<String>,
    pub enable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// A real system path as the user wrote it. Kiln owns the `/usr/etc`
    /// translation — it does not happen here.
    pub target: String,
    pub source: Option<String>,
    pub content: Option<String>,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ScriptPhase {
    Packages,
    Files,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Script {
    pub name: String,
    pub source: Option<String>,
    pub content: Option<String>,
    pub after: ScriptPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locale {
    pub lang: String,
    pub generate: BTreeSet<String>,
}

impl Default for Locale {
    fn default() -> Self {
        Locale {
            lang: "C.UTF-8".into(),
            generate: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemDefaults {
    /// unset by default; systemd's own default applies.
    pub hostname: Option<String>,
    pub timezone: String,
    pub keymap: String,
    pub locale: Locale,
}

impl Default for SystemDefaults {
    fn default() -> Self {
        SystemDefaults {
            hostname: None,
            timezone: "UTC".into(),
            keymap: "us".into(),
            locale: Locale::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical encoding. `origins` is deliberately absent from every impl below:
// provenance is diagnostics, not identity.
// ---------------------------------------------------------------------------

impl Manifest {
    /// blake3 of the canonical encoding.
    pub fn config_id(&self) -> Hash {
        Hash::of(&self.canon().to_bytes())
    }
}

impl Canonical for Manifest {
    fn canon(&self) -> Canon {
        Canon::map([
            ("hash_epoch", Canon::Int(HASH_EPOCH as i64)),
            ("schema", Canon::Int(self.schema as i64)),
            ("image", self.image.canon()),
            ("repos", self.repos.canon()),
            ("packages", self.packages.canon()),
            ("kernel", self.kernel.canon()),
            ("boot", self.boot.canon()),
            ("systemd", self.systemd.canon()),
            ("files", self.files.canon()),
            ("scripts", self.scripts.canon()),
            ("system", self.system.canon()),
            ("local_digests", self.local_digests.canon()),
        ])
    }
}

impl Canonical for Image {
    fn canon(&self) -> Canon {
        Canon::map([
            ("name", Canon::str(&self.name)),
            ("arch", Canon::str(&self.arch)),
        ])
    }
}

impl Canonical for Snapshot {
    fn canon(&self) -> Canon {
        match self {
            Snapshot::Latest => Canon::map([("latest", Canon::Bool(true))]),
            Snapshot::Date(d) => Canon::map([("date", Canon::str(d))]),
        }
    }
}

impl Canonical for Repos {
    fn canon(&self) -> Canon {
        Canon::map([
            ("snapshot", self.snapshot.canon()),
            ("mirrors", self.mirrors.canon()),
            ("extra", self.extra.canon()),
        ])
    }
}

impl Canonical for ExtraRepo {
    fn canon(&self) -> Canon {
        Canon::map([
            ("name", Canon::str(&self.name)),
            ("server", Canon::str(&self.server)),
            ("key", Canon::opt(self.key.as_ref().map(Canon::str))),
        ])
    }
}

impl Canonical for PackageSet {
    fn canon(&self) -> Canon {
        Canon::map([
            ("repo", self.repo.canon()),
            ("aur", self.aur.canon()),
            ("build", self.build.canon()),
            ("file", self.file.canon()),
            ("exclude", self.exclude.canon()),
        ])
    }
}

impl Canonical for AurPackage {
    fn canon(&self) -> Canon {
        Canon::map([
            ("name", Canon::str(&self.name)),
            ("commit", Canon::opt(self.commit.as_ref().map(Canon::str))),
        ])
    }
}

impl Canonical for LocalPackage {
    fn canon(&self) -> Canon {
        Canon::map([
            ("path", Canon::str(&self.path)),
            ("sha256", Canon::str(&self.sha256)),
        ])
    }
}

impl Canonical for Kernel {
    fn canon(&self) -> Canon {
        Canon::map([
            ("package", Canon::str(&self.package)),
            ("headers", Canon::Bool(self.headers)),
            ("cmdline", self.cmdline.canon()),
            ("modules", self.modules.canon()),
            ("out_of_tree", self.out_of_tree.canon()),
        ])
    }
}

impl Canonical for KernelModules {
    fn canon(&self) -> Canon {
        Canon::map([
            ("load", self.load.canon()),
            ("blacklist", self.blacklist.canon()),
            ("options", self.options.canon()),
        ])
    }
}

impl Canonical for OutOfTreeModule {
    fn canon(&self) -> Canon {
        Canon::map([
            ("name", Canon::str(&self.name)),
            ("source", Canon::str(&self.source)),
        ])
    }
}

impl Canonical for Boot {
    fn canon(&self) -> Canon {
        Canon::map([
            (
                "loader",
                Canon::str(match self.loader {
                    BootLoader::Grub2 => "grub2",
                }),
            ),
            ("timeout", Canon::Int(self.timeout)),
            (
                "initramfs",
                Canon::str(match self.initramfs {
                    Initramfs::Dracut => "dracut",
                }),
            ),
        ])
    }
}

impl Canonical for SystemdState {
    fn canon(&self) -> Canon {
        Canon::map([
            ("enable", self.enable.canon()),
            ("disable", self.disable.canon()),
            ("mask", self.mask.canon()),
            ("units", self.units.canon()),
        ])
    }
}

impl Canonical for UnitFile {
    fn canon(&self) -> Canon {
        Canon::map([
            ("name", Canon::str(&self.name)),
            ("source", Canon::opt(self.source.as_ref().map(Canon::str))),
            ("content", Canon::opt(self.content.as_ref().map(Canon::str))),
            ("enable", Canon::Bool(self.enable)),
        ])
    }
}

impl Canonical for FileEntry {
    fn canon(&self) -> Canon {
        Canon::map([
            ("target", Canon::str(&self.target)),
            ("source", Canon::opt(self.source.as_ref().map(Canon::str))),
            ("content", Canon::opt(self.content.as_ref().map(Canon::str))),
            ("mode", Canon::opt(self.mode.map(|m| Canon::Int(m as i64)))),
        ])
    }
}

impl Canonical for Script {
    fn canon(&self) -> Canon {
        Canon::map([
            ("name", Canon::str(&self.name)),
            ("source", Canon::opt(self.source.as_ref().map(Canon::str))),
            ("content", Canon::opt(self.content.as_ref().map(Canon::str))),
            (
                "after",
                Canon::str(match self.after {
                    ScriptPhase::Packages => "packages",
                    ScriptPhase::Files => "files",
                }),
            ),
        ])
    }
}

impl Canonical for Locale {
    fn canon(&self) -> Canon {
        Canon::map([
            ("lang", Canon::str(&self.lang)),
            ("generate", self.generate.canon()),
        ])
    }
}

impl Canonical for SystemDefaults {
    fn canon(&self) -> Canon {
        Canon::map([
            (
                "hostname",
                Canon::opt(self.hostname.as_ref().map(Canon::str)),
            ),
            ("timezone", Canon::str(&self.timezone)),
            ("keymap", Canon::str(&self.keymap)),
            ("locale", self.locale.canon()),
        ])
    }
}
