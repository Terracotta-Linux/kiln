//! `kiln config get|list|set|unset|add|remove`.
//!
//! `get` answers *"set in `hardware.toml:14`, overriding
//! `@kiln/hardware/nvidia:9`"* — the payoff for carrying spans through the
//! whole frontend. `kiln explain <key>` is an alias for `kiln config get
//! <key>` (see `explain.rs`).
//!
//! Four things can be asked about, and the argument alone says which. They are
//! tried in order of how specific they are:
//!
//! 1. **An element** — `packages.repo/neovim`. Answered from `item_origins`:
//!    *which file asked for this one package.* The counterpart of `kiln why`,
//!    which answers the same question about a built image and gets "a
//!    dependency of gnome-shell" where this gets "`desktop.toml:7`".
//! 2. **A key** — `boot.timeout`. Set in one file, overriding others (
//!    rule 2), or unioned out of several (rule 1).
//! 3. **A prefix** — `boot`, `kernel.modules`, or the bare `packages`. Every
//!    key underneath it, set or not, each with the file that decided it. This
//!    is what makes `kiln config get` usable by someone who does not already
//!    know the key they want, which is most of the people who need it.
//! 4. **Nothing** — the key is unset. Three different answers, because
//!    "`boot.timeout` is 5 because that is Kiln's default", "`packages.aur` is
//!    empty" and "there is no key called `boot.timout`" are three different
//!    facts and only the last one is a mistake.
//!
//! `set`/`unset`/`add`/`remove` write straight to the TOML files under
//! `--config`, through `kiln_config::edit` — a pure, formatting-preserving
//! text mutation — and then reload the whole frontend to make sure the edit
//! did not break anything before committing it. None of them build or deploy;
//! `kiln build`/`kiln apply` are still separate, manual steps.

use kiln_config::node::{self, Node, NodeKind};
use kiln_config::{schema, Frontend};
use kiln_diag::{did_you_mean, ExitCode, Origin, Src};
use std::path::{Path, PathBuf};

pub fn get(fe: &Frontend, key: &str) -> ExitCode {
    // Leading and trailing dots are what a half-typed key looks like, and
    // `kiln config get boot.` meaning `boot` costs one line.
    let key = key.trim().trim_matches('.');

    if key == "include" {
        return includes(fe);
    }
    if let Some((list, item)) = split_element(key) {
        if let Some(code) = element(fe, list, item) {
            return code;
        }
    }
    if fe.merged.origins.contains_key(key) {
        return exact(fe, key);
    }
    if let Some(code) = prefix(fe, key) {
        return code;
    }
    unset_answer(fe, key)
}

/// `include` is the one key with no value to explain: the include graph
/// consumes it, so it is not in the merged document at all and the generic
/// paths would answer "empty" about a configuration built out of nine files.
///
/// What somebody asking about `include` wants is the graph — every file that
/// participated, which is the only documentation of what the system is made of
/// (nothing is glob-loaded, so this list is complete by construction).
fn includes(fe: &Frontend) -> ExitCode {
    heading("include");
    println!("  kind        the include graph, not a value");
    println!(
        "  {}, entry point first:",
        crate::fmt::counted(fe.files.len(), "file")
    );
    for f in &fe.files {
        println!("    {}", f.name);
    }
    println!(
        "\n  Every one of them is reachable through an explicit `include` from\n  \
         {}. Nothing is glob-loaded, so this is the whole configuration.",
        fe.files
            .first()
            .map(|f| f.name.as_str())
            .unwrap_or("the entry point")
    );
    ExitCode::Ok
}

/// One key, set by at least one file.
fn exact(fe: &Frontend, key: &str) -> ExitCode {
    let prov = &fe.merged.origins[key];

    heading(key);
    let value = node::get(&fe.merged.doc, key).map(|e| &e.value);

    // rule 1 vs rule 2: a list unions and has no winner; a scalar has
    // exactly one. Saying "overriding" about a list would be a lie about the
    // merge rules, and it is the lie a user would act on.
    if prov.is_list {
        println!(
            "  kind        a list — {} unions into it (rule 1)",
            crate::fmt::counted(prov.others.len() + 1, "file")
        );
        let items = value.and_then(Node::as_array).unwrap_or(&[]);
        // The per-element origins name the same files, one line each, and also
        // say which element came from which. When they are available, a
        // separate list of contributing files is the same information twice.
        if !elements(fe, key, items) {
            println!("  nearest     {}", prov.effective.short());
            for o in &prov.others {
                println!("              {}", o.short());
            }
        }
        println!(
            "\n  Order does not matter: every contributor's elements are in the image,\n  \
             deduplicated. `kiln config get {key}/<element>` asks about one of them."
        );
    } else {
        if let Some(v) = value {
            println!("  value       {}", render(v, key));
        }
        println!("  set in      {}", prov.effective.short());
        match prov.others.len() {
            0 => println!("  overriding  nothing — no other file sets it"),
            _ => {
                for o in &prov.others {
                    println!("  overriding  {}", o.short());
                }
                println!(
                    "\n  The includer wins over what it includes (rule 2). Two files\n  \
                     at the same depth disagreeing would have been an error, not this."
                );
            }
        }
    }
    ExitCode::Ok
}

/// One element of a list, with the file that asked for it.
///
/// Returns `None` when the part before the slash does not name a list, so that
/// a `/` in something that is not an element spelling falls through to the
/// other three answers rather than being claimed by this one.
fn element(fe: &Frontend, list: &str, item: &str) -> Option<ExitCode> {
    if !schema::is_list(list) {
        return None;
    }
    let key = format!("{list}/{item}");

    let Some(origin) = fe.manifest.item_origins.get(&key) else {
        heading(&key);
        println!("  not in      the merged configuration");
        let siblings: Vec<&str> = elements_of(fe, list).into_iter().map(|(n, _)| n).collect();
        match did_you_mean(item, siblings.iter().copied()) {
            Some(h) => println!("  {h}"),
            None if siblings.is_empty() => println!("  `{list}` is empty in this configuration"),
            None => {
                println!("\n  `{list}` has:");
                for s in &siblings {
                    println!("    {s}");
                }
            }
        }
        return Some(ExitCode::Config);
    };

    heading(&key);
    println!("  asked for   {}", origin.short());
    println!("  in          {list}");
    if list.starts_with("packages.") {
        println!(
            "\n  That is where it was written down. `kiln why {item}` answers the other\n  \
             half — whether a built image contains it because you asked, or because\n  \
             something else depends on it."
        );
    }
    Some(ExitCode::Ok)
}

/// Every key under a dotted prefix: `kiln config get boot`, `kiln config get
/// packages`, `kiln config get kernel.modules`.
///
/// Both halves are listed — the keys some file set and the keys nothing set —
/// because "show me all of `boot`" is the question, and an answer holding only
/// the lines the user already wrote is the one thing they did not need to ask
/// for.
fn prefix(fe: &Frontend, key: &str) -> Option<ExitCode> {
    let keys = under(fe, Some(key));
    if keys.is_empty() {
        return None;
    }

    heading(key);
    println!("  a group of keys, not a value of its own\n");
    let width = keys.iter().map(String::len).max().unwrap_or(0);

    for k in &keys {
        match fe.merged.origins.get(k.as_str()) {
            Some(prov) => {
                // A list's own value can be twenty package names wide, which
                // turns a listing into a wall. The count is what a listing is
                // for; `kiln config get <that key>` prints the elements.
                let value = match prov.is_list {
                    true => crate::fmt::counted(count(fe, k), "element"),
                    false => node::get(&fe.merged.doc, k)
                        .map(|e| render(&e.value, k))
                        .unwrap_or_default(),
                };
                println!(
                    "  {k:<width$}  {value}\n  {:<width$}  {} {}",
                    "",
                    if prov.is_list {
                        "unions from"
                    } else {
                        "set in"
                    },
                    prov.effective.short()
                );
            }
            None => match default_for(k) {
                Some(d) => println!(
                    "  {k:<width$}  {}\n  {:<width$}  Kiln's default {}",
                    d.value,
                    "",
                    d.note.map(|n| format!(" — {n}")).unwrap_or_default()
                ),
                None if schema::is_list(k) || schema::is_map(k) => {
                    println!("  {k:<width$}  empty")
                }
                None => println!("  {k:<width$}  unset"),
            },
        }
    }
    println!("\n  `kiln config get <one of these>` for the whole story of one of them.");
    Some(ExitCode::Ok)
}

/// Every key under a prefix, or — with `prefix: None` — every leaf key the
/// schema knows at all (`kiln config list`'s whole-configuration form).
/// Schema keys come in the order the schema declares them, then anything
/// else. Declaration order groups keys the way the documentation does, where
/// sorting would put `boot.initramfs` above `boot.loader` for no reason a
/// reader could name.
fn under(fe: &Frontend, prefix: Option<&str>) -> Vec<String> {
    let dotted = match prefix {
        Some(key) => format!("{key}."),
        None => String::new(),
    };
    let mut out: Vec<String> = schema::KEYS
        .iter()
        .filter(|k| k.starts_with(&dotted))
        .map(|k| (*k).to_string())
        .collect();
    // Keys the schema cannot enumerate, because their last segment is a name
    // the user chose. `kernel.modules.options.<module>` is the only one today.
    let mut named: Vec<String> = fe
        .merged
        .origins
        .keys()
        .filter(|k| k.starts_with(&dotted) && !out.contains(k))
        .cloned()
        .collect();
    named.sort();
    out.extend(named);

    // `kernel.modules` is a heading, not a value: it holds `kernel.modules.load`
    // and nothing of its own. Listing it would put a row reading "unset" above
    // three rows that are set, which is the opposite of what it means.
    let groups: Vec<String> = out
        .iter()
        .filter(|k| out.iter().any(|other| other.starts_with(&format!("{k}."))))
        .cloned()
        .collect();
    out.retain(|k| !groups.contains(k));

    // `kiln`/`include` describe the file, not the image (`merge::strip_control_keys`
    // drops them the same way before they ever reach a Manifest); listing
    // everything should not surface them as if they were image settings.
    if prefix.is_none() {
        out.retain(|k| k != "kiln" && k != "include");
    }
    out
}

/// How many elements a list-valued key ended up with.
fn count(fe: &Frontend, key: &str) -> usize {
    node::get(&fe.merged.doc, key)
        .and_then(|e| e.value.as_array().map(<[Node]>::len))
        .unwrap_or(0)
}

/// Nothing set it. Which of the three "nothing" answers applies depends on
/// whether the schema knows the key, and on whether Kiln has an answer for it
/// anyway.
fn unset_answer(fe: &Frontend, key: &str) -> ExitCode {
    if let Some(d) = default_for(key) {
        heading(key);
        println!("  value       {}", d.value);
        println!("  set in      nothing — this is Kiln's default");
        if let Some(n) = d.note {
            println!("  because     {n}");
        }
        println!("\n  You write the key only to disagree with it.");
        return ExitCode::Ok;
    }

    if schema::KEYS.contains(&key) {
        heading(key);
        if schema::is_list(key) {
            println!("  value       empty — no file contributes to it");
            println!(
                "\n  It unions across files (rule 1), so any file this configuration\n  \
                 includes could add to it without naming it here. None does."
            );
        } else {
            println!("  value       unset, and Kiln has no default for it");
        }
        return ExitCode::Ok;
    }

    // A key the schema does not have. This is the only one of the four answers
    // that is a mistake, so it is the only one that fails.
    eprint!("\x1b[1;31merror\x1b[0m no key `{key}` in the Kiln schema");
    let known = candidates(fe);
    match did_you_mean(key, known.iter().map(String::as_str)) {
        Some(h) => eprintln!(" — {h}"),
        None => {
            eprintln!();
            eprintln!("\nThe top level is: {}", tops().join(", "));
            eprintln!("`kiln config get <one of those>` lists what is under it.");
        }
    }
    ExitCode::Config
}

/// What a misspelling could have meant: every key in the schema, every key this
/// configuration actually sets — `kernel.modules.options.*` is per-module and
/// so is in one list and not the other — and every element of every list.
fn candidates(fe: &Frontend) -> Vec<String> {
    let mut out: Vec<String> = schema::KEYS.iter().map(|k| (*k).to_string()).collect();
    out.extend(fe.merged.origins.keys().cloned());
    out.extend(fe.manifest.item_origins.keys().cloned());
    out.sort();
    out.dedup();
    out
}

fn tops() -> Vec<&'static str> {
    let mut out: Vec<&str> = schema::KEYS
        .iter()
        .filter(|k| !k.contains('.'))
        .copied()
        .collect();
    out.sort();
    out
}

/// `packages.repo/neovim` → `("packages.repo", "neovim")`. Split at the *first*
/// slash: a `[[file]]` target is a path, and everything after the list name
/// belongs to it, so `file//etc/motd` is `file` and `/etc/motd`.
fn split_element(key: &str) -> Option<(&str, &str)> {
    let (list, item) = key.split_once('/')?;
    (!item.is_empty()).then_some((list, item))
}

/// One line per element of a list, each with the file that asked for it.
/// `false` when the list's elements have no recorded identity and the caller
/// should fall back to naming the contributing files.
fn elements(fe: &Frontend, key: &str, items: &[Node]) -> bool {
    let named = elements_of(fe, key);
    if named.len() != items.len() || named.is_empty() {
        // A list carrying a type error dropped an element on the way to the
        // manifest, so the two disagree and the origins cannot be trusted to
        // line up with the values. Print the values and let the caller name
        // the files.
        println!("  {}:", crate::fmt::counted(items.len(), "element"));
        for i in items {
            println!("    {}", render(i, key));
        }
        return false;
    }

    println!(
        "  {}, and who asked for each:",
        crate::fmt::counted(items.len(), "element")
    );
    let width = named.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, origin) in &named {
        println!("    {name:<width$}  {}", origin.short());
    }
    true
}

/// The identity of every element of a list, in canonical order, paired with
/// where it was written. Reads `item_origins` rather than the merged tree,
/// because that is what the identities are keyed by and it is also where
/// shorthand expansion has already been undone: `packages.repo/neovim`, not
/// `{ name = "neovim" }`.
fn elements_of<'a>(fe: &'a Frontend, key: &str) -> Vec<(&'a str, &'a Origin)> {
    let prefix = format!("{key}/");
    fe.manifest
        .item_origins
        .iter()
        .filter_map(|(k, o)| k.strip_prefix(&prefix).map(|name| (name, o)))
        .collect()
}

fn heading(key: &str) {
    println!("\x1b[1m{key}\x1b[0m");
}

/// What Kiln does when a key is absent.
///
/// This has to match the schema's defaults exactly: a default here that
/// is not there, or there and not here, is a bug in one of the two. The note is
/// kept apart from the value so a listing can align a column of values and
/// still carry the "why".
struct Fallback {
    value: String,
    note: Option<&'static str>,
}

fn default_for(key: &str) -> Option<Fallback> {
    let (value, note): (&str, Option<&'static str>) = match key {
        "image.name" => ("\"system\"", Some("it only names the OSTree ref")),
        "image.arch" => {
            return Some(Fallback {
                value: format!("\"{}\"", kiln_manifest::host_arch()),
                note: Some("the host's architecture"),
            })
        }
        "repos.snapshot" => ("\"latest\"", Some("rolling, like Arch")),
        "repos.mirrors" => (
            "the Arch geo mirror",
            Some("works everywhere, including CI"),
        ),
        "kernel.package" => ("\"linux\"", None),
        "kernel.headers" => (
            "false",
            Some("headers are a module's build-time dependency, not image content"),
        ),
        "boot.loader" => ("\"grub2\"", Some("the only supported value")),
        "boot.timeout" => ("5", None),
        "boot.initramfs" => ("\"dracut\"", Some("the only supported value")),
        "system.timezone" => ("\"UTC\"", None),
        "system.keymap" => ("\"us\"", None),
        "system.locale.lang" => ("\"C.UTF-8\"", None),
        "system.hostname" => ("unset", Some("systemd's own default applies")),
        _ => return None,
    };
    Some(Fallback {
        value: value.to_string(),
        note,
    })
}

fn render(n: &Node, key: &str) -> String {
    match &n.kind {
        NodeKind::Array(items) => {
            let parts: Vec<String> = items.iter().map(|i| render(i, key)).collect();
            format!("[{}]", parts.join(", "))
        }
        // Collapse expanded shorthand back to what the user wrote: showing
        // `{ name = "firefox" }` for a line that said `"firefox"` is an
        // implementation detail leaking into an explanation.
        NodeKind::Table(t) => {
            let primary = schema::list_spec(key).and_then(|s| s.shorthand);
            if let (Some(p), 1) = (primary, t.len()) {
                if let Some(e) = t.get(p) {
                    return render(&e.value, key);
                }
            }
            let parts: Vec<String> = t
                .iter()
                .map(|(k, e)| format!("{k} = {}", render(&e.value, key)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        _ => n.render(),
    }
}

// ---------------------------------------------------------------------------
// `kiln config list [<prefix>]`
// ---------------------------------------------------------------------------

/// A flattened, origin-free view of every resolved key (or every key under a
/// prefix): one `key  value` line each, for scanning rather than for the
/// whole story of one key — that is `get`'s job.
pub fn list(fe: &Frontend, prefix: Option<&str>) -> ExitCode {
    let prefix = prefix.map(|p| p.trim().trim_matches('.'));
    if prefix == Some("include") {
        return includes(fe);
    }

    let mut keys = under(fe, prefix);
    if keys.is_empty() {
        match prefix {
            Some(p) if schema::KEYS.contains(&p) || fe.merged.origins.contains_key(p) => {
                keys = vec![p.to_string()];
            }
            Some(p) => {
                eprint!("\x1b[1;31merror\x1b[0m no key `{p}` in the Kiln schema");
                match did_you_mean(p, candidates(fe).iter().map(String::as_str)) {
                    Some(h) => eprintln!(" — {h}"),
                    None => eprintln!(),
                }
                return ExitCode::Config;
            }
            None => {}
        }
    }

    let width = keys.iter().map(String::len).max().unwrap_or(0);
    for k in &keys {
        println!("{k:<width$}  {}", resolved_display(fe, k));
    }
    ExitCode::Ok
}

fn resolved_display(fe: &Frontend, key: &str) -> String {
    if let Some(prov) = fe.merged.origins.get(key) {
        match node::get(&fe.merged.doc, key) {
            Some(e) => render(&e.value, key),
            None if prov.is_list => "[]".to_string(),
            None => String::new(),
        }
    } else if let Some(d) = default_for(key) {
        d.value
    } else if schema::is_list(key) || schema::is_map(key) {
        "[]".to_string()
    } else {
        "unset".to_string()
    }
}

// ---------------------------------------------------------------------------
// `kiln config set|unset|add|remove`
// ---------------------------------------------------------------------------

pub fn set(
    fe: &Frontend,
    config: Option<&Path>,
    opts: &kiln_config::Options,
    key: &str,
    raw_value: &str,
    file: Option<&Path>,
) -> ExitCode {
    let key = key.trim().trim_matches('.');
    let Some(ty) = schema::scalar_type(key) else {
        return not_a_scalar_error(fe, key);
    };
    let target = match writable_target(fe, key, file) {
        Ok(p) => p,
        Err(c) => return c,
    };
    let original = match std::fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) => return read_error(&target, &e),
    };
    let new_text = match kiln_config::edit::set_scalar(&original, key, raw_value, ty) {
        Ok(t) => t,
        Err(diag) => return edit_error(&diag),
    };
    finish(
        &target,
        &original,
        new_text,
        config,
        opts,
        &format!("set `{key}` in {}", display_name(fe, &target)),
    )
}

pub fn unset(
    fe: &Frontend,
    config: Option<&Path>,
    opts: &kiln_config::Options,
    key: &str,
    file: Option<&Path>,
) -> ExitCode {
    let key = key.trim().trim_matches('.');
    if schema::scalar_type(key).is_none() {
        return not_a_scalar_error(fe, key);
    }
    let target = match writable_target(fe, key, file) {
        Ok(p) => p,
        Err(c) => return c,
    };
    let original = match std::fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) => return read_error(&target, &e),
    };
    let (new_text, was_present) = match kiln_config::edit::unset_scalar(&original, key) {
        Ok(r) => r,
        Err(diag) => return edit_error(&diag),
    };
    if !was_present {
        println!("`{key}` is already unset in {}", display_name(fe, &target));
        return ExitCode::Ok;
    }
    finish(
        &target,
        &original,
        new_text,
        config,
        opts,
        &format!("unset `{key}` in {}", display_name(fe, &target)),
    )
}

pub fn add(
    fe: &Frontend,
    config: Option<&Path>,
    opts: &kiln_config::Options,
    key: &str,
    item: &str,
    file: Option<&Path>,
) -> ExitCode {
    let key = key.trim().trim_matches('.');
    if !schema::is_scalar_list(key) {
        return not_a_scalar_list_error(fe, key);
    }
    if merged_list_values(fe, key).iter().any(|v| v == item) {
        println!("`{item}` is already in `{key}`");
        return ExitCode::Ok;
    }
    let target = match add_target(fe, file) {
        Ok(p) => p,
        Err(c) => return c,
    };
    let original = match std::fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) => return read_error(&target, &e),
    };
    let new_text = match kiln_config::edit::add_list_item(&original, key, item) {
        Ok((t, _added)) => t,
        Err(diag) => return edit_error(&diag),
    };
    finish(
        &target,
        &original,
        new_text,
        config,
        opts,
        &format!("added `{item}` to `{key}` in {}", display_name(fe, &target)),
    )
}

pub fn remove(
    fe: &Frontend,
    config: Option<&Path>,
    opts: &kiln_config::Options,
    key: &str,
    item: &str,
    file: Option<&Path>,
) -> ExitCode {
    let key = key.trim().trim_matches('.');
    if !schema::is_scalar_list(key) {
        return not_a_scalar_list_error(fe, key);
    }

    let hits = contributors(fe, key, item);
    let target: Src = if let Some(f) = file {
        let resolved = std::fs::canonicalize(f).unwrap_or_else(|_| f.to_path_buf());
        match hits.iter().find(|s| s.path == resolved) {
            Some(s) => (*s).clone(),
            None => {
                eprintln!(
                    "\x1b[1;31merror\x1b[0m `{}` does not contribute `{item}` to `{key}`",
                    f.display()
                );
                return ExitCode::Config;
            }
        }
    } else {
        match hits.as_slice() {
            [] => {
                eprint!("\x1b[1;31merror\x1b[0m `{item}` is not in `{key}`");
                let elements = merged_list_values(fe, key);
                match did_you_mean(item, elements.iter().map(String::as_str)) {
                    Some(h) => eprintln!(" — {h}"),
                    None => eprintln!(),
                }
                return ExitCode::Config;
            }
            [one] => {
                if !one.path.starts_with(&fe.config_root) {
                    eprintln!(
                        "\x1b[1;31merror\x1b[0m `{item}` comes from `{}`, a shipped module\n\n\
                         There is no override for a list element (rule 1: lists union) — edit \
                         the module or drop the include.",
                        one.name
                    );
                    return ExitCode::Config;
                }
                (*one).clone()
            }
            many => {
                eprintln!(
                    "\x1b[1;31merror\x1b[0m `{item}` is contributed by {} files:",
                    many.len()
                );
                for s in many {
                    eprintln!("    {}", s.name);
                }
                eprintln!(
                    "\nRemoving it from only one would not remove it from the built image — it \
                     still unions in from the rest. Target one with --file."
                );
                return ExitCode::Config;
            }
        }
    };

    let original = match std::fs::read_to_string(&target.path) {
        Ok(t) => t,
        Err(e) => return read_error(&target.path, &e),
    };
    let new_text = match kiln_config::edit::remove_list_item(&original, key, item) {
        Ok((t, _removed)) => t,
        Err(diag) => return edit_error(&diag),
    };
    finish(
        &target.path,
        &original,
        new_text,
        config,
        opts,
        &format!("removed `{item}` from `{key}` in {}", target.name),
    )
}

/// Which file `set`/`unset` should write into.
///
/// `--file` wins outright, but only when it is already part of this
/// configuration's include graph — there is no scaffolding of a new file or a
/// new `include` line here. Otherwise: the file currently winning the key
/// (rule 2 — the includer wins — means editing that file is exactly what
/// changes the resolved value), refused when that file is a shipped module
/// rather than something under the config root; or, if nothing sets the key
/// at all, the entry point.
fn writable_target(fe: &Frontend, key: &str, file: Option<&Path>) -> Result<PathBuf, ExitCode> {
    if let Some(f) = file {
        return resolve_file_arg(fe, f).map(|s| s.path.clone());
    }
    if let Some(prov) = fe.merged.origins.get(key) {
        let path = prov.effective.path().to_path_buf();
        if !path.starts_with(&fe.config_root) {
            eprintln!(
                "\x1b[1;31merror\x1b[0m `{key}` is set by `{}`, a shipped module\n\nSet it in \
                 your own configuration instead — the includer always wins (rule 2).",
                prov.effective.file.name
            );
            return Err(ExitCode::Config);
        }
        return Ok(path);
    }
    entry_file(fe)
}

/// Where `add` writes when no `--file` is given: always the entry point,
/// since a list's elements union from everywhere and there is no "file
/// currently winning" to prefer the way a scalar has one.
fn add_target(fe: &Frontend, file: Option<&Path>) -> Result<PathBuf, ExitCode> {
    match file {
        Some(f) => resolve_file_arg(fe, f).map(|s| s.path.clone()),
        None => entry_file(fe),
    }
}

fn entry_file(fe: &Frontend) -> Result<PathBuf, ExitCode> {
    fe.files.first().map(|s| s.path.clone()).ok_or_else(|| {
        eprintln!("\x1b[1;31merror\x1b[0m no configuration file to write to");
        ExitCode::Config
    })
}

fn resolve_file_arg<'a>(fe: &'a Frontend, file: &Path) -> Result<&'a Src, ExitCode> {
    let resolved = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    fe.files.iter().find(|s| s.path == resolved).ok_or_else(|| {
        eprintln!(
            "\x1b[1;31merror\x1b[0m `--file {}` is not part of this configuration's include \
             graph",
            file.display()
        );
        ExitCode::Config
    })
}

/// Every file whose *own* text literally contains `item` in `key`'s array —
/// not the merged result, which no longer says which file to touch.
fn contributors<'a>(fe: &'a Frontend, key: &str, item: &str) -> Vec<&'a Src> {
    fe.files
        .iter()
        .filter(|s| kiln_config::edit::list_contains(&s.text, key, item))
        .collect()
}

fn merged_list_values(fe: &Frontend, key: &str) -> Vec<String> {
    node::get(&fe.merged.doc, key)
        .and_then(|e| e.value.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn display_name(fe: &Frontend, path: &Path) -> String {
    fe.files
        .iter()
        .find(|s| s.path == path)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| path.display().to_string())
}

fn not_a_scalar_error(fe: &Frontend, key: &str) -> ExitCode {
    if schema::is_list(key) {
        eprintln!(
            "\x1b[1;31merror\x1b[0m `{key}` is a list; use `kiln config add`/`kiln config remove`"
        );
    } else if schema::is_map(key) || schema::KEYS.contains(&key) {
        eprintln!(
            "\x1b[1;31merror\x1b[0m `{key}` is a table, not a scalar `kiln config set` can write"
        );
    } else {
        eprint!("\x1b[1;31merror\x1b[0m no key `{key}` in the Kiln schema");
        match did_you_mean(key, candidates(fe).iter().map(String::as_str)) {
            Some(h) => eprintln!(" — {h}"),
            None => eprintln!(),
        }
    }
    ExitCode::Config
}

fn not_a_scalar_list_error(fe: &Frontend, key: &str) -> ExitCode {
    if schema::is_list(key) {
        eprintln!(
            "\x1b[1;31merror\x1b[0m `{key}` is a keyed list — its entries have their own \
             fields, not just a value — which `kiln config add`/`remove` do not support yet"
        );
    } else if schema::scalar_type(key).is_some()
        || schema::is_map(key)
        || schema::KEYS.contains(&key)
    {
        eprintln!("\x1b[1;31merror\x1b[0m `{key}` is not a list; use `kiln config set`/`unset`");
    } else {
        eprint!("\x1b[1;31merror\x1b[0m no key `{key}` in the Kiln schema");
        match did_you_mean(key, candidates(fe).iter().map(String::as_str)) {
            Some(h) => eprintln!(" — {h}"),
            None => eprintln!(),
        }
    }
    ExitCode::Config
}

fn read_error(path: &Path, e: &std::io::Error) -> ExitCode {
    eprintln!("\x1b[1;31merror\x1b[0m reading {}: {e}", path.display());
    ExitCode::System
}

fn edit_error(diag: &kiln_diag::Diag) -> ExitCode {
    eprint!("{}", kiln_diag::render(diag));
    ExitCode::Config
}

/// Write the mutated text, then reload the whole frontend to make sure the
/// edit did not break anything. On failure, the original bytes are restored
/// before returning — nothing is left half-changed.
fn write_and_revalidate(
    target: &Path,
    original: &str,
    new_text: String,
    config: Option<&Path>,
    opts: &kiln_config::Options,
) -> Result<(), kiln_diag::Errors> {
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(".kiln-tmp");
    let tmp = PathBuf::from(tmp);

    let write_failed = |e: std::io::Error, path: &Path| {
        let mut errs = kiln_diag::Errors::new();
        errs.push(kiln_diag::Diag::error(
            "kiln::config",
            format!("writing {}: {e}", path.display()),
        ));
        errs
    };

    if let Err(e) = std::fs::write(&tmp, &new_text) {
        return Err(write_failed(e, &tmp));
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_failed(e, target));
    }

    match kiln_config::load(config, opts) {
        Ok(_) => Ok(()),
        Err(errs) => {
            let _ = std::fs::write(target, original);
            Err(errs)
        }
    }
}

fn finish(
    target: &Path,
    original: &str,
    new_text: String,
    config: Option<&Path>,
    opts: &kiln_config::Options,
    ok_message: &str,
) -> ExitCode {
    match write_and_revalidate(target, original, new_text, config, opts) {
        Ok(()) => {
            println!("{ok_message}");
            ExitCode::Ok
        }
        Err(errs) => {
            eprintln!(
                "\x1b[1;31merror\x1b[0m the edit would make the configuration invalid; nothing \
                 was changed\n"
            );
            eprint!("{}", kiln_diag::render_all(&errs));
            ExitCode::Config
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `default_for` is a third copy of the schema's defaults — after
    /// `kiln-manifest`'s `Default` impls and `kiln-config`'s literal fallbacks
    /// — and the only one a user ever reads. This is what keeps it honest: a
    /// default changed in the schema and not here now fails a test rather than
    /// making `kiln config get` confidently wrong.
    #[test]
    fn every_documented_default_is_the_one_validation_produces() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("crates/kiln-cli has a workspace root");
        let opts = kiln_config::Options {
            allow_external_sources: false,
            module_root: Some(root.join("modules")),
        };
        // `minimal` sets nothing but its include, so every key below is at its
        // default.
        let fe = kiln_config::load(Some(&root.join("tests/corpus/valid/minimal")), &opts)
            .expect("the minimal corpus configuration loads");
        let m = &fe.manifest;

        let quoted = |s: &str| format!("\"{s}\"");
        let actual: &[(&str, String)] = &[
            ("image.name", quoted(&m.image.name)),
            ("image.arch", quoted(&m.image.arch)),
            ("kernel.package", quoted(&m.kernel.package)),
            ("kernel.headers", m.kernel.headers.to_string()),
            ("boot.timeout", m.boot.timeout.to_string()),
            ("system.timezone", quoted(&m.system.timezone)),
            ("system.keymap", quoted(&m.system.keymap)),
            ("system.locale.lang", quoted(&m.system.locale.lang)),
        ];
        for (key, value) in actual {
            let documented = default_for(key)
                .unwrap_or_else(|| panic!("`kiln config get` documents no default for `{key}`"));
            assert_eq!(
                &documented.value, value,
                "`kiln config get` says `{key}` defaults to {} but validation produces {value}",
                documented.value
            );
        }

        // The keys whose default is described rather than shown, because there
        // is no single value to print.
        for key in ["repos.snapshot", "repos.mirrors", "system.hostname"] {
            assert!(
                default_for(key).is_some(),
                "`kiln config get` documents no default for `{key}`"
            );
        }
        assert!(m.system.hostname.is_none());
        assert_eq!(m.repos.snapshot, kiln_manifest::Snapshot::Latest);
    }
}
