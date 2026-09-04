//! `kiln explain <key>`.
//!
//! > `kiln explain kernel.cmdline` answering *"set in `hardware.toml:14`,
//! > overriding `@kiln/hardware/nvidia:9`"* is the payoff for carrying spans
//! > through the whole frontend.
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
//!    is what makes `kiln explain` usable by someone who does not already know
//!    the key they want, which is most of the people who need it.
//! 4. **Nothing** — the key is unset. Three different answers, because
//!    "`boot.timeout` is 5 because that is Kiln's default", "`packages.aur` is
//!    empty" and "there is no key called `boot.timout`" are three different
//!    facts and only the last one is a mistake.

use kiln_config::node::{self, Node, NodeKind};
use kiln_config::{schema, Frontend};
use kiln_diag::{did_you_mean, ExitCode, Origin};

pub fn run(fe: &Frontend, key: &str) -> ExitCode {
    // Leading and trailing dots are what a half-typed key looks like, and
    // `kiln explain boot.` meaning `boot` costs one line.
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
    unset(fe, key)
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
    println!("  {}, entry point first:", plural(fe.files.len(), "file"));
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
            plural(prov.others.len() + 1, "file")
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
             deduplicated. `kiln explain {key}/<element>` asks about one of them."
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

/// Every key under a dotted prefix: `kiln explain boot`, `kiln explain
/// packages`, `kiln explain kernel.modules`.
///
/// Both halves are listed — the keys some file set and the keys nothing set —
/// because "show me all of `boot`" is the question, and an answer holding only
/// the lines the user already wrote is the one thing they did not need to ask
/// for.
fn prefix(fe: &Frontend, key: &str) -> Option<ExitCode> {
    let keys = under(fe, key);
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
                // for; `kiln explain <that key>` prints the elements.
                let value = match prov.is_list {
                    true => plural(count(fe, k), "element"),
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
    println!("\n  `kiln explain <one of these>` for the whole story of one of them.");
    Some(ExitCode::Ok)
}

/// Every key under a prefix: schema keys in the order the schema declares
/// them, then anything else. Declaration order groups keys the way the documentation
/// does, where sorting would put `boot.initramfs` above `boot.loader` for no
/// reason a reader could name.
fn under(fe: &Frontend, key: &str) -> Vec<String> {
    let dotted = format!("{key}.");
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
fn unset(fe: &Frontend, key: &str) -> ExitCode {
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
            eprintln!("`kiln explain <one of those>` lists what is under it.");
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
        println!("  {}:", plural(items.len(), "element"));
        for i in items {
            println!("    {}", render(i, key));
        }
        return false;
    }

    println!(
        "  {}, and who asked for each:",
        plural(items.len(), "element")
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

fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
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
