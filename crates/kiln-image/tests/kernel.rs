//! Kernel placement and the initramfs.
//!
//! What matters here is asserted without a kernel: the sandbox specs (because a
//! host-only initramfs or a networked dracut is a defect you find at boot), the
//! pure bootability decision, and the tree surgery.

mod scratch;

use kiln_image::kernel::{self, Kernel};
use kiln_sandbox::{Network, Sandbox};

fn image_with_kernel(name: &str, kver: &str) -> std::path::PathBuf {
    let root = scratch::root(name);
    scratch::file(
        &root,
        &format!("usr/lib/modules/{kver}/pkgbase"),
        "linux\n",
        0o644,
    );
    scratch::file(
        &root,
        &format!("usr/lib/modules/{kver}/vmlinuz"),
        "MZ\n",
        0o644,
    );
    root
}

#[test]
fn the_kernel_is_the_module_directory_with_a_pkgbase() {
    let root = image_with_kernel("kernel-find", "6.19.2-arch1-1");
    // An out-of-tree module's package puts its own directory alongside, without
    // a `pkgbase` — which is exactly what tells them apart.
    scratch::file(
        &root,
        "usr/lib/modules/6.19.2-arch1-1/extramodules/v4l2loopback.ko.zst",
        "",
        0o644,
    );
    assert_eq!(
        kernel::find(&root).unwrap(),
        Kernel {
            version: "6.19.2-arch1-1".into(),
            moddir: "usr/lib/modules/6.19.2-arch1-1".into(),
        }
    );
}

/// An image with no kernel is an image that cannot boot, and the message should
/// say that rather than "no such file or directory".
#[test]
fn no_kernel_says_there_is_nothing_to_boot() {
    let root = scratch::root("kernel-none");
    let err = kernel::find(&root).unwrap_err();
    assert!(format!("{err}").contains("nothing to boot"), "{err}");
}

/// Two kernels mean two initramfs images, two BLS entries and a choice about
/// which one boots. That is a real feature and not one phase 2 has, so it says
/// so rather than picking the alphabetically first one and being wrong half the
/// time.
#[test]
fn two_kernels_is_refused_rather_than_guessed() {
    let root = image_with_kernel("kernel-two", "6.19.2-arch1-1");
    scratch::file(
        &root,
        "usr/lib/modules/6.12.8-lts1-1/pkgbase",
        "linux-lts\n",
        0o644,
    );
    let err = kernel::find(&root).unwrap_err();
    let text = format!("{err}");
    assert!(
        text.contains("6.12.8-lts1-1") && text.contains("6.19.2-arch1-1"),
        "{text}"
    );
}

/// on a current Arch this is already a no-op, because the `linux`
/// package ships `vmlinuz` next to `pkgbase`. Kept as a fallback, not designed
/// around — and the test says which case it is exercising.
#[test]
fn placing_vmlinuz_is_a_no_op_on_a_current_arch() {
    let root = image_with_kernel("kernel-k1", "6.19.2-arch1-1");
    let kernel = kernel::find(&root).unwrap();
    assert!(
        !kernel::place_vmlinuz(&root, &kernel).unwrap(),
        "nothing had to move"
    );
}

#[test]
fn placing_vmlinuz_falls_back_to_boot() {
    let root = scratch::root("kernel-k1-fallback");
    scratch::file(
        &root,
        "usr/lib/modules/6.19.2-arch1-1/pkgbase",
        "linux\n",
        0o644,
    );
    scratch::file(&root, "boot/vmlinuz-linux", "MZ\n", 0o644);

    let kernel = kernel::find(&root).unwrap();
    assert!(
        kernel::place_vmlinuz(&root, &kernel).unwrap(),
        "the fallback moved it"
    );
    assert!(root.join(kernel.vmlinuz()).is_file());
    assert!(!root.join("boot/vmlinuz-linux").exists());
}

/// Kernel step 4. `--no-hostonly` is not an optimization: a host-only initramfs
/// contains the modules the *builder* needs to boot, and the difference from
/// what the target needs only shows up as a machine that does not come up.
#[test]
fn dracut_is_reproducible_host_independent_and_offline() {
    let root = image_with_kernel("kernel-dracut", "6.19.2-arch1-1");
    let kernel = kernel::find(&root).unwrap();
    let spec = kernel::dracut_spec(&root, &kernel);

    assert_eq!(spec.network, Network::Disabled);
    assert_eq!(
        spec.env.get("SOURCE_DATE_EPOCH").map(String::as_str),
        Some("0")
    );
    for flag in ["--no-hostonly", "--no-hostonly-cmdline", "--reproducible"] {
        assert!(spec.command.iter().any(|a| a == flag), "missing {flag}");
    }
    // The kernel it builds for is the image's, named explicitly — never
    // whatever `uname -r` says on the builder.
    let kver = spec.command.iter().position(|a| a == "--kver").unwrap();
    assert_eq!(spec.command[kver + 1], "6.19.2-arch1-1");
    // And the module that makes the image boot at all.
    let add = spec.command.iter().position(|a| a == "--add").unwrap();
    assert_eq!(spec.command[add + 1], kernel::DRACUT_MODULE);
}

/// The command runs inside the staging root, so the paths it is given are
/// image-absolute. A staging-root-relative path here would write the initramfs
/// into a directory of that name inside the image.
#[test]
fn the_initramfs_path_is_image_absolute() {
    let root = image_with_kernel("kernel-paths", "6.19.2-arch1-1");
    let kernel = kernel::find(&root).unwrap();
    let spec = kernel::dracut_spec(&root, &kernel);
    assert_eq!(
        spec.command.last().unwrap(),
        "/usr/lib/modules/6.19.2-arch1-1/initramfs.img"
    );
    assert_eq!(spec.root, root);
}

/// The whole argv, once, so that a change to the isolation is visible in a
/// review rather than at boot.
#[test]
fn the_full_dracut_command_line() {
    let root = image_with_kernel("kernel-argv", "6.19.2-arch1-1");
    let kernel = kernel::find(&root).unwrap();
    let bwrap = kiln_sandbox::Bubblewrap::new(root.join("../scratch"));
    let argv = bwrap.argv(&kernel::dracut_spec(&root, &kernel)).unwrap();
    // The staging root's path varies per machine; the rest must not.
    let rendered = argv.join(" ").replace(root.to_str().unwrap(), "<root>");
    insta::assert_snapshot!(rendered);
}

/// dracut exits 0 having produced an initramfs that boots to an
/// emergency shell; the exit code is not the check.
#[test]
fn an_initramfs_without_the_ostree_module_is_rejected() {
    let listing = "usr/lib/systemd/systemd\nusr/bin/sh\n";
    let err = kernel::initramfs_is_bootable(listing).unwrap_err();
    insta::assert_snapshot!(err);
}

#[test]
fn an_initramfs_with_it_is_accepted() {
    let listing = "usr/lib/ostree/ostree-prepare-root\nusr/lib/systemd/systemd\n";
    assert_eq!(kernel::initramfs_is_bootable(listing), Ok(()));
}

/// Kernel step 6. OSTree owns `/boot`, and anything the transaction left in the
/// staging root's is a conflict at commit time. The directory stays: it is a
/// mountpoint.
#[test]
fn boot_is_emptied_but_not_removed() {
    let root = image_with_kernel("kernel-boot", "6.19.2-arch1-1");
    scratch::file(&root, "boot/vmlinuz-linux", "MZ\n", 0o644);
    scratch::file(&root, "boot/initramfs-linux.img", "cpio\n", 0o644);
    scratch::dir(&root, "boot/grub", 0o755);

    let mut removed = kernel::clear_boot(&root).unwrap();
    removed.sort();
    assert_eq!(removed, ["grub", "initramfs-linux.img", "vmlinuz-linux"]);
    assert!(root.join("boot").is_dir());
    assert_eq!(
        kiln_image::tree::entries(&root.join("boot")).unwrap().len(),
        0
    );
}
