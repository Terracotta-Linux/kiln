//! `kiln show` and the `kiln check --offline` summary.
//!
//! never print an OSTree checksum where a generation number would do, and
//! keep identities short outside `--verbose`.

use kiln_manifest::*;

pub fn summary(m: &Manifest, files: &[kiln_diag::Src], verbose: bool) {
    let id = m.config_id();
    println!(
        "\x1b[1m{}\x1b[0m  {}  config {}",
        m.image.name,
        m.image.arch,
        if verbose { id.to_string() } else { id.short() }
    );

    row(
        "packages",
        &[
            count("repo", m.packages.repo.len()),
            count("aur", m.packages.aur.len()),
            count("build", m.packages.build.len()),
            count("local", m.packages.file.len()),
            count("excluded", m.packages.exclude.len()),
        ],
    );
    row(
        "kernel",
        &[
            Some(m.kernel.package.clone()),
            count("cmdline", m.kernel.cmdline.len()),
            count(
                "modules",
                m.kernel.modules.load.len() + m.kernel.modules.blacklist.len(),
            ),
            count("out-of-tree", m.kernel.out_of_tree.len()),
        ],
    );
    row(
        "systemd",
        &[
            count("enabled", m.systemd.enable.len()),
            count("disabled", m.systemd.disable.len()),
            count("masked", m.systemd.mask.len()),
            count("units", m.systemd.units.len()),
        ],
    );
    row(
        "content",
        &[
            count("files", m.files.len()),
            count("scripts", m.scripts.len()),
            count("hashed inputs", m.local_digests.len()),
        ],
    );
    row(
        "repos",
        &[
            Some(match &m.repos.snapshot {
                Snapshot::Latest => "rolling".to_string(),
                Snapshot::Date(d) => format!("pinned {d}"),
            }),
            count("extra", m.repos.extra.len()),
        ],
    );

    // Empty only when the manifest came out of a commit rather than off disk
    // (`kiln show <gen>`): that generation was built from configuration files,
    // but they are not what is being read here and `sources 0` would say
    // something false about it. A frontend always has at least `system.toml`.
    if !files.is_empty() {
        println!("  {:<12} {}", "sources", files.len());
    }
    if verbose {
        for f in files {
            println!("  {:<12}   {}", "", f.name);
        }
        for (path, hash) in &m.local_digests {
            println!("  {:<12}   {}  {path}", "", hash.short());
        }
    }
}

fn count(label: &str, n: usize) -> Option<String> {
    (n > 0).then(|| format!("{n} {label}"))
}

fn row(label: &str, parts: &[Option<String>]) {
    let joined: Vec<&str> = parts.iter().filter_map(|p| p.as_deref()).collect();
    if joined.is_empty() {
        return;
    }
    println!("  {label:<12} {}", joined.join(", "));
}

/// The full merged manifest, for `kiln show`.
pub fn detail(m: &Manifest) {
    println!("\nimage        {} ({})", m.image.name, m.image.arch);
    list("packages.repo", m.packages.repo.iter().cloned());
    list("packages.aur", m.packages.aur.keys().cloned());
    list("packages.build", m.packages.build.iter().cloned());
    list("packages.exclude", m.packages.exclude.iter().cloned());
    println!(
        "kernel       {} (headers: {})",
        m.kernel.package, m.kernel.headers
    );
    list("kernel.cmdline", m.kernel.cmdline.iter().cloned());
    list("modules.load", m.kernel.modules.load.iter().cloned());
    list(
        "modules.blacklist",
        m.kernel.modules.blacklist.iter().cloned(),
    );
    println!(
        "boot         {} timeout={} initramfs={}",
        match m.boot.loader {
            BootLoader::Grub2 => "grub2",
        },
        m.boot.timeout,
        match m.boot.initramfs {
            Initramfs::Dracut => "dracut",
        }
    );
    list("systemd.enable", m.systemd.enable.iter().cloned());
    list("systemd.disable", m.systemd.disable.iter().cloned());
    list("systemd.mask", m.systemd.mask.iter().cloned());
    for (name, u) in &m.systemd.units {
        println!(
            "unit         {name}{}",
            if u.enable { " (enabled)" } else { "" }
        );
    }
    for (target, f) in &m.files {
        let from = f.source.clone().unwrap_or_else(|| "<inline>".into());
        let mode = f.mode.map(|m| format!(" {m:04o}")).unwrap_or_default();
        println!("file         {target} ← {from}{mode}");
    }
    for (name, s) in &m.scripts {
        println!(
            "script       {name} (after {})",
            match s.after {
                ScriptPhase::Packages => "packages",
                ScriptPhase::Files => "files",
            }
        );
    }
    println!(
        "system       tz={} keymap={} lang={}{}",
        m.system.timezone,
        m.system.keymap,
        m.system.locale.lang,
        m.system
            .hostname
            .as_ref()
            .map(|h| format!(" hostname={h}"))
            .unwrap_or_default()
    );
    println!("\nconfig_id    {}", m.config_id());
}

fn list(label: &str, items: impl Iterator<Item = String>) {
    let v: Vec<String> = items.collect();
    if !v.is_empty() {
        println!("{label:<12} {}", v.join(" "));
    }
}
