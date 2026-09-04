//! Boot acceptance in qemu.
//!
//! *"This is the only test that proves the project works."* Everything else in
//! the suite checks a tree, a plan or a diagnostic. This is the one that puts a
//! real Arch-derived OSTree image on a real disk, boots it, and asks the
//! running system whether the Arch→OSTree contract actually holds.
//!
//! The shape: deploy generation 1, boot; deploy generation 2, boot;
//! `kiln rollback`, boot; assert back on generation 1.
//!
//! Everything runs through the **real `kiln` binary** with `--sysroot` pointing
//! at a mounted disk image (this is exactly that seam). Nothing here reaches
//! into a library to set something up, because a test that assembles the image
//! itself proves nothing about the command a user types.
//!
//! **Prerequisites**, all skipped-with-a-message rather than failed: root, KVM,
//! qemu, and network access to the Arch mirrors. Unlike every other test in
//! this workspace, this one *must* use the network — `tests/repo-fixture` holds
//! four tiny packages with no kernel and no userland, and there is no way to
//! boot that. The whole point is a real image.
//!
//! It takes roughly twenty minutes and downloads several hundred megabytes.
//!
//! ## What it does not prove
//!
//! The VM boots the kernel and initramfs **directly**, extracted from the BLS
//! entry libostree wrote, rather than through GRUB from the ESP. So this covers
//! everything Kiln is responsible for — that the entry exists, that it is the
//! right one, that its `options` line carries the kargs the manifest asked for,
//! and that the initramfs pivots into the deployment — and does not cover
//! firmware→GRUB→kernel, which is GRUB's job and needs OVMF and a partitioned
//! disk. The spike deferred the same thing for the same reason.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Big enough for a base image, three deployments and the artifact store.
const DISK_SIZE: &str = "16G";

/// Per boot. A cold first boot with tmpfiles restoring the whole `/var` is the
/// slow one; 7 minutes is generous and still fails in finite time.
const BOOT_TIMEOUT: &str = "420";

/// Added in generation 2, to prove a second build genuinely produces a
/// different image and that a rollback genuinely goes back.
const GEN2_PACKAGE: &str = "nano";

// ── the test ────────────────────────────────────────────────────────────────

/// Boot acceptance, the whole of it.
///
/// One test rather than three, because the three phases are one story and
/// share twenty minutes of setup: a `#[test]` per boot would either rebuild the
/// image three times or depend on execution order.
#[test]
#[ignore = "boot acceptance: needs root, KVM, qemu and the network; ~20 minutes"]
fn an_image_boots_a_second_generation_boots_and_rollback_returns_to_the_first() {
    let Some(env) = Env::prepare("boot-acceptance") else {
        return;
    };

    // ── generation 1 ────────────────────────────────────────────────────
    env.step("kiln sysroot init");
    env.kiln(&["sysroot", "init"]).expect_ok("sysroot init");

    // The installer's sequence, verbatim: build, then deploy the generation by number.
    // Generation 2 below uses `kiln apply` instead, so both paths to a bootable
    // deployment are covered — and this one is covered *here* because it is an
    // installer's path and nothing else walks it. It shipped broken once:
    // `kiln deploy` reordered the deployment list and could not deploy a
    // generation that had only ever been committed, which is everything
    // `kiln build` produces.
    env.step("kiln build — generation 1");
    env.kiln(&["build"]).expect_ok("the first build");

    env.step("kiln deploy 1 — the installer's \"make it bootable\"");
    env.kiln(&["deploy", "1"])
        .expect_ok("deploying the first generation");

    let gen1 = env.boot("gen1");
    gen1.assert_contract();
    gen1.assert_generation(1);
    gen1.assert("nano_installed", "no");
    gen1.assert("var_seed", "restored from factory");
    gen1.assert("script_marker", "script ran in bootacceptance generation 1");

    // A PKGBUILD in the configuration tree, compiled in a sandbox with
    // no network against a build root Kiln assembled, installed from disk by
    // libalpm — and still there, still executable, still owned by its package,
    // on a running system with a read-only /usr.
    gen1.assert("built_package", "yes");
    gen1.assert("built_package_runs", "built by kiln from a PKGBUILD");
    gen1.assert("built_package_owns", "kiln-boot-marker");

    // ── generation 2 ────────────────────────────────────────────────────
    env.step("kiln apply — generation 2");
    env.add_package(GEN2_PACKAGE);
    let second = env.kiln(&["apply"]);
    second.expect_ok("the second build");

    // *rebuilding an image after changing one line of `system.toml` must
    // not rebuild your out-of-tree NVIDIA module.* The recipe did not change,
    // so its `build_key` did not, so nothing is compiled the second time. This
    // is the single largest speed win in the system, and the only way to check
    // it is to build twice.
    let log = String::from_utf8_lossy(&second.stdout).into_owned();
    assert!(
        log.contains("kiln-boot-marker: 1 package from the build cache"),
        "the second build recompiled an unchanged recipe:\n{log}"
    );

    let gen2 = env.boot("gen2");
    gen2.assert_contract();
    gen2.assert_generation(2);
    gen2.assert("nano_installed", "yes");
    gen2.assert("built_package", "yes");

    // the same image must land on the same service-account ids across
    // generations. A drifting gid is a whole class of "why does this daemon
    // suddenly not own its files" that only appears after an update.
    for key in ["gid_systemd-journal", "gid_systemd-network"] {
        assert_eq!(
            gen1.get(key),
            gen2.get(key),
            "{key} moved between generations — the UID seed did not replay"
        );
    }

    // ── rollback ────────────────────────────────────────────────────────
    env.step("kiln rollback");
    env.kiln(&["rollback"]).expect_ok("rollback");

    let back = env.boot("rollback");
    back.assert_contract();
    back.assert_generation(1);
    back.assert("nano_installed", "no");
    back.assert("built_package", "yes");

    println!("\n\x1b[1;32mBOOT ACCEPTANCE: PASS\x1b[0m");
    println!("gen 1 booted, gen 2 booted, rollback booted back to gen 1");
}

// ── the environment ─────────────────────────────────────────────────────────

struct Env {
    /// `target/test-roots/<name>`.
    base: PathBuf,
    /// The raw disk image. Mounted at `mnt` only while the host is working on
    /// it — never while qemu is running. See `Mounted`.
    disk: PathBuf,
    mnt: PathBuf,
    /// Where the kernel and initramfs are copied to for qemu to boot, since
    /// they cannot be read off the disk while the VM has it.
    out: PathBuf,
    /// A writable copy of `tests/vm/config`, so generation 2 can add a package
    /// without the test editing a tracked file.
    config: PathBuf,
}

/// The disk mounted on the host, for as long as this is alive.
///
/// **The host and the VM must never have the filesystem at the same time**, and
/// neither may start from what it remembers of it. qemu reads and writes
/// `disk.img` through its own descriptor; the host reads and writes the same
/// bytes through a loop device, which carries its own page cache over that
/// file. Two caches over one extent map are individually right and jointly
/// wrong, and what that produces names neither:
///
/// ```text
/// # on the host, during the next build
/// error preparing the build directory: Bad message (os error 74)
/// # or in the next guest
/// EXT4-fs error (device vda): ext4_lookup: deleted inode referenced
/// /usr/bin/bash: error while loading shared libraries: invalid ELF header
/// ```
///
/// — a corrupted image, a broken `/var` drain, a mangled commit. It is none of
/// them; the file on disk is byte-perfect throughout, and `sha256sum` says so
/// while the guest cannot execute it.
///
/// So every handover **settles**: flush everything dirty, then drop the page
/// cache, so the next reader has no choice but to read the disk. `settle` is
/// called before each mount and before each boot. This is a heavier hammer
/// than a correctness argument about writeback ordering, and it is the right
/// one for a test: it makes the coherence question have no answer to get wrong.
struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let out = Command::new("umount").arg(&self.0).output();
        if !out.map(|o| o.status.success()).unwrap_or(false) {
            // Lazy only as a fallback, never as the default: `-l` returns
            // before the filesystem is released, which is the race itself.
            let _ = Command::new("umount")
                .args(["-R", "-l"])
                .arg(&self.0)
                .output();
        }
        settle();
    }
}

/// Everything dirty written out, and nothing cached remembered.
///
/// `sync` alone is not enough: it makes the disk match the caches, and the
/// problem is a *reader* whose cache is stale relative to a writer that went
/// through a different one. Dropping the caches is what forces the next read to
/// come off the disk.
fn settle() {
    let _ = Command::new("sync").output();
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3\n");
}

impl Env {
    /// Check every prerequisite and lay out the disk, or explain why not.
    ///
    /// Returns `None` rather than failing: a machine without KVM should not
    /// turn the suite red for a reason that has nothing to do with the code.
    fn prepare(name: &str) -> Option<Env> {
        for (ok, why) in [
            (
                is_root(),
                "needs root: it mounts a disk image and runs a build",
            ),
            (Path::new("/dev/kvm").exists(), "needs /dev/kvm"),
            (have("qemu-system-x86_64"), "needs qemu-system-x86_64"),
            (have("mkfs.ext4"), "needs mkfs.ext4 (e2fsprogs)"),
            (online(), "needs network access to the Arch mirrors"),
        ] {
            if !ok {
                eprintln!("skipped: boot acceptance {why}");
                return None;
            }
        }

        let base = workspace().join("target/test-roots").join(name);
        // libostree sets the immutable attribute on deployment roots, so a
        // plain remove_dir_all fails and leaves the previous run's deployments
        // in place — which shows up as generation numbers that keep climbing.
        let _ = Command::new("umount")
            .args(["-R", "-l"])
            .arg(base.join("mnt"))
            .output();
        let _ = Command::new("chattr")
            .args(["-R", "-i"])
            .arg(&base)
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("creating the test root");

        let disk = base.join("disk.img");
        let mnt = base.join("mnt");
        let out = base.join("out");
        let config = base.join("config");

        println!("\n\x1b[1;36m▸ disk: create the image\x1b[0m");
        std::fs::create_dir_all(&mnt).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        run("truncate", &["-s", DISK_SIZE, &s(&disk)]).expect_ok("truncate");
        run(
            "mkfs.ext4",
            &["-q", "-F", "-L", "kiln-root", "-m", "0", &s(&disk)],
        )
        .expect_ok("mkfs.ext4");

        let env = Env {
            disk,
            mnt,
            out,
            config,
            base,
        };

        {
            let _held = env.mount();
            // libostree's `deploy_tree` fails with a bare
            // `opendir(ostree/deploy/kiln/var)` if the stateroot's `var` is
            // missing, and `/boot` is a mounted partition on a real machine.
            // Both are `kiln sysroot init`'s job — but `/boot` has to be a
            // directory on the disk before anything writes an entry into it.
            std::fs::create_dir_all(env.mnt.join("boot")).unwrap();
        }

        env.step("config: copy the fixture so generation 2 can differ");
        copy_tree(&workspace().join("tests/vm/config"), &env.config);

        Some(env)
    }

    fn mount(&self) -> Mounted {
        // Start from the bytes on disk, not from anything remembered about
        // them. See `Mounted`.
        settle();
        run("mount", &["-o", "loop", &s(&self.disk), &s(&self.mnt)]).expect_ok("mount");
        Mounted(self.mnt.clone())
    }

    /// `e2fsck -fn`, read-only, between phases.
    ///
    /// Here because the damage it catches is otherwise attributed to entirely
    /// the wrong thing: a filesystem the host corrupted surfaces two minutes
    /// later as `systemd-tmpfiles` reporting that `/var/lib` "already exists
    /// and is not a directory", which reads as a bug in the `/var` drain
    /// and is not. Naming the moment the image stopped being
    /// consistent turns an afternoon into one line.
    fn fsck(&self, when: &str) {
        settle();
        let out = run("e2fsck", &["-fn", &s(&self.disk)]);
        // 0 is clean. `-n` answers "no" to every repair, so anything else means
        // the image is not consistent.
        let code = out.status.code().unwrap_or(-1);
        if code == 0 {
            return;
        }
        panic!(
            "the filesystem is inconsistent {when} (e2fsck exit {code}).\n\n{}\n\n\
             Something wrote to the image while another kernel had it: the host and the \
             guest must never hold it at once (see `Mounted`).",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }

    /// Run the real `kiln` binary against this sysroot and config.
    ///
    /// The mount is taken for the call and released at the end of it, so the
    /// disk is never held by the host and the VM at once.
    fn kiln(&self, args: &[&str]) -> Output {
        let _held = self.mount();
        let mut argv: Vec<String> = vec![
            "--sysroot".into(),
            s(&self.mnt),
            "--config".into(),
            s(&self.config),
            "--module-root".into(),
            s(&workspace().join("modules")),
        ];
        argv.extend(args.iter().map(|a| a.to_string()));
        println!("\x1b[2m  $ kiln {}\x1b[0m", argv.join(" "));

        let out = Command::new(env!("CARGO_BIN_EXE_kiln"))
            .args(&argv)
            .output()
            .expect("the kiln binary should run");
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            println!("    {line}");
        }
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            eprintln!("    {line}");
        }
        out
    }

    /// Generation 2 differs from generation 1 by one package. Appended to the
    /// *copy*, so the tracked fixture stays the single description of the
    /// image and the diff between generations is one line.
    fn add_package(&self, package: &str) {
        let at = self.config.join("gen2.toml");
        std::fs::write(
            &at,
            format!("kiln = 1\n\n[packages]\nrepo = [\"{package}\"]\n"),
        )
        .unwrap();

        let system = self.config.join("system.toml");
        let text = std::fs::read_to_string(&system).unwrap();
        let text = text.replace(
            "include = [\"@kiln/profiles/minimal\"]",
            "include = [\"@kiln/profiles/minimal\", \"gen2.toml\"]",
        );
        std::fs::write(&system, text).unwrap();
    }

    /// Boot the default deployment and collect what the probe reported.
    ///
    /// The kernel and initramfs are **copied off** the disk first and the mount
    /// released, because qemu is about to hand the same filesystem to a guest
    /// that will mount it read-write. Reading them straight off the loop mount
    /// while the VM runs is the corruption `Mounted` describes.
    fn boot(&self, label: &str) -> Facts {
        let (entry, kernel, initrd) = {
            let _held = self.mount();
            let entry = self.default_entry();
            let kernel = self.out.join("vmlinuz");
            let initrd = self.out.join("initramfs.img");
            let from_kernel = self
                .mnt
                .join("boot")
                .join(entry.linux.trim_start_matches('/'));
            let from_initrd = self
                .mnt
                .join("boot")
                .join(entry.initrd.trim_start_matches('/'));
            assert!(
                from_kernel.exists(),
                "no kernel at {}",
                from_kernel.display()
            );
            assert!(
                from_initrd.exists(),
                "no initramfs at {}",
                from_initrd.display()
            );
            std::fs::copy(&from_kernel, &kernel).expect("copying the kernel");
            std::fs::copy(&from_initrd, &initrd).expect("copying the initramfs");
            (entry, kernel, initrd)
        };

        self.fsck(&format!("before booting {label}"));
        settle();
        self.step(&format!("qemu: boot [{label}]"));
        println!("    entry:   {}", entry.title);
        println!("    options: {}", entry.options);

        // `rd.emergency=poweroff` turns a failed initramfs pivot into a fast,
        // visible failure rather than a seven-minute hang on the timeout.
        let append = format!("{} rd.emergency=poweroff panic=10", entry.options);

        let args: Vec<String> = vec![
            "--foreground".into(),
            BOOT_TIMEOUT.into(),
            "qemu-system-x86_64".into(),
            "-enable-kvm".into(),
            "-cpu".into(),
            "host".into(),
            "-m".into(),
            "2048".into(),
            "-smp".into(),
            "4".into(),
            "-nographic".into(),
            "-no-reboot".into(),
            // **The guest never writes the image.** Every write goes to a
            // temporary overlay qemu discards on exit, so the host stays the
            // only writer and the two can never disagree about the bytes.
            //
            // This costs the test nothing it asserts. Nothing here depends on
            // guest-side persistence: `kiln apply` and `kiln rollback` run on
            // the host, and what the probe checks — the `/var` drain, the
            // factory seed, the pinned ids — is restored from the image on
            // every boot by design. If anything it is the stricter
            // arrangement, because each boot is a first boot onto a bare
            // `/var`, which is the path that actually has to work.
            //
            // Without it the host and the guest are two writers with two page
            // caches over one file, and the corruption that produces surfaces
            // one phase later as `Bad message (os error 74)` or an ext4
            // "deleted inode referenced" — see `Mounted`. `settle` and `fsck`
            // narrow that window; this closes it.
            "-snapshot".into(),
            "-kernel".into(),
            s(&kernel),
            "-initrd".into(),
            s(&initrd),
            "-append".into(),
            append,
            "-drive".into(),
            format!(
                "file={},format=raw,if=virtio,cache=writeback",
                s(&self.disk)
            ),
        ];

        let mut child = Command::new("timeout")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning qemu");

        let stdout = child.stdout.take().unwrap();
        let mut facts = BTreeMap::new();
        let mut log = String::new();
        let mut reached = false;

        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let line = line.trim_end_matches(['\r', '\u{0}']).to_string();
            log.push_str(&line);
            log.push('\n');
            if let Some(rest) = line.split("KILN| ").nth(1) {
                reached = true;
                println!("\x1b[0;32m    | {rest}\x1b[0m");
                if let Some((k, v)) = rest.split_once('=') {
                    facts.insert(k.trim().to_string(), v.trim().to_string());
                }
            } else if line.contains("Kernel panic")
                || line.contains("emergency")
                || line.contains("ostree-prepare-root")
                || line.contains("Failed to start")
            {
                println!("\x1b[0;31m    | {line}\x1b[0m");
            }
        }
        let _ = child.wait();

        let path = self.base.join(format!("boot-{label}.log"));
        std::fs::write(&path, &log).unwrap();
        println!("    serial log: {}", path.display());

        assert!(
            reached,
            "the probe never ran: the image did not reach multi-user.target — see {}",
            path.display()
        );
        self.fsck(&format!("after booting {label}"));
        Facts {
            label: label.to_string(),
            facts,
            log_path: path,
        }
    }

    /// The BLS entry the firmware would pick.
    ///
    /// **By `version` descending, never by filename.** from phase 0:
    /// libostree's `ostree-N.conf` filename numbering runs *opposite* to BLS
    /// boot order, so sorting by filename picks the rollback deployment and the
    /// test then cheerfully asserts that a rollback worked when nothing
    /// happened.
    fn default_entry(&self) -> BootEntry {
        let dir = self.mnt.join("boot/loader/entries");
        let mut entries: Vec<BootEntry> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("conf"))
            .map(|p| BootEntry::parse(&p))
            .collect();
        assert!(
            !entries.is_empty(),
            "no BLS entries under {}",
            dir.display()
        );

        entries.sort_by_key(|e| std::cmp::Reverse(e.version));
        for (i, e) in entries.iter().enumerate() {
            println!(
                "    {}version {} — {}",
                if i == 0 { "-> " } else { "   " },
                e.version,
                e.title
            );
        }
        entries.remove(0)
    }

    fn step(&self, what: &str) {
        println!("\n\x1b[1;36m▸ {what}\x1b[0m");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Belt and braces: every operation releases its own mount, but a panic
        // between `mount` and the guard's construction would leave one behind,
        // and a stray mount makes the *next* run's wipe fail for a reason that
        // looks nothing like this.
        let _ = Command::new("umount")
            .args(["-R", "-l"])
            .arg(&self.mnt)
            .output();
    }
}

// ── what the booted system said ─────────────────────────────────────────────

struct Facts {
    label: String,
    facts: BTreeMap<String, String>,
    log_path: PathBuf,
}

impl Facts {
    fn get(&self, key: &str) -> &str {
        self.facts
            .get(key)
            .map(String::as_str)
            .unwrap_or("<missing>")
    }

    fn assert(&self, key: &str, want: &str) {
        assert_eq!(
            self.get(key),
            want,
            "[{}] {key} — see {}",
            self.label,
            self.log_path.display()
        );
    }

    /// Which image is actually running. The generation comes from the record
    /// Kiln wrote *into the image* (step 11), not from anything the test
    /// passed in — which is the only way a rollback assertion means anything.
    fn assert_generation(&self, want: u64) {
        self.assert("generation", &want.to_string());
        self.assert("image", "bootacceptance");
    }

    /// The Arch→OSTree contract, as observed from inside the booted system —
    /// these are all claims about this moment.
    fn assert_contract(&self) {
        // The filesystem shape.
        self.assert("usr_readonly", "yes");
        self.assert("etc_writable", "yes");
        self.assert("etc_passwd", "yes");

        // The /var drain. Every `d`, `C` and `L` line Kiln generated
        // must have produced something on a machine whose /var started bare.
        self.assert("var_entries_missing", "0");
        self.assert("var_lib_pacman_absent", "yes");
        self.assert("var_home", "yes");
        self.assert("roothome", "yes");

        // libostree's own view: without `/ostree → sysroot/ostree` it
        // cannot read its own sysroot from inside the deployment, and
        // `kiln list`, `kiln status` and `kiln rollback` have nothing to stand
        // on.
        self.assert("booted_via_ostree", "yes");
        self.assert("sysroot_mounted", "/sysroot");
        self.assert("sysroot_repo", "yes");
        assert!(
            self.get("ostree_deployments").parse::<u32>().unwrap_or(0) >= 1,
            "[{}] ostree_deployments = {} — libostree cannot read its own sysroot from \
             inside the deployment; see {}",
            self.label,
            self.get("ostree_deployments"),
            self.log_path.display()
        );

        // The package database in a read-only /usr.
        self.assert("manifest_present", "yes");
        assert!(
            self.get("pacman_query_count").parse::<u32>().unwrap_or(0) > 20,
            "[{}] pacman_query_count = {} — the package database is not usable from the \
             booted image; see {}",
            self.label,
            self.get("pacman_query_count"),
            self.log_path.display()
        );

        // Automatic rollback on boot failure — every piece of it that
        // this harness can reach:
        //
        //  - Kiln's grub.d fragment reached the deployment's /etc, survived the
        //    /etc → /usr/etc move and the merge back, and is executable;
        //  - running it emits GRUB script that counts, which is the fragment's
        //    entire job;
        //  - the image can clear its own counter — `grub-editenv` from the
        //    `grub` package, and Kiln's own boot-success script;
        //  - and it did. This boot reached `boot-complete.target`, the unit
        //    ran, and the counter Kiln armed at deploy is gone.
        //
        // That last one is the whole feature. A `boot_counter` still set here
        // is a machine that will demote a generation which works.
        //
        // What is *not* asserted is the generated `/boot/grub/grub.cfg`. This
        // harness only ever deploys through `--sysroot`, and libostree's grub2
        // backend cannot run there at all: it invokes `grub-mkconfig` chrooted
        // into the deployment while passing a **host-absolute** `-o` path,
        // which inside that chroot does not exist. So a `--sysroot` deploy gets
        // BLS entries and no `grub.cfg` by design — the installer's own
        // `grub-install` writes the first one — and checking its contents needs
        // the same OVMF-and-a-partition-table setup as firmware→GRUB→kernel.
        self.assert("grub_snippet", "yes");
        self.assert("bless_script", "yes");
        self.assert("grub_editenv", "yes");
        self.assert("bless_unit_enabled", "enabled");
        assert!(
            self.get("grub_snippet_counts").parse::<u32>().unwrap_or(0) > 0,
            "[{}] /etc/grub.d/09_kiln_boot_counter emits no boot counting — the fragment is \
             in the image but produces nothing, so automatic rollback would be off \
             on a machine that has GRUB; see {}",
            self.label,
            self.log_path.display()
        );
        self.assert("boot_counter", "");
        self.assert("boot_success", "1");

        // Did boot actually succeed. Not `is-system-running`, which reports
        // `degraded` for reasons a VM with no network legitimately produces —
        // these two are the questions that matter.
        self.assert("multi_user_reached", "active");
        self.assert("failed_units", "0");
    }
}

// ── odds and ends ───────────────────────────────────────────────────────────

struct BootEntry {
    title: String,
    version: u64,
    linux: String,
    initrd: String,
    options: String,
}

impl BootEntry {
    fn parse(path: &Path) -> BootEntry {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let get = |key: &str| -> String {
            text.lines()
                .find(|l| l.split_whitespace().next() == Some(key))
                .map(|l| l[key.len()..].trim().to_string())
                .unwrap_or_default()
        };
        BootEntry {
            title: get("title"),
            version: get("version").parse().unwrap_or(0),
            linux: get("linux"),
            initrd: get("initrd"),
            options: get("options"),
        }
    }
}

trait ExpectOk {
    fn expect_ok(&self, what: &str);
}

impl ExpectOk for Output {
    fn expect_ok(&self, what: &str) {
        assert!(
            self.status.success(),
            "{what} failed with {}:\n{}",
            self.status,
            String::from_utf8_lossy(&self.stderr)
        );
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/kiln-cli has a workspace root")
        .to_path_buf()
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running {program}: {e}"))
}

fn have(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(2)?
                .parse::<u32>()
                .ok()
        })
        == Some(0)
}

/// The one test in this workspace that needs the network, checked before
/// twenty minutes are spent discovering it.
fn online() -> bool {
    Command::new("curl")
        .args([
            "-sSf",
            "--max-time",
            "10",
            "-o",
            "/dev/null",
            "https://archlinux.org/",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn copy_tree(from: &Path, to: &Path) {
    let out = Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .output()
        .expect("running cp");
    assert!(
        out.status.success(),
        "copying {} to {}: {}",
        from.display(),
        to.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}
