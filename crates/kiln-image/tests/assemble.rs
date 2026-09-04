//! The assembler, end to end against the fixture repository.
//!
//! **Privileged.** Extracting a package means creating files owned by root with
//! the modes the archive declares, which an ordinary user cannot do — and
//! faking it would test something other than what a build does. Run under
//! `sudo -E cargo test -- --ignored`.
//!
//! The sandbox is a fake. The fixture's `fixture-linux` is not a kernel and
//! there is no dracut that could build an initramfs from it, so what is checked
//! here is the *orchestration*: which commands, against which root, in which
//! order, and what the assembler does with the answers. Whether dracut actually
//! produces a bootable initramfs is the boot acceptance test's question, and
//! nothing short of a VM can answer it.

mod fixture;
mod scratch;

use kiln_image::assemble::{self, Options};
use kiln_manifest::{FileEntry, Manifest, UnitFile};
use kiln_record::Record;
use kiln_resolve::{
    BuildPlan, ContentRef, EnableState, ImageRef, Provenance, ResolvedInput, UidMap,
};
use kiln_sandbox::{Outcome, Sandbox, SandboxSpec};
use std::cell::RefCell;
use std::path::PathBuf;

/// The fixture packages that make a plausible image: a filesystem layout, a
/// kernel, something that provides `init`, and a package with an `.INSTALL`
/// scriptlet that calls `systemctl` — which is what the shims are for.
const PACKAGES: &[(&str, &str)] = &[
    ("fixture-filesystem", "1.0-1"),
    ("fixture-base", "1.0-1"),
    ("fixture-linux", "6.19-1"),
    ("fixture-init", "1.0-1"),
    // Deliberately no `fixture-app`: it ships an `.INSTALL` scriptlet, the
    // fixture ships no shell, and a scriptlet that cannot run correctly fails
    // the build — which `kiln-alpm`'s own tests already prove. An image that
    // fails to assemble is no use for testing assembly.
    ("fixture-varpayload", "1.0-1"),
];

fn plan() -> BuildPlan {
    let mut plan = BuildPlan {
        config_id: kiln_manifest::Hash("b3:fixture".into()),
        image: ImageRef {
            name: "fixture".into(),
            arch: "x86_64".into(),
        },
        inputs: PACKAGES
            .iter()
            .map(|(name, evr)| ResolvedInput::RepoPackage {
                name: (*name).into(),
                evr: (*evr).into(),
                filename: format!("{name}-{evr}-any.pkg.tar.zst"),
                sha256: String::new(),
                repo: "fixture".into(),
                explicit: *name == "fixture-base",
            })
            .collect(),
        volatile: Vec::new(),
        uid_map: UidMap::new(),
        provenance: Provenance {
            resolved_at: "2026-09-01T00:00:00Z".into(),
            snapshot: "2026-09-01".into(),
            repos: vec![("fixture".into(), vec!["file:///fixture".into()])],
            libalpm: kiln_alpm::libalpm_version().into(),
        },
    };
    plan.inputs.push(ResolvedInput::File {
        target: "/etc/motd".into(),
        content: ContentRef::Inline {
            digest: kiln_manifest::Hash::of(b"assembled by kiln\n"),
        },
        mode: None,
    });
    plan.inputs.push(ResolvedInput::Unit {
        name: "fixture-app.service".into(),
        content: ContentRef::Inline {
            digest: kiln_manifest::Hash::of(b""),
        },
        enable: EnableState::Enabled,
    });
    plan.canonicalize();
    plan
}

fn manifest() -> Manifest {
    let mut manifest = Manifest::default();
    manifest.files.insert(
        "/etc/motd".into(),
        FileEntry {
            target: "/etc/motd".into(),
            source: None,
            content: Some("assembled by kiln\n".into()),
            mode: None,
        },
    );
    manifest.systemd.units.insert(
        "fixture-app.service".into(),
        UnitFile {
            name: "fixture-app.service".into(),
            source: None,
            content: Some("[Service]\nExecStart=/usr/bin/fixture-tool\n".into()),
            enable: true,
        },
    );
    manifest
}

/// A resolution session, refreshed, so assembly has sync databases to import.
/// This is what the real pipeline does: resolution downloads the metadata, and
/// assembly copies it in rather than going online again.
fn resolved(base: &std::path::Path) -> PathBuf {
    let state = base.join("state");
    let mut session = kiln_alpm::Session::open(
        kiln_alpm::Config::for_resolution(&state, "x86_64").with_repos(vec![
            kiln_alpm::RepoSpec::new(
                "fixture",
                vec![kiln_alpm::mirrors::file(&fixture::repo())],
                kiln_alpm::Trust::Unsigned,
            ),
        ]),
    )
    .expect("opening a resolution session");
    session.refresh(true).expect("refreshing the fixture repo");
    session.config().dbpath.clone()
}

fn options(name: &str) -> Options {
    let base = fixture::workspace().join("target/test-roots").join(name);
    std::fs::remove_dir_all(&base).ok();
    std::fs::create_dir_all(&base).unwrap();
    Options {
        syncdb_from: resolved(&base),
        gpgdir: base.join("keyring"),
        root: base.join("root"),
        config_root: base.join("config"),
        work: base.join("work"),
        cache: base.join("cache"),
        artifacts: Vec::new(),
        repos: vec![kiln_alpm::RepoSpec::new(
            "fixture",
            vec![kiln_alpm::mirrors::file(&fixture::repo())],
            kiln_alpm::Trust::Unsigned,
        )],
        generation: 1,
    }
}

#[test]
#[ignore = "privileged: extracting a package needs root"]
fn a_whole_image_is_assembled() {
    if !fixture::require_root("assembling an image") {
        return;
    }
    let opts = options("assemble-whole");
    let sandbox = FakeSandbox::default();
    let report = assemble::assemble(&plan(), &manifest(), &opts, &sandbox).unwrap();
    let root = &opts.root;

    // The packages are in, and the database is where the booted image will look
    // for it.
    assert!(report.installed.contains(&"fixture-base".to_string()));
    assert!(root.join("usr/lib/sysimage/pacman/local").is_dir());
    assert!(!root.join("var/lib/pacman").exists());

    // Nothing of the build machinery survived into the image.
    assert!(!root
        .join(assemble::SHIM_DIR_IN_ROOT)
        .join("systemctl")
        .exists());
    assert!(kiln_image::tree::entries(&root.join("run"))
        .unwrap()
        .is_empty());

    // The overlay and the unit state ran.
    assert_eq!(
        std::fs::read_to_string(root.join("usr/etc/motd")).unwrap(),
        "assembled by kiln\n"
    );
    assert!(root
        .join("usr/lib/systemd/system/fixture-app.service")
        .is_file());

    // And the contract holds. `assemble` checks this itself; asserting it here
    // too is what makes a failure say *which* part of the contract broke.
    assert!(kiln_image::verify::check(root).is_empty());
}

///, the reason the transaction is split. `filesystem` owns
/// `/etc/passwd`; seeding first makes pacman abort on a file conflict, and
/// `--overwrite` would let the stock file clobber the pins.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn the_uid_seed_lands_between_the_two_transactions() {
    if !fixture::require_root("assembling an image") {
        return;
    }
    let mut plan = plan();
    plan.uid_map.groups.insert("fixture-svc".into(), 941);

    let opts = options("assemble-uid");
    let report = assemble::assemble(&plan, &manifest(), &opts, &FakeSandbox::default()).unwrap();

    assert_eq!(report.uid_drift, Vec::new(), "the pin was honoured");
    let record = report.record.unwrap();
    assert_eq!(record.uid_map.groups.get("fixture-svc"), Some(&941));
}

/// `/usr/local/bin` and the shim mechanism. libalpm runs a package's `.INSTALL` scriptlet
/// chrooted into the install root with pacman's own `PATH`, which begins with
/// `/usr/local/bin` — so that directory is the shadowing lever for scriptlets,
/// the same shape as the `HookDir` lever for hooks.
///
/// This is unprivileged, and the end-to-end test asserts the other half: that
/// nothing is left behind. Whether a *real* Arch scriptlet's `systemctl
/// daemon-reload` gets caught needs a shell in the image, which the fixture
/// deliberately does not have.
#[test]
fn the_shims_shadow_the_scriptlet_path_and_leave_nothing_behind() {
    let root = fixture::workspace().join("target/test-roots/assemble-shims");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();

    let names = assemble::place_shims(&root).unwrap();
    assert!(names.contains(&"systemctl".to_string()));
    for name in &names {
        let at = root.join(assemble::SHIM_DIR_IN_ROOT).join(name);
        assert!(at.is_file(), "{name} was not placed");
        let mode = std::os::unix::fs::PermissionsExt::mode(&at.metadata().unwrap().permissions());
        assert_eq!(mode & 0o111, 0o111, "{name} is not executable");
    }

    // The log the shims append to has to exist inside the root, because the
    // path in the script is absolute and the scriptlet is chrooted.
    let log = root.join("run/kiln");
    assert!(log.is_dir());
    std::fs::write(log.join("shims.log"), "systemctl daemon-reload\n").unwrap();
    assert_eq!(
        assemble::collect_shim_log(&root),
        ["systemctl daemon-reload"]
    );

    assemble::remove_shims(&root, &names).unwrap();
    for name in &names {
        assert!(!root.join(assemble::SHIM_DIR_IN_ROOT).join(name).exists());
    }
    // A `systemctl` in /usr/local/bin that exits 0 would be a very confusing
    // thing to find on a booted system.
    assert!(!root.join("run/kiln/shims.log").exists());
}

/// Step 11. The image describes itself, so `kiln status`, `kiln diff` and
/// `kiln why` work on a machine whose configuration has since been edited or
/// deleted — the normal case when debugging why the generation you rolled back
/// to behaves differently.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn the_image_carries_its_own_record() {
    if !fixture::require_root("assembling an image") {
        return;
    }
    let opts = options("assemble-record");
    let plan = plan();
    assemble::assemble(&plan, &manifest(), &opts, &FakeSandbox::default()).unwrap();

    let record = Record::read(&opts.root.join(kiln_record::IN_IMAGE)).unwrap();
    assert_eq!(record.plan_id(), plan.plan_id());
    assert_eq!(record.generation, 1);
    assert!(record
        .repo_packages
        .iter()
        .any(|p| p.name == "fixture-base"));
}

/// The order the kernel step drives the sandbox in. depmod before dracut,
/// dracut before the check, and the check is a *separate command* —
/// dracut's exit code is not the check.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn the_kernel_step_runs_depmod_then_dracut_then_verifies() {
    if !fixture::require_root("assembling an image") {
        return;
    }
    let opts = options("assemble-kernel");
    let sandbox = FakeSandbox::default();
    let report = assemble::assemble(&plan(), &manifest(), &opts, &sandbox).unwrap();

    let commands: Vec<String> = sandbox
        .specs()
        .iter()
        .map(|s| s.command.first().cloned().unwrap_or_default())
        .collect();
    assert_eq!(commands, ["depmod", "dracut", "lsinitrd"]);
    assert_eq!(report.kernel.unwrap().version, "6.19.0-fixture");
    // Every one of them ran against the staging root, never the host.
    for spec in sandbox.specs() {
        assert_eq!(spec.root, opts.root);
    }
}

///, at the level that matters: an initramfs without
/// `ostree-prepare-root` boots to an emergency shell, and the assembler must
/// refuse to commit it rather than discover it at boot.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn an_initramfs_missing_the_ostree_module_fails_the_build() {
    if !fixture::require_root("assembling an image") {
        return;
    }
    let opts = options("assemble-k3");
    let sandbox = FakeSandbox::with_listing("usr/lib/systemd/systemd\n");
    let err = assemble::assemble(&plan(), &manifest(), &opts, &sandbox).unwrap_err();
    assert!(format!("{err}").contains("emergency shell"), "{err}");
}

/// A record of what a build of this fixture *contains*, so that a change to any
/// step shows up as a diff a reviewer reads rather than as a green test.
#[test]
#[ignore = "privileged: extracting a package needs root"]
fn the_tmpfiles_fragment_for_the_fixtures_var_payload() {
    if !fixture::require_root("assembling an image") {
        return;
    }
    let opts = options("assemble-drain");
    let report = assemble::assemble(&plan(), &manifest(), &opts, &FakeSandbox::default()).unwrap();
    insta::assert_snapshot!(report.normalize.drain.render());
}

#[derive(Default)]
struct FakeSandbox {
    specs: RefCell<Vec<SandboxSpec>>,
    listing: Option<String>,
}

impl FakeSandbox {
    fn with_listing(listing: &str) -> FakeSandbox {
        FakeSandbox {
            listing: Some(listing.to_string()),
            ..FakeSandbox::default()
        }
    }

    fn specs(&self) -> Vec<SandboxSpec> {
        self.specs.borrow().clone()
    }
}

impl Sandbox for FakeSandbox {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn argv(&self, spec: &SandboxSpec) -> kiln_sandbox::Result<Vec<String>> {
        Ok(spec.command.clone())
    }

    fn run(&self, spec: &SandboxSpec) -> kiln_sandbox::Result<Outcome> {
        self.specs.borrow_mut().push(spec.clone());
        // dracut is faked, so the file it would have written has to appear:
        // `lsinitrd` runs against a path, and the assembler is entitled to
        // assume the previous step produced one.
        if spec.command.first().map(String::as_str) == Some("dracut") {
            if let Some(out) = spec.command.last() {
                let at: PathBuf = spec.root.join(out.trim_start_matches('/'));
                std::fs::create_dir_all(at.parent().unwrap()).unwrap();
                std::fs::write(&at, b"a pretend initramfs").unwrap();
            }
        }
        Ok(Outcome {
            status: 0,
            stdout: match spec.command.first().map(String::as_str) {
                Some("lsinitrd") => self
                    .listing
                    .clone()
                    .unwrap_or_else(|| "usr/lib/ostree/ostree-prepare-root\n".into()),
                _ => String::new(),
            },
            ..Outcome::default()
        })
    }
}

/// *packaged content goes through pacman.* Every input the plan calls a
/// package ends up in one transaction — but only a repository package can be
/// asked for by *name*. Everything else is a file realization produced or was
/// handed, and no database anywhere has heard of it.
///
/// Naming one anyway is how a `packages.file` entry reached libalpm as "no
/// package named `packages/myapp-1.0-1-x86_64.pkg.tar.zst`", which is a true
/// statement about the wrong question.
#[test]
fn the_transaction_asks_for_repository_packages_by_name_and_everything_else_by_path() {
    let artifacts = vec![
        PathBuf::from("/var/lib/kiln/cache/build/aa/zen-browser-bin-1.16.3-1-x86_64.pkg.tar.zst"),
        PathBuf::from("/etc/kiln/packages/myapp-1.0-1-x86_64.pkg.tar.zst"),
    ];
    let mut plan = plan_of(vec![
        ResolvedInput::RepoPackage {
            name: "linux".into(),
            evr: "6.19.2-1".into(),
            filename: "linux-6.19.2-1-x86_64.pkg.tar.zst".into(),
            sha256: "3c9f".into(),
            repo: "core".into(),
            explicit: true,
        },
        ResolvedInput::RepoPackage {
            name: "glibc".into(),
            evr: "2.42-3".into(),
            filename: "glibc-2.42-3-x86_64.pkg.tar.zst".into(),
            sha256: "a10b".into(),
            repo: "core".into(),
            explicit: false,
        },
        ResolvedInput::AurPackage {
            name: "zen-browser-bin".into(),
            pkgbase: "zen-browser-bin".into(),
            evr: "1.16.3-1".into(),
            aur_commit: "3f1a9c8e".into(),
            srcinfo_hash: kiln_manifest::Hash("b3:aa01".into()),
            pulled_in_by: None,
        },
        ResolvedInput::FilePackage {
            path: "packages/myapp-1.0-1-x86_64.pkg.tar.zst".into(),
            sha256: "9f2c".into(),
        },
    ]);
    plan.canonicalize();

    let t = assemble::main_transaction(&plan, &artifacts);

    assert_eq!(t.packages, ["glibc", "linux"]);
    // `pacman -Qe` on the booted image has to distinguish what the
    // configuration asked for from what came along.
    assert_eq!(t.explicit, ["linux"]);
    assert_eq!(t.locals, artifacts);
}

/// an empty configuration produces an empty image — but a configuration
/// whose only package is one Kiln *built* is not empty, and a transaction that
/// looked only at names would decide it was and install nothing at all.
#[test]
fn a_transaction_holding_only_built_artifacts_is_not_empty() {
    let plan = plan_of(vec![ResolvedInput::FilePackage {
        path: "packages/myapp-1.0-1-x86_64.pkg.tar.zst".into(),
        sha256: "9f2c".into(),
    }]);
    let artifacts = vec![PathBuf::from(
        "/etc/kiln/packages/myapp-1.0-1-x86_64.pkg.tar.zst",
    )];

    assert!(assemble::main_transaction(&plan, &artifacts)
        .packages
        .is_empty());
    assert!(!assemble::main_transaction(&plan, &artifacts).is_empty());
    assert!(assemble::main_transaction(&plan, &[]).is_empty());
}

fn plan_of(inputs: Vec<ResolvedInput>) -> BuildPlan {
    BuildPlan {
        config_id: kiln_manifest::Hash("b3:test".into()),
        image: ImageRef {
            name: "test".into(),
            arch: "x86_64".into(),
        },
        inputs,
        volatile: Vec::new(),
        uid_map: UidMap::new(),
        provenance: Provenance {
            resolved_at: "2026-09-01T00:00:00Z".into(),
            snapshot: "2026-09-01".into(),
            repos: Vec::new(),
            libalpm: "0".into(),
        },
    }
}
