//! Deployment, generations, and rollback.
//!
//! **`ostree admin rollback` does not exist**. libostree 2026.4
//! has `set-default`, `undeploy` and `pin`; there is no rollback verb, and
//! `set-default` is itself not a libostree call — it is a reordering of the
//! deployment list followed by `write_deployments`. `kiln rollback` is Kiln's
//! own operation, and this module is where that is true rather than assumed.

use crate::commit;
use crate::grubcfg;
use crate::grubenv;
use crate::{Error, Result};
use kiln_manifest::Manifest;
use ostree::gio;
use ostree::{Deployment, SysrootSimpleWriteDeploymentFlags};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The stateroot. Every Kiln deployment lives under one, and there is one.
///
/// Not the image name: `/var` belongs to the stateroot, and giving each image
/// its own would mean switching images silently switched `/var` too — which is
/// the one thing on the machine that is *not* supposed to be rebuilt.
pub const STATEROOT: &str = "kiln";

/// The binary libostree needs *inside the image* for its grub2 backend.
pub const GRUB_MKCONFIG: &str = "usr/bin/grub-mkconfig";

/// Which bootloader backend libostree drives for this sysroot.
///
/// Two values, not libostree's six: Kiln settled on GRUB2, and the alternative
/// is not another bootloader but *no* bootloader configuration at all — BLS
/// fragments for something else to read, which is what an image with no `grub`
/// in it can honestly claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Grub2,
    None,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Grub2 => "grub2",
            Backend::None => "none",
        }
    }
}

/// A sysroot Kiln can act on. Thin, because everything interesting is a
/// question about *which deployment*, not about libostree.
pub struct Sysroot {
    inner: ostree::Sysroot,
    path: PathBuf,
}

/// One deployment, as `kiln list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub number: u64,
    pub checksum: String,
    pub built_at: String,
    pub image: String,
    /// libostree's index *today*. Deliberately not shown to the user and not
    /// accepted from them: indices renumber as deployments come and go, so
    /// today's 1 is tomorrow's 0.
    pub index: i32,
    pub booted: bool,
    pub pinned: bool,
    /// The one that boots next: position 0 in the deployment list.
    ///
    /// A different question from `booted`, and the reason `kiln list` can say
    /// anything at all about a generation that was just applied — a staged
    /// deployment sits in front of the running one from the moment `kiln apply`
    /// returns, so in the steady state this is the booted one and after an
    /// apply it is not.
    pub boots_next: bool,
    /// The one `kiln rollback` would move to.
    pub rollback_target: bool,
    /// Generation 1: the floor, and deliberately a generation known to
    /// have booted on this exact hardware. Automatic rollback needs somewhere
    /// to roll back *to*, and a user's second-ever build could be the one that
    /// does not boot — so this one is pinned when it is deployed and `kiln
    /// clean` will not take it without `--remove-baseline`.
    pub baseline: bool,
}

/// Which generation is the floor. Not "the oldest still deployed":
/// removing the baseline and having the next-oldest silently inherit the
/// protection would mean nothing is ever actually removable, and would make
/// `--remove-baseline` a flag that removes one thing and creates another.
pub const BASELINE: u64 = 1;

/// Whether a newly staged generation is being counted, and if not, why.
///
/// Three outcomes rather than a bool, because the two ways of *not* counting
/// mean different things to the person reading them: one is a property of the
/// image they built, the other is a property of the machine they are on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Counter {
    /// On probation for this many attempts.
    Armed(u32),
    /// The image ships nothing that could clear a counter, so arming one would
    /// trap it: three boots and an automatic rollback, for no reason at all.
    ImageCannotBless,
    /// `grubenv` could not be written — most often a read-only `/boot`. The
    /// deployment is fine; the safety net is not there.
    Unwritable(String),
}

#[derive(Debug, Clone)]
pub struct Deployed {
    pub generation: u64,
    pub checksum: String,
    /// True when the deployment is staged for next boot rather than written
    /// immediately. Staging is what avoids touching the bootloader from a
    /// running system.
    pub staged: bool,
    /// Whether this deployment is on probation, and if not, why not.
    pub counted: Counter,
    /// This deploy created the baseline, and pinned it.
    pub baseline: bool,
    /// `/boot/grub/grub.cfg` was a regular file and is now the symlink
    /// libostree's swap can reach. True at most once per machine, and worth
    /// saying out loud when it happens: it means this machine was one deploy
    /// away from stopping in the initramfs.
    pub grub_cfg_repaired: bool,
    /// Which bootloader backend the sysroot was configured for, decided from
    /// the commit. `None` means the image ships no `grub`, so libostree
    /// writes BLS fragments and nothing regenerates a `grub.cfg`.
    pub backend: Backend,
}

impl Sysroot {
    /// Open an existing sysroot. `path` is `/` on a real machine and a
    /// directory under `target/` in a test.
    pub fn open(path: &Path) -> Result<Sysroot> {
        let inner = ostree::Sysroot::new(Some(&gio::File::for_path(path)));
        inner
            .load(gio::Cancellable::NONE)
            .map_err(Error::of("opening the sysroot"))?;
        Ok(Sysroot {
            inner,
            path: path.to_path_buf(),
        })
    }

    /// Create the sysroot layout. Kiln does not install anything, but it
    /// exposes this so that an installer can be written against it.
    pub fn init(path: &Path) -> Result<Sysroot> {
        // `boot` as well as the sysroot itself: libostree writes the BLS
        // entries there and fails with a bare `opendir(boot)` if it is missing.
        // On a real machine it is a mounted ext4 partition; creating
        // the mountpoint is the installer's job, and says Kiln exposes
        // this so an installer can be written against it.
        for dir in [path, &path.join("boot")] {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                doing: "creating the sysroot at",
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let inner = ostree::Sysroot::new(Some(&gio::File::for_path(path)));
        inner
            .ensure_initialized(gio::Cancellable::NONE)
            .map_err(Error::of("initializing the sysroot"))?;
        inner
            .load(gio::Cancellable::NONE)
            .map_err(Error::of("loading the new sysroot"))?;
        inner
            .init_osname(STATEROOT, gio::Cancellable::NONE)
            .or_else(|e| {
                // Already there is success, not failure: `sysroot init` has to
                // be safe to run twice.
                if e.message().contains("File exists") {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(Error::of("creating the stateroot"))?;

        // libostree 2026.4's `init_osname` creates `deploy/` and `backing/` but
        // **not** the stateroot's `var`, and `deploy_tree` then fails with a
        // bare `opendir(ostree/deploy/kiln/var): No such file or directory`.
        // This is the persistent `/var` every deployment shares — the one thing
        // on the machine that is deliberately not rebuilt — so it has
        // to exist before the first deployment, not after it.
        let stateroot_var = path.join("ostree/deploy").join(STATEROOT).join("var");
        std::fs::create_dir_all(&stateroot_var).map_err(|source| Error::Io {
            doing: "creating the stateroot's /var at",
            path: stateroot_var,
            source,
        })?;

        inner
            .load(gio::Cancellable::NONE)
            .map_err(Error::of("reloading the sysroot"))?;
        let sysroot = Sysroot {
            inner,
            path: path.to_path_buf(),
        };
        // There is no commit to ask yet, and `backend_for` needs one. `none` is
        // the safe starting point — BLS entries, which every backend writes —
        // and the first deploy raises it to grub2 if that will work. Starting
        // at grub2 would mean a sysroot that cannot be deployed into until
        // something corrects it.
        sysroot.configure(Backend::None)?;
        Ok(sysroot)
    }

    /// The repository settings Kiln depends on, written into the sysroot's own
    /// `ostree/repo/config`. these stay Kiln's business rather than
    /// leaking into an installer that should not have to know them.
    ///
    /// `backend` is decided by `backend_for`, and written explicitly rather
    /// than left to libostree's `auto`. `auto` asks whether a `grub.cfg`
    /// already exists on the sysroot, which is a reasonable question and not
    /// the one Kiln needs answered: it makes the backend depend on whether an
    /// installer happened to run `grub-install` before or after the first
    /// deploy, and silently switches Kiln onto a code path that cannot work
    /// under `--sysroot` at all. Deciding it here makes both outcomes
    /// deterministic and reportable.
    ///
    /// Deliberately **not** `boot-counting-tries`. libostree would then write
    /// `ostree-42+3.conf` BLS filenames, and nothing on this path decrements
    /// them — BLS boot counting is the bootloader's job and GRUB2 does not
    /// implement it. Kiln's counter lives in `grubenv`; see `crate::grubenv`.
    ///
    /// Idempotent, and called on every write path rather than only by `init`,
    /// so a sysroot created by an older Kiln gains the settings on its next
    /// deploy rather than staying subtly different forever.
    pub fn configure(&self, backend: Backend) -> Result<()> {
        let repo = self.repo();
        let config = repo.copy_config();
        if config.string("sysroot", "bootloader").as_deref() == Ok(backend.as_str()) {
            return Ok(());
        }
        config.set_string("sysroot", "bootloader", backend.as_str());
        repo.write_config(&config)
            .map_err(Error::of("writing the repository configuration"))
    }

    /// Ensure libostree's regenerated `grub.cfg` is the one GRUB reads.
    ///
    /// Only under the grub2 backend. `Backend::None` means nothing on this
    /// sysroot regenerates a `grub.cfg` at all, so there is no swap for a link
    /// to follow and the file is still the installer's — creating the
    /// link there would point GRUB at something no deploy has written yet.
    ///
    /// See [`crate::grubcfg`] for why a regular file at that path is a machine
    /// that boots exactly once more.
    fn link_grub_cfg(&self, backend: Backend) -> Result<bool> {
        match backend {
            Backend::Grub2 => Ok(grubcfg::link(&self.path)? == grubcfg::Link::Repaired),
            Backend::None => Ok(false),
        }
    }

    /// Which bootloader backend libostree can actually drive for this commit on
    /// this sysroot. Two conditions, both measured against ostree 2026.4 rather
    /// than assumed, and both of them failures rather than degradations if
    /// gotten wrong — the deploy dies after the tree is already checked out.
    ///
    /// **The image must contain `grub-mkconfig`.** libostree regenerates
    /// `grub.cfg` by running it *chrooted into the deployment*, which is also
    /// why Kiln's `/etc/grub.d` fragment works at all. An image without
    /// the `grub` package gets `Failed to execute child process`.
    ///
    /// **The sysroot must be `/`.** Into that same chroot libostree passes a
    /// **host-absolute** output path, `<sysroot>/boot/loader.N/grub.cfg`. When
    /// the sysroot is `/` that path means the same thing inside and out. Under
    /// `--sysroot /mnt` it does not exist inside the chroot at all, and
    /// grub-mkconfig exits non-zero having written nothing.
    ///
    /// So an installer's deploys get `None`: BLS entries and no `grub.cfg`,
    /// which is honest, because writing the bootloader onto the disk is the
    /// installer's job anyway. Its `grub-install` runs `grub-mkconfig`
    /// inside the target, picks up Kiln's fragment and libostree's own
    /// `15_ostree`, and produces exactly the config this would have. From then
    /// on the machine deploys to `/` and Kiln maintains it.
    pub fn backend_for(&self, checksum: &str) -> Backend {
        if self.path != Path::new("/") {
            return Backend::None;
        }
        match self.commit_has(checksum, GRUB_MKCONFIG) {
            true => Backend::Grub2,
            false => Backend::None,
        }
    }

    /// Is `path` in this commit's tree? Read from the repository, without
    /// checking anything out — the questions here are asked *before* the
    /// deployment exists.
    fn commit_has(&self, checksum: &str, path: &str) -> bool {
        use ostree::gio::prelude::FileExt;
        let Ok((root, _)) = self.repo().read_commit(checksum, gio::Cancellable::NONE) else {
            return false;
        };
        root.resolve_relative_path(path)
            .query_exists(gio::Cancellable::NONE)
    }

    pub fn repo(&self) -> ostree::Repo {
        self.inner.repo()
    }

    /// Where a generation's tree is checked out, for the commands that query
    /// the *image* rather than the plan that made it — `kiln why` and `kiln
    /// owns` read its pacman database.
    ///
    /// A generation that is committed but not deployed has no checkout, and
    /// that is a real answer rather than an error: the commit is still there,
    /// and `kiln show` and `kiln rebuild` both work on it. Only the questions
    /// that need a filesystem need this.
    pub fn deployment_root(&self, generation: u64) -> Result<PathBuf> {
        let generations = self.generations()?;
        let position = generations
            .iter()
            .position(|g| g.number == generation)
            .ok_or_else(|| Error::NoSuchGeneration {
                wanted: generation,
                available: generations.iter().map(|g| g.number).collect(),
            })?;
        Ok(self.deployment_path(&self.inner.deployments()[position]))
    }

    /// The on-disk directory of a deployment, absolute.
    fn deployment_path(&self, deployment: &Deployment) -> PathBuf {
        self.path.join(
            self.inner
                .deployment_dirpath(deployment)
                .trim_start_matches('/'),
        )
    }

    /// Can this commit clear its own boot counter?
    ///
    /// The mechanism has two halves and Kiln only owns one of them from
    /// here: it can arm the counter, but the *image* is what disarms it, from
    /// `kiln-boot-success.service` by way of `grub-editenv`. Arming an image
    /// that cannot do that would be putting a working one on probation it can
    /// never leave — three boots and an automatic rollback to something older,
    /// for no reason at all. So the counter is armed only when the thing that
    /// clears it is demonstrably present.
    pub fn can_bless(&self, checksum: &str) -> bool {
        self.commit_has(checksum, "usr/bin/grub-editenv")
            && self.commit_has(checksum, crate::grubenv::BLESS)
    }

    /// Put a freshly deployed generation on probation, if it is able to
    /// come off it. See `can_bless` for why that condition is not optional.
    ///
    /// Never fails the deploy. By the time this runs the tree is committed and
    /// the deployment written, and `/boot` being read-only — which it is on a
    /// good many booted OSTree systems — would otherwise turn a perfectly good
    /// `kiln apply` into an error after the fact. An unarmed counter is a
    /// missing safety net, which is worth saying out loud; it is not a reason
    /// to throw away the image.
    fn arm(&self, checksum: &str, tries: u32) -> Counter {
        if tries == 0 || !self.can_bless(checksum) {
            return Counter::ImageCannotBless;
        }
        match grubenv::arm(&self.path, tries) {
            Ok(()) => Counter::Armed(tries),
            // `/boot` read-only is the common case on a booted system, and
            // `kiln apply` runs as root there just as `kiln-boot-success.service`
            // does inside the image — so take the same remount-write-remount
            // path rather than settling for an unarmed counter. Only tried
            // against `/`: under `--sysroot` there is no live mount to take
            // read-write, and `self.path` is a plain directory.
            Err(first) if self.path == Path::new("/") => {
                match remount_rw_and_retry(&self.boot(), || grubenv::arm(&self.path, tries)) {
                    Ok(()) => Counter::Armed(tries),
                    Err(_) => Counter::Unwritable(first.to_string()),
                }
            }
            Err(e) => Counter::Unwritable(e.to_string()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `/boot` inside this sysroot, where the BLS entries live.
    pub fn boot(&self) -> PathBuf {
        self.path.join("boot")
    }

    /// Every deployment, newest first, as generations.
    pub fn generations(&self) -> Result<Vec<Generation>> {
        let deployments = self.inner.deployments();
        let booted = self.inner.booted_deployment().map(|d| d.csum().to_string());
        let repo = self.repo();

        let mut out = Vec::new();
        for (position, deployment) in deployments.iter().enumerate() {
            let checksum = deployment.csum().to_string();
            let metadata = commit::read_metadata(&repo, &checksum)?;
            out.push(Generation {
                number: metadata.generation,
                built_at: metadata.built_at,
                image: metadata.image,
                index: deployment.index(),
                booted: booted.as_deref() == Some(checksum.as_str()),
                pinned: deployment.is_pinned(),
                boots_next: position == 0,
                // the deployment list is in boot order, so the rollback
                // target is the second entry — not "the one before the booted
                // one", which is the same thing only when the booted deployment
                // is also the default.
                rollback_target: position == 1,
                baseline: metadata.generation == BASELINE,
                checksum,
            });
        }
        Ok(out)
    }

    /// Deploy a commit for the next boot, staging it when that is possible.
    ///
    /// Staging is what avoids touching the bootloader from a running system:
    /// the deployment is finalized at shutdown, so a machine that loses power
    /// mid-`apply` still boots what it booted before. libostree only offers it
    /// to a system that is *itself* booted from OSTree — otherwise
    /// `stage_tree` fails with "Not currently booted into an OSTree system" —
    /// so a sysroot being installed into, or a test's, is written immediately
    /// instead. There is nothing to protect there: nothing is running from it.
    ///
    /// Kargs are **fully declarative**: the complete set from `kernel.cmdline`
    /// is passed every time, so removing a line from the TOML actually removes
    /// the karg. rpm-ostree's `kargs --append/--delete` model lets the live set
    /// drift from any written source of truth, and then nothing can say what
    /// the machine will boot with.
    pub fn deploy(
        &self,
        checksum: &str,
        manifest: &Manifest,
        image: &str,
        tries: u32,
    ) -> Result<Deployed> {
        if !self.inner.is_booted() {
            return self.deploy_now(checksum, manifest, image, tries);
        }
        let backend = self.backend_for(checksum);
        self.configure(backend)?;
        let grub_cfg_repaired = self.link_grub_cfg(backend)?;
        let kargs = kargs(manifest);
        let kargs: Vec<&str> = kargs.iter().map(String::as_str).collect();

        let origin = self
            .inner
            .origin_new_from_refspec(&format!("kiln/{image}/{}", manifest.image.arch));
        let merge = self.inner.merge_deployment(Some(STATEROOT));

        let deployment = self
            .inner
            .stage_tree(
                Some(STATEROOT),
                checksum,
                Some(&origin),
                merge.as_ref(),
                &kargs,
                gio::Cancellable::NONE,
            )
            .map_err(Error::of("staging the deployment"))?;

        let generation = commit::read_metadata(&self.repo(), checksum)?.generation;
        let counted = self.arm(checksum, tries);
        Ok(Deployed {
            generation,
            checksum: deployment.csum().to_string(),
            staged: deployment.is_staged(),
            counted,
            baseline: false,
            backend,
            grub_cfg_repaired,
        })
    }

    /// Deploy immediately rather than staging. What an installer writing a
    /// fresh disk needs, and what a test needs: a staged deployment is not in
    /// the deployment list until the next boot, so nothing can assert on it.
    ///
    /// `deploy` picks this automatically for a sysroot that is not the booted
    /// one; it is public because an installer wants to say so explicitly.
    pub fn deploy_now(
        &self,
        checksum: &str,
        manifest: &Manifest,
        image: &str,
        tries: u32,
    ) -> Result<Deployed> {
        let backend = self.backend_for(checksum);
        self.configure(backend)?;
        let grub_cfg_repaired = self.link_grub_cfg(backend)?;
        let kargs = kargs(manifest);
        let kargs: Vec<&str> = kargs.iter().map(String::as_str).collect();

        let origin = self
            .inner
            .origin_new_from_refspec(&format!("kiln/{image}/{}", manifest.image.arch));
        let merge = self.inner.merge_deployment(Some(STATEROOT));

        let deployment = self
            .inner
            .deploy_tree(
                Some(STATEROOT),
                checksum,
                Some(&origin),
                merge.as_ref(),
                &kargs,
                gio::Cancellable::NONE,
            )
            .map_err(Error::of("deploying"))?;

        self.inner
            .simple_write_deployment(
                Some(STATEROOT),
                &deployment,
                merge.as_ref(),
                SysrootSimpleWriteDeploymentFlags::NONE,
                gio::Cancellable::NONE,
            )
            .map_err(Error::of("writing the deployment"))?;
        self.inner
            .load(gio::Cancellable::NONE)
            .map_err(Error::of("reloading after deploying"))?;

        let generation = commit::read_metadata(&self.repo(), checksum)?.generation;
        let counted = self.arm(checksum, tries);

        // The baseline is pinned by whoever created it, at the moment it
        // is created — not by a later `kiln clean` noticing it is the oldest.
        // Automatic rollback needs somewhere to roll back *to*, and a user's
        // second-ever build could be the one that does not boot.
        let baseline = generation == BASELINE;
        if baseline {
            self.inner
                .deployment_set_pinned(&deployment, true)
                .map_err(Error::of("pinning the baseline generation"))?;
        }

        Ok(Deployed {
            generation,
            checksum: deployment.csum().to_string(),
            staged: false,
            counted,
            baseline,
            backend,
            grub_cfg_repaired,
        })
    }

    /// Make `generation` the default for the next boot.
    ///
    /// This is `ostree admin set-default` and it is not a libostree call: the
    /// deployment list is reordered so the wanted one is first, and
    /// `write_deployments` rewrites the BLS entries from it. Which is why
    /// `kiln rollback` had to be Kiln's own command — there was
    /// never anything to pass through to.
    pub fn set_default(&self, generation: u64) -> Result<Generation> {
        let generations = self.generations()?;
        let position = generations
            .iter()
            .position(|g| g.number == generation)
            .ok_or_else(|| Error::NoSuchGeneration {
                wanted: generation,
                available: generations.iter().map(|g| g.number).collect(),
            })?;

        let mut deployments: Vec<Deployment> = self.inner.deployments();
        let chosen = deployments.remove(position);
        deployments.insert(0, chosen);

        self.inner
            .write_deployments(&deployments, gio::Cancellable::NONE)
            .map_err(Error::of("reordering the deployments"))?;
        self.inner
            .load(gio::Cancellable::NONE)
            .map_err(Error::of("reloading after reordering"))?;

        // A generation chosen by hand gets no probation: `kiln deploy`
        // and `kiln rollback` are both deliberate, and counting attempts
        // against a decision the user just made would roll it back underneath
        // them. Writing `boot_success=1` also resolves any demotion `kiln
        // status` was reporting, which is the right moment for it — the user
        // has now acted on the news.
        //
        // Best-effort for the same reason as `arm`: the deployments have
        // already been reordered, and a read-only `/boot` must not turn a
        // completed `kiln rollback` into a failure. The worst case is a stale
        // counter, which the next boot spends and `kiln status` explains.
        grubenv::disarm(&self.path).ok();

        self.generations()?
            .into_iter()
            .find(|g| g.number == generation)
            .ok_or_else(|| Error::NoSuchGeneration {
                wanted: generation,
                available: Vec::new(),
            })
    }

    pub fn set_pinned(&self, generation: u64, pinned: bool) -> Result<()> {
        let generations = self.generations()?;
        let position = generations
            .iter()
            .position(|g| g.number == generation)
            .ok_or_else(|| Error::NoSuchGeneration {
                wanted: generation,
                available: generations.iter().map(|g| g.number).collect(),
            })?;
        let deployments = self.inner.deployments();
        self.inner
            .deployment_set_pinned(&deployments[position], pinned)
            .map_err(Error::of("pinning the deployment"))
    }

    /// Undeploy the named generations, then prune. `kiln rm`.
    ///
    /// One `write_deployments` for the whole set rather than one per
    /// generation: libostree renumbers deployment indices on every write, so
    /// removing three generations one at a time means computing the list three
    /// times against three different numberings, and the second removal takes
    /// the wrong deployment. The refusals are already decided by
    /// `Removal::plan`; this is the part that touches the disk.
    pub fn remove(&self, generations: &[u64]) -> Result<()> {
        let known = self.generations()?;
        for wanted in generations {
            if !known.iter().any(|g| g.number == *wanted) {
                return Err(Error::NoSuchGeneration {
                    wanted: *wanted,
                    available: known.iter().map(|g| g.number).collect(),
                });
            }
        }

        let deployments = self.inner.deployments();
        let keep: Vec<Deployment> = known
            .iter()
            .zip(deployments.iter())
            .filter(|(g, _)| !generations.contains(&g.number))
            .map(|(_, d)| d.clone())
            .collect();

        // A pinned deployment that is being removed has to be unpinned first:
        // libostree's own cleanup keeps pinned deployments, and one left in the
        // list would be undeployed here and resurrected by the next prune.
        for (g, d) in known.iter().zip(deployments.iter()) {
            if generations.contains(&g.number) && g.pinned {
                self.inner
                    .deployment_set_pinned(d, false)
                    .map_err(Error::of("unpinning a deployment before removing it"))?;
            }
        }

        self.inner
            .write_deployments(&keep, gio::Cancellable::NONE)
            .map_err(Error::of("removing deployments"))?;
        self.inner
            .load(gio::Cancellable::NONE)
            .map_err(Error::of("reloading after removing deployments"))?;
        self.cleanup()
    }

    pub fn cleanup(&self) -> Result<()> {
        self.inner
            .cleanup(gio::Cancellable::NONE)
            .map_err(Error::of("cleaning up old deployments"))
    }
}

/// Take `mountpoint` read-write, run `write`, then put it back read-only —
/// the same dance `kiln-boot-success.service`'s script does inside the image
/// (`kiln_image::bootcount::BOOT_SUCCESS`), done here from the live host so
/// arming the counter doesn't need to wait for that service's first run.
///
/// Best-effort on both remounts: if `mountpoint` is not actually a separate
/// mount (a `--sysroot` directory, or a system that already mounts `/boot`
/// read-write), the `remount,rw` fails and `write` runs against whatever is
/// already there — no worse than not trying. If the `remount,ro` afterwards
/// fails, the write already succeeded and is not undone by leaving `/boot`
/// writable; that's the same trade `kiln-boot-success.service` makes.
fn remount_rw_and_retry<T>(mountpoint: &Path, write: impl FnOnce() -> Result<T>) -> Result<T> {
    let remounted = Command::new("mount")
        .args(["-o", "remount,rw"])
        .arg(mountpoint)
        .status()
        .is_ok_and(|s| s.success());
    let result = write();
    if remounted {
        let _ = Command::new("mount")
            .args(["-o", "remount,ro"])
            .arg(mountpoint)
            .status();
    }
    result
}

/// What `kiln rm` and `kiln clean` would do, decided without touching the disk.
///
/// Pure on purpose. Which generations survive a `clean` is *policy*,
/// and policy that can only be checked by deploying four images and running the
/// command is policy nobody checks. Everything here is a decision about a list
/// of `Generation`s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removal {
    pub remove: Vec<u64>,
    /// Generations that were asked for or would otherwise have been removed,
    /// with the reason they are being kept. Rendered by the CLI; a `clean` that
    /// silently keeps things is one nobody can predict.
    pub refused: Vec<(u64, &'static str)>,
}

/// Why a generation cannot be removed, in the order the reasons are checked.
/// The first one that applies is the one reported: "it is booted" is more
/// useful than "it is pinned" about a deployment that is both.
fn protection(g: &Generation, remove_baseline: bool) -> Option<&'static str> {
    if g.booted {
        return Some("it is the running system");
    }
    // The baseline is checked before the pin and *returns* either way, because
    // Kiln pinned it itself when it deployed it. Falling through to the
    // pin check would make `--remove-baseline` a flag that never works on its
    // own — the user would have to `kiln unpin 1` as well, to release a pin
    // they did not place — and a flag whose whole job is to name one specific
    // generation should be enough to remove it.
    if g.baseline {
        return match remove_baseline {
            true => None,
            false => Some("it is the baseline; `--remove-baseline` overrides"),
        };
    }
    if g.pinned {
        return Some("it is pinned; `kiln unpin` releases it");
    }
    None
}

impl Removal {
    /// `kiln rm <gen>...`: exactly what was named, minus what is protected.
    pub fn requested(generations: &[Generation], wanted: &[u64], remove_baseline: bool) -> Removal {
        let mut out = Removal::default();
        for number in wanted {
            match generations.iter().find(|g| g.number == *number) {
                None => continue,
                Some(g) => match protection(g, remove_baseline) {
                    Some(why) => out.refused.push((*number, why)),
                    None => out.remove.push(*number),
                },
            }
        }
        out
    }

    /// `kiln clean --keep N`: the budget — N generations, plus the
    /// baseline, plus anything pinned, plus the running system.
    ///
    /// The rollback target is not a separate rule. It is the second entry in
    /// boot order, so with any `keep` of 2 or more it is already inside the
    /// window; a `--keep 1` that deliberately asks for one generation should
    /// not silently get two.
    pub fn budget(generations: &[Generation], keep: usize, remove_baseline: bool) -> Removal {
        let mut out = Removal::default();
        for (position, g) in generations.iter().enumerate() {
            if position < keep {
                continue;
            }
            match protection(g, remove_baseline) {
                Some(why) => out.refused.push((g.number, why)),
                None => out.remove.push(g.number),
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.remove.is_empty()
    }
}

/// Deploy a commit onto a sysroot. The free function describes.
pub fn deploy(
    sysroot: &Sysroot,
    checksum: &str,
    manifest: &Manifest,
    image: &str,
    tries: u32,
) -> Result<Deployed> {
    sysroot.deploy(checksum, manifest, image, tries)
}

/// Roll back to the generation that is currently the rollback target.
///
/// It takes no argument on purpose: "the previous one" is what a person means
/// at 2am, and `kiln deploy <gen>` already exists for the case where they mean
/// something specific.
pub fn rollback(sysroot: &Sysroot) -> Result<Generation> {
    let generations = sysroot.generations()?;
    let target = generations
        .iter()
        .find(|g| g.rollback_target)
        .ok_or_else(|| Error::NoSuchGeneration {
            wanted: 0,
            available: generations.iter().map(|g| g.number).collect(),
        })?;
    sysroot.set_default(target.number)
}

/// The complete kernel command line for a deployment. every deploy passes
/// the whole set, so removing a line from the TOML removes the karg.
pub fn kargs(manifest: &Manifest) -> Vec<String> {
    manifest.kernel.cmdline.iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Newest first, as `generations()` returns them.
    fn gens(spec: &[(u64, bool, bool)]) -> Vec<Generation> {
        spec.iter()
            .enumerate()
            .map(|(position, (number, booted, pinned))| Generation {
                number: *number,
                checksum: format!("{number:0>64}"),
                built_at: "2026-09-01T00:00:00Z".into(),
                image: "workstation".into(),
                index: position as i32,
                booted: *booted,
                pinned: *pinned,
                boots_next: position == 0,
                rollback_target: position == 1,
                baseline: *number == BASELINE,
            })
            .collect()
    }

    /// three generations, plus the baseline, plus anything pinned, plus
    /// the running system. Generation 1 is the floor and survives being far
    /// outside the window.
    #[test]
    fn clean_keeps_three_the_baseline_and_the_pins() {
        let generations = gens(&[
            (9, true, false),
            (8, false, false),
            (7, false, false),
            (6, false, false),
            (5, false, true),
            (1, false, true),
        ]);
        let plan = Removal::budget(&generations, 3, false);
        assert_eq!(plan.remove, vec![6]);
        assert_eq!(
            plan.refused.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![5, 1]
        );
    }

    /// The booted deployment is never removable, however far down the list it
    /// is — a machine that is running generation 4 and has built five more is
    /// exactly the case `kiln clean` must not turn into an unbootable one.
    #[test]
    fn clean_never_removes_the_running_system() {
        let generations = gens(&[
            (9, false, false),
            (8, false, false),
            (7, false, false),
            (4, true, false),
        ]);
        let plan = Removal::budget(&generations, 3, false);
        assert!(plan.remove.is_empty());
        assert_eq!(plan.refused, vec![(4, "it is the running system")]);
    }

    /// The baseline goes only when it is asked for by name *and* the
    /// flag that exists to say "yes, I mean the floor" is given.
    #[test]
    fn the_baseline_needs_the_flag_that_names_it() {
        let generations = gens(&[(3, true, false), (1, false, true)]);

        let refused = Removal::requested(&generations, &[1], false);
        assert!(refused.remove.is_empty());
        assert!(refused.refused[0].1.contains("baseline"));

        let allowed = Removal::requested(&generations, &[1], true);
        assert_eq!(allowed.remove, vec![1]);
    }

    /// A pin is a statement, and `kiln rm` reports it rather than overriding
    /// it: the fix is one command the message names, and silently unpinning
    /// would make `kiln pin` mean nothing.
    #[test]
    fn removing_a_pinned_generation_names_the_command_that_releases_it() {
        let generations = gens(&[(3, true, false), (2, false, true)]);
        let plan = Removal::requested(&generations, &[2], false);
        assert!(plan.remove.is_empty());
        assert!(plan.refused[0].1.contains("kiln unpin"));
    }

    /// `--keep 1` asks for one generation and gets one. The rollback target is
    /// not a separate protection rule, because with any sane `keep` it is
    /// already inside the window.
    #[test]
    fn keep_one_keeps_one() {
        let generations = gens(&[(9, true, false), (8, false, false), (7, false, false)]);
        assert_eq!(Removal::budget(&generations, 1, false).remove, vec![8, 7]);
    }
}
