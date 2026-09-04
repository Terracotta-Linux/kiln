//! Reporting `/etc` drift in `kiln status`.
//!
//! `kiln-ostree::drift` finds the changes; this decides what to say about
//! them, which is a different question. The set of paths where the live `/etc`
//! and the shipped `/usr/etc` disagree is a fact about a deployment. Whether
//! one of them matters depends on who claims the path:
//!
//! 1. **Kiln claims it** — a `[[file]]` in the generation's own manifest. The
//!    loudest case there is, because the user wrote that file down in the
//!    configuration, `kiln build` will faithfully put it in the next image,
//!    and the merge will just as faithfully throw it away. Every later edit to
//!    it is a no-op that looks like a change.
//! 2. **A package claims it** — `/etc/pacman.conf`, `/etc/nsswitch.conf`. The
//!    same shadowing one step further away: what is lost is not the user's own
//!    line but the upstream default, the next time the package updates.
//! 3. **Nobody claims it** — a locally created file. Not drift in the sense
//!    that matters; counted, and named only under `--verbose`.
//!
//! The manifest comes from the generation's commit metadata, not from
//! `/etc/kiln`, for one reason: the configuration on disk has very possibly
//! been edited since — that is *why* somebody is running `kiln status`.

use kiln_manifest::Manifest;
use kiln_ostree::drift::{self, Change, How};
use std::path::Path;

/// How many shadowing changes to name before the list stops being a report and
/// starts being a wall. A machine somebody has been administering by hand for
/// a year can have hundreds.
const LIST_AT_MOST: usize = 8;

/// What `kiln status` prints about `/etc`. `None` when there is nothing to say
/// — the usual case on a machine nobody has hand-edited, and a status command
/// should be silent about a system that is behaving.
pub fn report(deployment: &Path, manifest: Option<&Manifest>, verbose: bool) -> Option<String> {
    // A scan that could not read the deployment is not worth an error of its
    // own: everything else `kiln status` prints is still true and still what
    // the user asked for.
    let changes = drift::scan(deployment).ok()?;
    if changes.is_empty() {
        return None;
    }

    let (shadowing, local): (Vec<&Change>, Vec<&Change>) =
        changes.iter().partition(|c| c.shadows_the_image());
    let mut out = String::new();

    if shadowing.is_empty() {
        // Only local additions. Worth one line, because it is the honest answer
        // to "is my /etc what the image says", but not worth a warning: nothing
        // the image ships is being shadowed.
        out.push_str(&format!(
            "/etc        {} in /etc that the image does not ship, shadowing nothing\n",
            files(local.len())
        ));
        if verbose {
            for c in &local {
                out.push_str(&format!("            + {}\n", c.path()));
            }
        }
        return Some(out);
    }

    out.push_str(&format!(
        "/etc        \x1b[1;33m{}\x1b[0m\n",
        match shadowing.len() {
            1 => "1 local change to a file the image ships".to_string(),
            n => format!("{n} local changes to files the image ships"),
        }
    ));

    // Kiln's own `[[file]]` targets first, and always named in full however
    // long the list gets. This is the one class where the user has written the
    // file down somewhere and is entitled to be told it is not taking effect.
    let (ours, theirs): (Vec<&Change>, Vec<&Change>) = shadowing
        .into_iter()
        .partition(|c| manifest.is_some_and(|m| m.files.contains_key(c.path())));

    for c in &ours {
        out.push_str(&format!(
            "            {}   \x1b[1m← a [[file]] in this configuration\x1b[0m\n",
            line(c)
        ));
    }
    let cut = if verbose { theirs.len() } else { LIST_AT_MOST };
    for c in theirs.iter().take(cut) {
        out.push_str(&format!("            {}\n", line(c)));
    }
    if theirs.len() > cut {
        out.push_str(&format!(
            "            … and {} more; `kiln status --verbose` lists them\n",
            theirs.len() - cut
        ));
    }
    if !local.is_empty() {
        out.push_str(&format!(
            "            plus {} the image does not ship, shadowing nothing\n",
            files(local.len())
        ));
    }

    // The consequence, in the terms states it. Without this the report is a
    // list of paths, and the reason those paths are worth reading is exactly
    // the part that is not visible from the system itself.
    out.push_str(
        "\n            OSTree 3-way-merges /etc at deploy, so these win over every\n\
         \x20           future generation — including one built to change them. Put a\n\
         \x20           file back under Kiln's control by restoring the image's copy:\n\
         \x20             cp /usr/etc/<path> /etc/<path>\n",
    );
    if !ours.is_empty() {
        out.push_str(
            "\x20           The [[file]] entries above are the sharp case: editing the\n\
             \x20           configuration and rebuilding will not change them on this\n\
             \x20           machine until the live copy is restored.\n",
        );
    }
    Some(out)
}

/// One change, as `<mark> <path>` with what differs only when it is not the
/// contents. `M` and `D` are the spelling every version control tool has
/// already taught, and "the file is different" needs no annotation — but "the
/// bytes are identical and the mode is not" reads as a false positive unless
/// it says so.
fn line(c: &Change) -> String {
    match c {
        Change::Modified {
            path,
            how: How::Contents,
        } => format!("M {path}"),
        Change::Modified {
            path,
            how: How::Kind,
        } => format!("M {path}   (not the kind of file the image ships)"),
        Change::Modified { path, how } => format!(
            "M {path}   ({} differs, the contents do not)",
            match how {
                How::Mode => "the mode",
                How::Owner => "the owner",
                How::Contents | How::Kind => unreachable!("matched above"),
            }
        ),
        Change::Removed { path } => format!("D {path}"),
        Change::Added { path } => format!("+ {path}"),
    }
}

fn files(n: usize) -> String {
    match n {
        1 => "1 file".to_string(),
        _ => format!("{n} files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A deployment-shaped pair of directories, the way `kiln-ostree::drift`
    /// reads one: `usr/etc` is what the image shipped, `etc` is what the
    /// machine has.
    struct Deployment(std::path::PathBuf);

    impl Deployment {
        fn new(name: &str) -> Deployment {
            let dir = std::env::temp_dir()
                .join(format!("kiln-drift-report-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("usr/etc")).unwrap();
            fs::create_dir_all(dir.join("etc")).unwrap();
            Deployment(dir)
        }

        fn shipped(&self, rel: &str, content: &str) -> &Deployment {
            fs::write(self.0.join("usr/etc").join(rel), content).unwrap();
            self
        }

        fn live(&self, rel: &str, content: &str) -> &Deployment {
            fs::write(self.0.join("etc").join(rel), content).unwrap();
            self
        }

        fn both(&self, rel: &str, content: &str) -> &Deployment {
            self.shipped(rel, content).live(rel, content)
        }

        fn report(&self, manifest: Option<&Manifest>) -> String {
            plain(super::report(&self.0, manifest, false).unwrap_or_default())
        }

        fn verbose(&self, manifest: Option<&Manifest>) -> String {
            plain(super::report(&self.0, manifest, true).unwrap_or_default())
        }
    }

    impl Drop for Deployment {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The report is coloured, and a test asserting on colour would be
    /// asserting on the terminal rather than on what the report says.
    fn plain(s: String) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// A manifest that ships one `[[file]]`, which is the only thing about a
    /// manifest this module reads.
    fn ships(target: &str) -> Manifest {
        let mut m = Manifest::default();
        m.files.insert(
            target.to_string(),
            kiln_manifest::FileEntry {
                target: target.to_string(),
                source: Some("files/motd".into()),
                content: None,
                mode: None,
            },
        );
        m
    }

    #[test]
    fn a_system_nobody_has_touched_says_nothing() {
        let d = Deployment::new("quiet");
        d.both("pacman.conf", "[options]\n").both("motd", "hi\n");
        assert_eq!(super::report(&d.0, None, false), None);
    }

    #[test]
    fn a_kiln_shipped_file_is_named_as_one() {
        let d = Deployment::new("ours");
        d.shipped("motd", "welcome\n").live("motd", "mine\n");
        let r = d.report(Some(&ships("/etc/motd")));
        assert!(
            r.contains("M /etc/motd   ← a [[file]] in this configuration"),
            "{r}"
        );
        // The consequence, not just the fact. Without this sentence the report
        // does not say why a user should care.
        assert!(r.contains("will not change them on this"), "{r}");
    }

    #[test]
    fn a_package_shipped_file_is_reported_without_the_configuration_note() {
        let d = Deployment::new("theirs");
        d.shipped("pacman.conf", "a\n").live("pacman.conf", "b\n");
        let r = d.report(Some(&ships("/etc/motd")));
        assert!(r.contains("M /etc/pacman.conf"), "{r}");
        assert!(!r.contains("[[file]]"), "{r}");
    }

    /// an addition shadows nothing, so it must not be reported as if it
    /// did. A machine with a locally created file and no other change is a
    /// machine with no drift worth a warning.
    #[test]
    fn local_additions_alone_are_not_a_warning() {
        let d = Deployment::new("added");
        d.live("mine.conf", "x\n");
        let r = d.report(None);
        assert!(r.contains("shadowing nothing"), "{r}");
        assert!(!r.contains("local change"), "{r}");
        assert!(!r.contains("mine.conf"), "{r}");
        assert!(d.verbose(None).contains("+ /etc/mine.conf"));
    }

    #[test]
    fn a_long_list_is_cut_and_says_how_to_see_the_rest() {
        let d = Deployment::new("many");
        for i in 0..12 {
            d.shipped(&format!("c{i}.conf"), "a\n")
                .live(&format!("c{i}.conf"), "b\n");
        }
        let r = d.report(None);
        assert!(
            r.contains("12 local changes to files the image ships"),
            "{r}"
        );
        assert!(
            r.contains("… and 4 more; `kiln status --verbose` lists them"),
            "{r}"
        );
        assert_eq!(d.verbose(None).matches("M /etc/c").count(), 12);
    }

    /// A `[[file]]` is named however long the list gets: it is the class the
    /// user can act on from the configuration, and truncating it away would
    /// hide the one line that has a fix.
    #[test]
    fn a_kiln_file_survives_the_cut() {
        let d = Deployment::new("cut");
        for i in 0..12 {
            d.shipped(&format!("c{i}.conf"), "a\n")
                .live(&format!("c{i}.conf"), "b\n");
        }
        d.shipped("motd", "welcome\n").live("motd", "mine\n");
        let r = d.report(Some(&ships("/etc/motd")));
        assert!(r.contains("M /etc/motd   ← a [[file]]"), "{r}");
    }

    #[test]
    fn a_mode_only_change_says_the_contents_are_the_same() {
        use std::os::unix::fs::PermissionsExt;
        let d = Deployment::new("mode");
        d.both("secret.conf", "x\n");
        fs::set_permissions(
            d.0.join("etc/secret.conf"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(
            d.report(None)
                .contains("M /etc/secret.conf   (the mode differs, the contents do not)"),
            "{}",
            d.report(None)
        );
    }
}
