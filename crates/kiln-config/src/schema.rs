//! What the merge algebra needs to know about the schema, and nothing more.
//!
//! "Array-of-table entries merge by their identity key —
//! `target` for files, `name` for the rest." Merge is otherwise generic, so the
//! identity keys live here as data rather than being spread through the code.

/// How one array-valued key merges.
pub struct ListSpec {
    /// Dotted path from the document root.
    pub path: &'static str,
    /// Key that identifies an entry, for arrays of tables. `None` means a plain
    /// set of scalars that unions and deduplicates by value.
    pub identity: Option<&'static str>,
    /// Scalar shorthand: the key a bare string expands into.
    /// `"firefox"` ≡ `{ name = "firefox" }`.
    pub shorthand: Option<&'static str>,
}

const fn spec(
    path: &'static str,
    identity: Option<&'static str>,
    shorthand: Option<&'static str>,
) -> ListSpec {
    ListSpec {
        path,
        identity,
        shorthand,
    }
}

pub const LISTS: &[ListSpec] = &[
    spec("include", None, None),
    spec("packages.repo", Some("name"), Some("name")),
    spec("packages.aur", Some("name"), Some("name")),
    spec("packages.build", Some("path"), Some("path")),
    spec("packages.file", Some("path"), Some("path")),
    spec("packages.exclude", None, None),
    spec("repos.extra", Some("name"), Some("name")),
    spec("repos.mirrors", None, None),
    spec("kernel.cmdline", None, None),
    spec("kernel.modules.load", None, None),
    spec("kernel.modules.blacklist", None, None),
    spec("kernel.module", Some("name"), None),
    spec("systemd.enable", None, None),
    spec("systemd.disable", None, None),
    spec("systemd.mask", None, None),
    spec("systemd.unit", Some("name"), None),
    spec("file", Some("target"), None),
    spec("script", Some("name"), None),
    spec("system.locale.generate", None, None),
];

/// Keys whose value is a table of names the *user* chooses, so the schema can
/// enumerate the key but never its contents. `deny_unknown_fields` has to stop
/// at one of these, and `kiln explain kernel.modules` has to call an empty one
/// "empty" rather than "unset" — a map that nothing wrote to is not the same
/// shape of nothing as a scalar nobody set.
pub const MAPS: &[&str] = &["kernel.modules.options"];

pub fn is_map(path: &str) -> bool {
    MAPS.contains(&path)
}

pub fn list_spec(path: &str) -> Option<&'static ListSpec> {
    LISTS.iter().find(|s| s.path == path)
}

pub fn is_list(path: &str) -> bool {
    list_spec(path).is_some()
}

/// Every key the schema knows, dotted, for `deny_unknown_fields` and
/// did-you-mean. — "that is the whole language".
pub const KEYS: &[&str] = &[
    "kiln",
    "include",
    "image",
    "image.name",
    "image.arch",
    "repos",
    "repos.snapshot",
    "repos.extra",
    "repos.mirrors",
    "packages",
    "packages.repo",
    "packages.aur",
    "packages.build",
    "packages.file",
    "packages.exclude",
    "kernel",
    "kernel.package",
    "kernel.headers",
    "kernel.cmdline",
    "kernel.modules",
    "kernel.modules.load",
    "kernel.modules.blacklist",
    "kernel.modules.options",
    "kernel.module",
    "boot",
    "boot.loader",
    "boot.timeout",
    "boot.initramfs",
    "systemd",
    "systemd.enable",
    "systemd.disable",
    "systemd.mask",
    "systemd.unit",
    "file",
    "script",
    "system",
    "system.hostname",
    "system.timezone",
    "system.keymap",
    "system.locale",
    "system.locale.lang",
    "system.locale.generate",
];

/// Keys of the tables that appear inside arrays of tables, for the same reason.
pub fn entry_keys(path: &str) -> Option<&'static [&'static str]> {
    Some(match path {
        "packages.repo" | "packages.aur" => &["name", "commit"],
        "packages.build" => &["path"],
        "packages.file" => &["path", "sha256"],
        "repos.extra" => &["name", "server", "key"],
        "kernel.module" => &["name", "source"],
        "systemd.unit" => &["name", "source", "content", "enable"],
        "file" => &["source", "target", "content", "mode"],
        "script" => &["name", "source", "content", "after"],
        _ => return None,
    })
}

/// `kernel.modules.options` is a free-form map of module name to option string,
/// so its keys cannot be validated against a list.
pub fn is_open_map(path: &str) -> bool {
    path == "kernel.modules.options"
}

/// The type a scalar key takes. Checking this in the *structure* phase rather
/// than the semantic one means a file with three type errors reports all three
/// at once instead of one per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Str,
    Int,
    Bool,
    Table,
}

pub fn scalar_type(path: &str) -> Option<Ty> {
    Some(match path {
        "kiln" => Ty::Int,
        "image"
        | "repos"
        | "packages"
        | "kernel"
        | "kernel.modules"
        | "boot"
        | "systemd"
        | "system"
        | "system.locale"
        | "kernel.modules.options" => Ty::Table,
        "image.name" | "image.arch" | "repos.snapshot" | "kernel.package" | "boot.loader"
        | "boot.initramfs" | "system.hostname" | "system.timezone" | "system.keymap"
        | "system.locale.lang" => Ty::Str,
        "kernel.headers" => Ty::Bool,
        "boot.timeout" => Ty::Int,
        _ => return None,
    })
}
