//! The semantic phase: merged tree → `Manifest`.
//!
//! Type extraction and semantic validation are the same pass because they ask
//! the same question of the same node, and doing them separately would mean
//! carrying a second half-typed representation. Everything wrong is reported
//! before returning.

use crate::digest;
use crate::discover::Loader;
use crate::merge::Merged;
use crate::node::{Node, NodeKind};
use kiln_diag::{did_you_mean, Diag, Errors, Origin};
use kiln_manifest::*;
use std::collections::BTreeMap;

/// Targets Kiln will not write to, and why. `/etc` and `/usr` are the whole
/// legitimate surface: everything else is either owned by OSTree, drained at
/// build time, or a runtime mount.
const FORBIDDEN_TARGETS: &[(&str, &str)] = &[
    (
        "/boot",
        "OSTree owns /boot; the kernel and boot entries are Kiln's to place",
    ),
    (
        "/home",
        "/home is a symlink into /var, and Kiln does not manage user data at all",
    ),
    (
        "/root",
        "/root is a symlink into /var and is machine state, not image content",
    ),
    ("/proc", "runtime mount"),
    ("/sys", "runtime mount"),
    ("/dev", "runtime mount"),
    ("/run", "runtime mount"),
    ("/tmp", "runtime mount"),
    ("/sysroot", "OSTree's physical root"),
    ("/ostree", "OSTree's own storage"),
];

/// Targets that are accepted but do not mean what they look like. The table
/// below: a `[[file]]` under one of these is *seeded*, not written.
///
/// `/var` does not exist in the commit at all, so the bytes go to
/// `/usr/share/factory/var/**` and a tmpfiles `C` line restores them on a
/// machine that does not already have them. That is a real difference in
/// behaviour — the file is a **default**, and a machine whose `/var` already
/// holds one keeps its own — so it gets said out loud rather than silently
/// done. `/opt` and `/srv` are relocated into `/var` before the drain, so they
/// take the same route.
///
/// Not an error, which is what this used to be. This table is explicit that
/// these are accepted "with an informational note", and `kiln-image`'s
/// `overlay::route` has always implemented exactly that — the refusal here made
/// that path unreachable, and made the one obvious way to ship a default
/// database or a seed file impossible to express.
const SEEDED_TARGETS: &[(&str, &str)] = &[
    (
        "/var",
        "/var is not in the commit: this becomes a factory copy plus a tmpfiles \
         `C` line, so it is restored on a machine that has no copy of its own and left \
         alone on one that does",
    ),
    (
        "/opt",
        "/opt is relocated into /var before the drain, so this is seeded rather \
         than written — install to /usr instead if you want it in the image itself",
    ),
    (
        "/srv",
        "/srv is relocated into /var before the drain, so this is seeded rather \
         than written",
    ),
];

const UNIT_SUFFIXES: &[&str] = &[
    "service",
    "socket",
    "timer",
    "target",
    "mount",
    "automount",
    "path",
    "slice",
    "scope",
    "swap",
    "device",
];

/// Returns the manifest and any *notes* — things that are accepted but do not
/// mean what they look like, which the caller surfaces as warnings.
///
/// Notes are separate from `errs` rather than a severity inside it because
/// `Errors::into_result` turns a non-empty set into a failure. A note that
/// failed the build would not be a note.
pub fn validate(merged: &Merged, loader: &mut Loader) -> Result<(Manifest, Errors), Errors> {
    let mut v = Validator {
        errs: Errors::new(),
        notes: Errors::new(),
        loader,
        digests: BTreeMap::new(),
        item_origins: BTreeMap::new(),
    };
    let m = v.manifest(&merged.doc, merged.origins.clone());
    let mut m = m;
    m.local_digests = std::mem::take(&mut v.digests);
    let notes = std::mem::take(&mut v.notes);
    v.errs.into_result((m, notes))
}

struct Validator<'a> {
    errs: Errors,
    /// Accepted, but worth saying out loud. See `SEEDED_TARGETS`.
    notes: Errors,
    loader: &'a mut Loader,
    digests: BTreeMap<String, Hash>,
    /// Filled in by `str_set` as it walks list elements; moved into the
    /// Manifest at the end.
    item_origins: BTreeMap<String, Origin>,
}

impl Validator<'_> {
    fn manifest(&mut self, doc: &Node, origins: kiln_diag::OriginMap) -> Manifest {
        Manifest {
            schema: SCHEMA_VERSION,
            image: self.image(doc),
            repos: self.repos(doc),
            packages: self.packages(doc),
            kernel: self.kernel(doc),
            boot: self.boot(doc),
            systemd: self.systemd(doc),
            files: self.files(doc),
            scripts: self.scripts(doc),
            system: self.system(doc),
            local_digests: BTreeMap::new(),
            origins,
            item_origins: std::mem::take(&mut self.item_origins),
        }
    }

    // -- accessors ---------------------------------------------------------

    fn node<'d>(&self, doc: &'d Node, path: &str) -> Option<&'d Node> {
        crate::node::get(doc, path).map(|e| &e.value)
    }

    fn string(&mut self, doc: &Node, path: &str, default: &str) -> String {
        match self.node(doc, path) {
            None => default.to_string(),
            Some(n) => match n.as_str() {
                Some(s) => s.to_string(),
                None => {
                    self.wrong_type(n, path, "a string");
                    default.to_string()
                }
            },
        }
    }

    fn opt_string(&mut self, doc: &Node, path: &str) -> Option<String> {
        let n = self.node(doc, path)?;
        match n.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                self.wrong_type(n, path, "a string");
                None
            }
        }
    }

    fn bool(&mut self, doc: &Node, path: &str, default: bool) -> bool {
        match self.node(doc, path) {
            None => default,
            Some(n) => match n.kind {
                NodeKind::Bool(b) => b,
                _ => {
                    self.wrong_type(n, path, "a boolean");
                    default
                }
            },
        }
    }

    fn int(&mut self, doc: &Node, path: &str, default: i64) -> i64 {
        match self.node(doc, path) {
            None => default,
            Some(n) => match n.kind {
                NodeKind::Int(i) => i,
                _ => {
                    self.wrong_type(n, path, "an integer");
                    default
                }
            },
        }
    }

    fn str_set(&mut self, doc: &Node, path: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let Some(n) = self.node(doc, path) else {
            return out;
        };
        let Some(items) = n.as_array() else {
            self.wrong_type(n, path, "a list of strings");
            return out;
        };
        for item in items {
            match item.as_str() {
                Some(s) => {
                    // The element's own span, kept so that a later phase can
                    // underline `nvidai` rather than the whole array. The
                    // first writer wins: after merge a duplicate is the same
                    // value, and the shallowest file is the one to name.
                    self.item_origins
                        .entry(format!("{path}/{s}"))
                        .or_insert_with(|| item.origin.clone());
                    out.insert(s.to_string());
                }
                None => self.wrong_type(item, path, "a string"),
            }
        }
        out
    }

    /// Entries of an array of tables, with their own origins for diagnostics.
    fn entries<'d>(&mut self, doc: &'d Node, path: &str) -> Vec<&'d Node> {
        let Some(n) = self.node(doc, path) else {
            return Vec::new();
        };
        match n.as_array() {
            Some(items) => items.iter().collect(),
            None => {
                self.wrong_type(n, path, "a list");
                Vec::new()
            }
        }
    }

    fn field(&mut self, entry: &Node, key: &str) -> Option<String> {
        let e = entry.as_table()?.get(key)?;
        match e.value.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                self.wrong_type(&e.value, key, "a string");
                None
            }
        }
    }

    fn required(&mut self, entry: &Node, key: &str, what: &str) -> Option<String> {
        match self.field(entry, key) {
            Some(s) => Some(s),
            None if entry.as_table().is_some_and(|t| t.contains_key(key)) => None,
            None => {
                self.errs.push(
                    Diag::error("kiln::semantic", format!("{what} is missing `{key}`"))
                        .label(&entry.origin, "this entry")
                        .help(format!("{what} needs a `{key}`")),
                );
                None
            }
        }
    }

    /// Record where one element of an identity-keyed list was written, so a
    /// later phase can underline the name itself rather than the whole array.
    /// See `Manifest::item_origins` — resolution failures are always about one
    /// element, and the array as a whole is not where the user made the
    /// mistake.
    ///
    /// Every array-of-tables in the schema is noted, not only the ones a
    /// resolution failure can name: `kiln explain packages.repo` prints one
    /// line per element with the file that asked for it, and a list that
    /// skipped this would silently print its elements with no origin at all —
    /// which reads as "nothing asked for this" rather than "Kiln did not
    /// record it".
    fn note_item(&mut self, list: &str, item: &str, entry: &Node, key: &str) {
        let origin = self.at(entry, key).clone();
        self.item_origins
            .entry(format!("{list}/{item}"))
            .or_insert(origin);
    }

    /// The origin of one field of an array-of-tables entry, so a diagnostic
    /// underlines `target = "/var/..."` rather than the whole `[[file]]` block.
    fn at<'d>(&self, entry: &'d Node, key: &str) -> &'d Origin {
        entry
            .as_table()
            .and_then(|t| t.get(key))
            .map(|e| &e.value.origin)
            .unwrap_or(&entry.origin)
    }

    fn wrong_type(&mut self, n: &Node, path: &str, want: &str) {
        self.errs.push(
            Diag::error(
                "kiln::semantic",
                format!("`{path}` must be {want}, found {}", n.type_name()),
            )
            .label(&n.origin, "here"),
        );
    }

    // -- sections ----------------------------------------------------------

    fn image(&mut self, doc: &Node) -> Image {
        Image {
            name: self.string(doc, "image.name", "system"),
            arch: self.string(doc, "image.arch", host_arch()),
        }
    }

    fn repos(&mut self, doc: &Node) -> Repos {
        let snapshot = match self.opt_string(doc, "repos.snapshot") {
            None => Snapshot::Latest,
            Some(s) if s == "latest" => Snapshot::Latest,
            Some(s) => {
                if !is_iso_date(&s) {
                    if let Some(n) = self.node(doc, "repos.snapshot") {
                        self.errs.push(
                            Diag::error("kiln::semantic", format!("`{s}` is not a snapshot date"))
                                .label(&n.origin, "here")
                                .help("write `\"latest\"` to track live mirrors, or a date like \"2026-08-24\""),
                        );
                    }
                }
                Snapshot::Date(s)
            }
        };

        let mut extra = BTreeMap::new();
        for e in self.entries(doc, "repos.extra") {
            let (Some(name), Some(server)) = (
                self.required(e, "name", "a repository"),
                self.required(e, "server", "a repository"),
            ) else {
                continue;
            };
            let key = self.field(e, "key");
            if let Some(k) = &key {
                self.hash_local(k, e, "repos.extra key");
            }
            self.note_item("repos.extra", &name, e, "name");
            extra.insert(name.clone(), ExtraRepo { name, server, key });
        }

        Repos {
            snapshot,
            mirrors: self.str_set(doc, "repos.mirrors"),
            extra,
        }
    }

    fn packages(&mut self, doc: &Node) -> PackageSet {
        let mut repo = std::collections::BTreeSet::new();
        for e in self.entries(doc, "packages.repo") {
            let Some(name) = self.required(e, "name", "a package") else {
                continue;
            };
            self.note_item("packages.repo", &name, e, "name");
            repo.insert(name);
        }

        let mut aur = BTreeMap::new();
        for e in self.entries(doc, "packages.aur") {
            let Some(name) = self.required(e, "name", "an AUR package") else {
                continue;
            };
            let commit = self.field(e, "commit");
            self.note_item("packages.aur", &name, e, "name");
            aur.insert(name.clone(), AurPackage { name, commit });
        }

        let mut build = std::collections::BTreeSet::new();
        for e in self.entries(doc, "packages.build") {
            let Some(path) = self.required(e, "path", "a PKGBUILD") else {
                continue;
            };
            self.hash_local(&path, e, "PKGBUILD directory");
            self.note_item("packages.build", &path, e, "path");
            build.insert(path);
        }

        let mut file = BTreeMap::new();
        for e in self.entries(doc, "packages.file") {
            let Some(path) = self.required(e, "path", "a local package") else {
                continue;
            };
            // "an optional integrity guarantee is not a guarantee".
            let Some(sha256) = self.required(e, "sha256", "a local package") else {
                continue;
            };
            if path.contains("://") {
                if !is_url(&path) {
                    self.errs.push(
                        Diag::error(
                            "kiln::semantic",
                            format!("local package `{path}` has an unsupported URL scheme"),
                        )
                        .label(self.at(e, "path"), "referenced here")
                        .help("only http:// and https:// are supported"),
                    );
                    continue;
                }
                // A URL is fetched during realization, not hashed here: the
                // frontend never touches the network. `sha256` is what carries
                // its identity into `config_id` instead of a local digest.
            } else {
                self.hash_local(&path, e, "local package");
            }
            if sha256.contains("://") && !is_url(&sha256) {
                self.errs.push(
                    Diag::error(
                        "kiln::semantic",
                        format!("checksum `{sha256}` has an unsupported URL scheme"),
                    )
                    .label(self.at(e, "sha256"), "referenced here")
                    .help("only http:// and https:// are supported"),
                );
                continue;
            }
            self.note_item("packages.file", &path, e, "path");
            file.insert(path.clone(), LocalPackage { path, sha256 });
        }

        PackageSet {
            repo,
            aur,
            build,
            file,
            exclude: self.str_set(doc, "packages.exclude"),
        }
    }

    fn kernel(&mut self, doc: &Node) -> Kernel {
        let mut options = BTreeMap::new();
        if let Some(n) = self.node(doc, "kernel.modules.options") {
            match n.as_table() {
                Some(t) => {
                    for (k, e) in t {
                        match e.value.as_str() {
                            Some(s) => {
                                options.insert(k.clone(), s.to_string());
                            }
                            None => self.wrong_type(&e.value, k, "a string"),
                        }
                    }
                }
                None => self.wrong_type(n, "kernel.modules.options", "a table"),
            }
        }

        let mut out_of_tree = BTreeMap::new();
        for e in self.entries(doc, "kernel.module") {
            let (Some(name), Some(source)) = (
                self.required(e, "name", "a kernel module"),
                self.required(e, "source", "a kernel module"),
            ) else {
                continue;
            };
            self.hash_local(&source, e, "kernel module source");
            self.note_item("kernel.module", &name, e, "name");
            out_of_tree.insert(name.clone(), OutOfTreeModule { name, source });
        }

        Kernel {
            package: self.string(doc, "kernel.package", "linux"),
            headers: self.bool(doc, "kernel.headers", false),
            cmdline: self.str_set(doc, "kernel.cmdline"),
            dracut_modules: self.str_set(doc, "kernel.dracut_modules"),
            modules: KernelModules {
                load: self.str_set(doc, "kernel.modules.load"),
                blacklist: self.str_set(doc, "kernel.modules.blacklist"),
                options,
            },
            out_of_tree,
        }
    }

    fn boot(&mut self, doc: &Node) -> Boot {
        let loader = match self.opt_string(doc, "boot.loader") {
            None => BootLoader::Grub2,
            Some(s) if s == "grub2" => BootLoader::Grub2,
            // Worth its own message rather than a "did you mean" against a
            // one-item list. An earlier draft of the design defaulted to
            // systemd-boot, the shipped module library said so, and it is what
            // most Arch users run — so someone writing it has a good reason to
            // expect it to work, and deserves to be told why it cannot.
            Some(s) if s == "systemd-boot" || s == "sd-boot" => {
                if let Some(n) = self.node(doc, "boot.loader") {
                    self.errs.push(
                        Diag::error(
                            "kiln::semantic",
                            "systemd-boot cannot boot an OSTree system",
                        )
                        .label(&n.origin, "not available")
                        .help(
                            "OSTree keeps /boot/loader as a symlink pair so that entry swaps \
                             are atomic, and UEFI firmware reads only FAT — so /boot cannot \
                             be the ESP, and systemd-boot cannot read the ext4 /boot OSTree \
                             needs. Kiln uses GRUB2 through libostree's own backend \
                             instead; remove this line to get it.",
                        ),
                    );
                }
                BootLoader::Grub2
            }
            Some(s) => {
                self.bad_enum(doc, "boot.loader", &s, &["grub2"]);
                BootLoader::Grub2
            }
        };
        let initramfs = match self.opt_string(doc, "boot.initramfs") {
            None => Initramfs::Dracut,
            Some(s) if s == "dracut" => Initramfs::Dracut,
            // Named rather than folded into the unknown-value message, for the
            // same reason `systemd-boot` is above: it is what an Arch user
            // reaches for, and "unknown value" would leave them looking for a
            // typo instead of reading why.
            Some(s) if s == "mkinitcpio" => {
                if let Some(n) = self.node(doc, "boot.initramfs") {
                    self.errs.push(
                        Diag::error(
                            "kiln::semantic",
                            "mkinitcpio cannot build an initramfs that boots an OSTree system",
                        )
                        .label(&n.origin, "not available")
                        .help(
                            "the sysroot pivot needs an initramfs hook, and upstream ostree                              ships one for dracut (`50ostree`, in the `ostree` package) and                              none for mkinitcpio. Writing and maintaining a boot-critical                              hook is not something Kiln does; remove this                              line to get dracut.",
                        ),
                    );
                }
                Initramfs::Dracut
            }
            Some(s) => {
                self.bad_enum(doc, "boot.initramfs", &s, &["dracut"]);
                Initramfs::Dracut
            }
        };
        let timeout = self.int(doc, "boot.timeout", 5);
        if timeout < 0 {
            if let Some(n) = self.node(doc, "boot.timeout") {
                self.errs.push(
                    Diag::error("kiln::semantic", "`boot.timeout` cannot be negative")
                        .label(&n.origin, "here")
                        .help("`0` means no menu delay"),
                );
            }
        }
        Boot {
            loader,
            timeout: timeout.max(0),
            initramfs,
        }
    }

    fn bad_enum(&mut self, doc: &Node, path: &str, got: &str, allowed: &[&str]) {
        let Some(n) = self.node(doc, path) else {
            return;
        };
        self.errs.push(
            Diag::error(
                "kiln::semantic",
                format!("unknown value `{got}` for `{path}`"),
            )
            .label(&n.origin, "here")
            .maybe_help(
                did_you_mean(got, allowed.iter().copied())
                    .or_else(|| Some(format!("`{path}` takes: {}", allowed.join(", ")))),
            ),
        );
    }

    fn systemd(&mut self, doc: &Node) -> SystemdState {
        let mut units = BTreeMap::new();
        for e in self.entries(doc, "systemd.unit") {
            let Some(name) = self.required(e, "name", "a unit") else {
                continue;
            };
            self.check_unit_name(&name, e);
            let source = self.field(e, "source");
            let content = self.field(e, "content");
            self.check_source_xor_content(e, source.as_deref(), content.as_deref(), "unit");
            if let Some(s) = &source {
                self.hash_local(s, e, "unit file");
            }
            let enable = e
                .as_table()
                .and_then(|t| t.get("enable"))
                .map(|x| matches!(x.value.kind, NodeKind::Bool(true)))
                .unwrap_or(false);
            self.note_item("systemd.unit", &name, e, "name");
            units.insert(
                name.clone(),
                UnitFile {
                    name,
                    source,
                    content,
                    enable,
                },
            );
        }

        let state = SystemdState {
            enable: self.str_set(doc, "systemd.enable"),
            disable: self.str_set(doc, "systemd.disable"),
            mask: self.str_set(doc, "systemd.mask"),
            units,
        };
        for (list, names) in [
            ("systemd.enable", &state.enable),
            ("systemd.disable", &state.disable),
            ("systemd.mask", &state.mask),
        ] {
            for n in names {
                if let Some(node) = self.node(doc, list) {
                    self.check_unit_name_at(n, &node.origin);
                }
            }
        }
        state
    }

    fn check_unit_name(&mut self, name: &str, at: &Node) {
        self.check_unit_name_at(name, &self.at(at, "name").clone());
    }

    fn check_unit_name_at(&mut self, name: &str, origin: &Origin) {
        let ok = name
            .rsplit_once('.')
            .is_some_and(|(stem, suffix)| !stem.is_empty() && UNIT_SUFFIXES.contains(&suffix));
        if !ok {
            let help = match name.rsplit_once('.') {
                Some((_, suffix)) => did_you_mean(suffix, UNIT_SUFFIXES.iter().copied())
                    .unwrap_or_else(|| {
                        format!("a unit name ends in one of: {}", UNIT_SUFFIXES.join(", "))
                    }),
                None => format!(
                    "`{name}` has no unit type; write `{name}.service` if that is what you meant"
                ),
            };
            self.errs.push(
                Diag::error(
                    "kiln::semantic",
                    format!("`{name}` is not a systemd unit name"),
                )
                .label(origin, "here")
                .help(help),
            );
        }
    }

    fn files(&mut self, doc: &Node) -> BTreeMap<String, FileEntry> {
        let mut out = BTreeMap::new();
        for e in self.entries(doc, "file") {
            let Some(target) = self.required(e, "target", "a file") else {
                continue;
            };
            self.check_target(&target, e);
            let source = self.field(e, "source");
            let content = self.field(e, "content");
            self.check_source_xor_content(e, source.as_deref(), content.as_deref(), "file");
            if let Some(s) = &source {
                self.hash_local(s, e, "file source");
            }
            let mode = self.mode(e);
            self.note_item("file", &target, e, "target");
            out.insert(
                target.clone(),
                FileEntry {
                    target,
                    source,
                    content,
                    mode,
                },
            );
        }
        out
    }

    /// "Modes are strings. TOML has no octal literal and `0755` would be a
    /// parse error or, worse, decimal 755."
    fn mode(&mut self, entry: &Node) -> Option<u32> {
        let e = entry.as_table()?.get("mode")?;
        let text = match &e.value.kind {
            NodeKind::Str(s) => s.clone(),
            NodeKind::Int(i) => {
                self.errs.push(
                    Diag::error("kiln::semantic", "`mode` must be a string")
                        .label(&e.value.origin, format!("this is the decimal number {i}"))
                        .help(format!(
                            "write `mode = \"0{i}\"` — TOML has no octal literal"
                        )),
                );
                return None;
            }
            _ => {
                self.wrong_type(&e.value, "mode", "a string like \"0755\"");
                return None;
            }
        };
        let valid = {
            let digits = text.strip_prefix('0').unwrap_or(&text);
            (3..=4).contains(&digits.len())
                && digits.chars().all(|c| ('0'..='7').contains(&c))
                && !text.is_empty()
        };
        if !valid {
            self.errs.push(
                Diag::error("kiln::semantic", format!("`{text}` is not a file mode"))
                    .label(&e.value.origin, "here")
                    .help("a mode is three or four octal digits, as a string: \"0755\""),
            );
            return None;
        }
        u32::from_str_radix(&text, 8).ok()
    }

    fn check_target(&mut self, target: &str, entry: &Node) {
        let at = &self.at(entry, "target").clone();
        if !target.starts_with('/') {
            self.errs.push(
                Diag::error(
                    "kiln::semantic",
                    format!("`{target}` is not an absolute path"),
                )
                .label(at, "here")
                .help("`target` is a real system path and must start with `/`"),
            );
            return;
        }
        if target.split('/').any(|c| c == "..") {
            self.errs.push(
                Diag::error("kiln::semantic", format!("`{target}` contains `..`"))
                    .label(at, "here")
                    .help("write the path Kiln should create, not a path to traverse"),
            );
            return;
        }
        for (prefix, note) in SEEDED_TARGETS {
            if target == *prefix || target.starts_with(&format!("{prefix}/")) {
                self.notes.push(
                    Diag::warning(
                        "kiln::semantic",
                        format!("`{target}` is seeded, not written into the image"),
                    )
                    .label(at, "here")
                    .help(*note),
                );
            }
        }
        for (prefix, why) in FORBIDDEN_TARGETS {
            if target == *prefix || target.starts_with(&format!("{prefix}/")) {
                self.errs.push(
                    Diag::error(
                        "kiln::semantic",
                        format!("Kiln cannot ship a file to `{target}`"),
                    )
                    .label(at, "here")
                    .help(*why),
                );
                return;
            }
        }
    }

    /// A file with neither is empty by accident; a file with both is ambiguous.
    fn check_source_xor_content(
        &mut self,
        at: &Node,
        source: Option<&str>,
        content: Option<&str>,
        what: &str,
    ) {
        match (source, content) {
            (None, None) => self.errs.push(
                Diag::error("kiln::semantic", format!("this {what} has no content"))
                    .label(&at.origin, "here")
                    .help(format!(
                        "give the {what} either a `source` path or inline `content`"
                    )),
            ),
            (Some(_), Some(_)) => self.errs.push(
                Diag::error(
                    "kiln::semantic",
                    format!("this {what} has both `source` and `content`"),
                )
                .label(&at.origin, "here")
                .help("pick one — Kiln will not guess which you meant"),
            ),
            _ => {}
        }
    }

    fn scripts(&mut self, doc: &Node) -> BTreeMap<String, Script> {
        let mut out = BTreeMap::new();
        for e in self.entries(doc, "script") {
            let Some(name) = self.required(e, "name", "a script") else {
                continue;
            };
            let source = self.field(e, "source");
            let content = self.field(e, "content");
            self.check_source_xor_content(e, source.as_deref(), content.as_deref(), "script");
            if let Some(s) = &source {
                self.hash_local(s, e, "script");
            }
            let after = match self.field(e, "after").as_deref() {
                None | Some("files") => ScriptPhase::Files,
                Some("packages") => ScriptPhase::Packages,
                Some(other) => {
                    self.errs.push(
                        Diag::error("kiln::semantic", format!("unknown script phase `{other}`"))
                            .label(&e.origin, "here")
                            .help("`after` is either \"packages\" or \"files\" (the default)"),
                    );
                    ScriptPhase::Files
                }
            };
            self.note_item("script", &name, e, "name");
            out.insert(
                name.clone(),
                Script {
                    name,
                    source,
                    content,
                    after,
                },
            );
        }
        out
    }

    fn system(&mut self, doc: &Node) -> SystemDefaults {
        SystemDefaults {
            hostname: self.opt_string(doc, "system.hostname"),
            timezone: self.string(doc, "system.timezone", "UTC"),
            keymap: self.string(doc, "system.keymap", "us"),
            locale: Locale {
                lang: self.string(doc, "system.locale.lang", "C.UTF-8"),
                generate: self.str_set(doc, "system.locale.generate"),
            },
        }
    }

    // -- local files -------------------------------------------------------

    /// Resolve a `source`/`path` against the config root, enforce the security
    /// boundary, and fold its digest into the configuration identity.
    fn hash_local(&mut self, rel: &str, entry: &Node, what: &str) {
        if self.digests.contains_key(rel) {
            return;
        }
        let field = ["source", "path", "key"]
            .iter()
            .find(|k| entry.as_table().is_some_and(|t| t.contains_key(**k)))
            .copied()
            .unwrap_or("source");
        let at = self.at(entry, field).clone();
        let joined = self.loader.config_root.join(rel.trim_end_matches('/'));
        let resolved = match joined.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                self.errs.push(
                    Diag::error("kiln::semantic", format!("{what} `{rel}` does not exist"))
                        .label(&at, "referenced here")
                        .help(format!("looked for {}: {e}", joined.display())),
                );
                return;
            }
        };
        if let Err(d) = self.loader.check_boundary(&resolved, &at, what) {
            self.errs.push(d);
            return;
        }
        if !resolved.starts_with(&self.loader.config_root) {
            self.loader.note_escape(resolved.clone(), at.clone());
        }
        match digest::digest(&resolved) {
            Ok(h) => {
                self.digests.insert(rel.to_string(), h);
            }
            Err(e) => self.errs.push(
                Diag::error("kiln::semantic", format!("cannot hash {what} `{rel}`: {e}"))
                    .label(&at, "referenced here"),
            ),
        }
    }
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}
