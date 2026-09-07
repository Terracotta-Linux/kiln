//! Step 9: kernel placement and the initramfs.
//!
//! The initramfs is generated **inside the staging root, against the staging
//! root's kernel and modules** — never the host's. The host runs a different
//! kernel, and an image whose initramfs came from it is an image that boots on
//! exactly one machine.
//!
//! The other rule here is *verify it, do not trust dracut's exit
//! code*. A silently-absent `ostree` dracut module produces an image that boots
//! to an emergency shell. That is the most expensive failure in the pipeline
//! and the one furthest from its cause, and `lsinitrd | grep
//! ostree-prepare-root` is the whole of the insurance.

use crate::tree::{self, Result};
use kiln_sandbox::SandboxSpec;
use std::collections::BTreeSet;
use std::path::Path;

/// The one string that decides whether an image boots. The
/// `50ostree` dracut module writes this binary into the initramfs, and it is
/// what pivots the sysroot before systemd starts.
pub const REQUIRED_IN_INITRAMFS: &str = "ostree-prepare-root";

/// The dracut module that provides it, shipped by the `ostree` package — which
/// is therefore a package the image must contain. Not waste: the
/// `ostree` binary is independently required in the image for `kiln status` and
/// `kiln list` to work on the booted machine.
pub const DRACUT_MODULE: &str = "ostree";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kernel {
    /// e.g. `6.19.2-arch1-1`. The directory name under `/usr/lib/modules`.
    pub version: String,
    /// `usr/lib/modules/<version>`, staging-root-relative.
    pub moddir: String,
}

impl Kernel {
    pub fn vmlinuz(&self) -> String {
        format!("{}/vmlinuz", self.moddir)
    }

    pub fn initramfs(&self) -> String {
        format!("{}/initramfs.img", self.moddir)
    }
}

/// Find the kernel by the Arch convention: the module directory that has a
/// `pkgbase` file. Package-shipped module directories from out-of-tree modules
/// do not have one, which is exactly what distinguishes them.
pub fn find(root: &Path) -> Result<Kernel> {
    let modules = root.join("usr/lib/modules");
    let mut found: Vec<String> = Vec::new();
    for entry in tree::entries(&modules)? {
        if entry.join("pkgbase").is_file() {
            if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
                found.push(name.to_string());
            }
        }
    }
    match found.len() {
        0 => Err(tree::shape(
            "no /usr/lib/modules/*/pkgbase in the image: it has no kernel package, \
             so there is nothing to boot"
                .to_string(),
        )),
        1 => Ok(Kernel {
            moddir: format!("usr/lib/modules/{}", found[0]),
            version: found.remove(0),
        }),
        // Two kernels mean two initramfs images, two BLS entries and a choice
        // about which one boots — a real feature, and not one phase 2 has. Say
        // so rather than picking the alphabetically first one.
        _ => Err(tree::shape(format!(
            "the image contains {} kernels ({}); Kiln builds a single-kernel image",
            found.len(),
            found.join(", ")
        ))),
    }
}

/// Kernel step 2. On a current Arch this is a no-op — the `linux` package ships
/// `vmlinuz` next to `pkgbase` already, and `/boot/vmlinuz-linux` is a pacman
/// hook's *copy*. Kept as a fallback for a kernel package that
/// does not, and deliberately not designed around.
pub fn place_vmlinuz(root: &Path, kernel: &Kernel) -> Result<bool> {
    let dest = root.join(kernel.vmlinuz());
    if dest.is_file() {
        return Ok(false);
    }
    for candidate in tree::entries(&root.join("boot"))? {
        let name = candidate.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("vmlinuz") {
            std::fs::rename(&candidate, &dest).map_err(tree::io("moving the kernel to", &dest))?;
            return Ok(true);
        }
    }
    Err(tree::shape(format!(
        "no vmlinuz for {}: not at /{} and not in /boot",
        kernel.version,
        kernel.vmlinuz()
    )))
}

/// `depmod -a <kver>`, run *inside* the staging root rather than on the host
/// with `-b`. Inside, the image's `/usr/lib/modules` is the only one there is,
/// so there is no `-b` to get wrong and no host module tree to read by
/// accident.
pub fn depmod_spec(root: &Path, kernel: &Kernel) -> SandboxSpec {
    SandboxSpec::in_root(root, ["depmod".into(), "-a".into(), kernel.version.clone()])
}

/// The dracut invocation. step 4, with the flags that make it
/// reproducible and host-independent.
///
/// `--no-hostonly` is not an optimization: a host-only initramfs contains the
/// modules the *builder* needs to boot, which is a different set from the ones
/// the target needs, and the difference only shows up as a machine that does
/// not come up.
///
/// `extra_modules` is `kernel.dracut_modules` from the manifest. Non-hostonly
/// mode does not include every module whose package is present — a module can
/// return "available but not automatic" from its own `check()`, as Plymouth's
/// does — so a module wanted in the initramfs has to be named, the same way
/// `DRACUT_MODULE` always is.
///
/// `drivers` is `kernel.modules.initramfs`, and it is a separate list because
/// a dracut *module* and a kernel *driver* are different things: `--add`
/// takes the first, `--add-drivers` the second. Naming drivers is not
/// belt-and-braces. A non-hostonly initramfs carries what is needed to reach
/// the root filesystem, which does not include a GPU — so the boot splash
/// starts with no device it will draw on and waits out its device timeout
/// while the firmware framebuffer sits there unused. dracut takes them as one
/// space-separated argument.
pub fn dracut_spec(
    root: &Path,
    kernel: &Kernel,
    extra_modules: &BTreeSet<String>,
    drivers: &BTreeSet<String>,
) -> SandboxSpec {
    let mut argv = vec![
        "dracut".to_string(),
        "--force".into(),
        "--no-hostonly".into(),
        "--no-hostonly-cmdline".into(),
        "--reproducible".into(),
        "--kver".into(),
        kernel.version.clone(),
        "--add".into(),
        DRACUT_MODULE.into(),
    ];
    for module in extra_modules {
        argv.push("--add".into());
        argv.push(module.clone());
    }
    if !drivers.is_empty() {
        argv.push("--add-drivers".into());
        argv.push(drivers.iter().cloned().collect::<Vec<_>>().join(" "));
    }
    argv.push(format!("/{}", kernel.initramfs()));

    SandboxSpec::in_root(root, argv)
        // `in_root` already binds /dev, /proc and /sys, which dracut needs, and
        // already refuses the network, which it does not. `SOURCE_DATE_EPOCH` is
        // in `default_env` for the same reason; setting it here too is saying it
        // twice, and names it explicitly enough that a reader should be able
        // to find it in this function.
        .with_env("SOURCE_DATE_EPOCH", "0")
}

/// `lsinitrd`, inside the image, over the image's own initramfs.
pub fn verify_spec(root: &Path, kernel: &Kernel) -> SandboxSpec {
    SandboxSpec::in_root(
        root,
        ["lsinitrd".to_string(), format!("/{}", kernel.initramfs())],
    )
}

/// Drivers named in `kernel.modules.initramfs` that did not make it in.
///
/// This exists because **dracut does not fail when it cannot find one**. It
/// logs `Failed to find module 'x'`, prints `dracut[E]: FAILED:`, and then
/// exits 0 with an image missing exactly the driver that was asked for.
/// Unchecked, a typo would reach the user as the symptom the key exists to
/// prevent — a boot splash with no device to draw on — with nothing in the
/// build pointing at the cause. Same rule as `initramfs_is_bootable`: verify
/// it, do not trust dracut's exit code.
///
/// `builtin` is the kernel's `modules.builtin`. A driver compiled into the
/// kernel cannot be put in an initramfs and does not need to be, so naming one
/// is redundant rather than wrong, and must not fail a build.
///
/// Names are compared with `-` folded to `_`, because that is what dracut does
/// with them: asking for `definitely-not-a-driver` gets you
/// `Failed to find module 'definitely_not_a_driver'`.
pub fn drivers_missing(listing: &str, builtin: &str, drivers: &BTreeSet<String>) -> Vec<String> {
    drivers
        .iter()
        .filter(|driver| {
            let file = format!("/{}.ko", driver.replace('-', "_"));
            !listing.contains(&file) && !builtin.contains(&file)
        })
        .cloned()
        .collect()
}

/// Given `lsinitrd`'s output, is this initramfs one that will boot?
///
/// Pure, and separate from running `lsinitrd`, because the *decision* is the
/// part worth testing and it should not need a kernel to test it.
pub fn initramfs_is_bootable(listing: &str) -> std::result::Result<(), String> {
    if listing.contains(REQUIRED_IN_INITRAMFS) {
        return Ok(());
    }
    Err(format!(
        "the initramfs does not contain `{REQUIRED_IN_INITRAMFS}`, so this image would boot \
         to an emergency shell. The `{DRACUT_MODULE}` dracut module ships in the `ostree` \
         package: check that it is in `packages.repo` and that dracut found \
         /usr/lib/dracut/modules.d/50{DRACUT_MODULE}."
    ))
}

/// Kernel step 6. OSTree owns `/boot`; the deployment's `/boot` comes from the
/// sysroot, and anything the transaction left in the staging root's is a
/// conflict at commit time.
///
/// The directory itself stays: it is a mountpoint.
pub fn clear_boot(root: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for entry in tree::entries(&root.join("boot"))? {
        if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
            removed.push(name.to_string());
        }
        tree::remove(&entry)?;
    }
    Ok(removed)
}
