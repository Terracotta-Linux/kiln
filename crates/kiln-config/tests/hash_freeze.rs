//! Hash-freeze.
//!
//! > A fixed corpus with committed expected `config_id`s. Refactoring must not
//! > change hashes; a deliberate change requires bumping a schema/hash version
//! > and updating the file in the same commit. Without this, an innocuous
//! > refactor silently invalidates every user's cache and forces the world to
//! > rebuild.
//!
//! These numbers are load-bearing. If this test fails, exactly one of two things
//! is true, and the failure message says which question to answer.

use kiln_config::Options;
use std::path::{Path, PathBuf};

/// Frozen at hash epoch 6 — `kernel.dkms` joined `Kernel`'s canonical encoding.
/// A DKMS package ships sources and expects the *machine* to compile them at
/// install time, which an immutable image never does; naming one now means Kiln
/// compiles it into the image instead, so which DKMS packages a configuration
/// names decides what drivers the image contains. All five fixtures moved
/// together — four of them name no DKMS package at all — which is what says it
/// was a schema change rather than a module content change (see cause (c)
/// below). `workstation` also gained a `kernel.dkms` line of its own, so the
/// corpus covers the key; that change rides along in the same commit.
///
/// Epochs 4 and 5 were the same shape of change: `kernel.dracut_modules` and
/// then `kernel.modules.initramfs` joining the canonical encoding, because
/// dracut's default, non-hostonly selection includes neither every module whose
/// package is installed nor the drivers a splash needs before there is a root
/// filesystem. Do not "fix" these by pasting new values.
///
/// Three of these fixtures include shipped modules, so their identity depends on
/// what those modules say — which is the point of them, and the third way a
/// value here can legitimately move (see cause (c) below). It is a *narrower*
/// licence than it looks: the module's own diff has to be in the same commit,
/// and the change has to be one that genuinely alters the image. Only
/// `workstation` has ever moved that way, when `@kiln/gpu/nvidia-open` stopped
/// installing `nvidia-open-dkms` — a DKMS package cannot build in an immutable
/// image, so the module had been shipping something that does not work.
/// `minimal` and `four-lines` did not move when the library was reorganized
/// around it, because restructuring `@kiln/profiles/minimal` into a composition
/// left the merged manifest identical. That is the check which says a
/// reorganization was a reorganization.
///
/// The second such move was `@kiln/boot/grub2` gaining the `grub` package.
/// libostree runs `grub-mkconfig` **chrooted into the deployment**, so
/// the grub2 backend needs the binary inside the image; without it there is no
/// `/boot/grub/grub.cfg` regeneration and no automatic rollback. All three
/// fixtures that include a profile moved and neither `order-independence`
/// fixture did, which is what says the cause was the module's content rather
/// than anything about hashing — a semantics change would have moved all five
/// and would have needed an epoch bump instead.
///
/// The third was the same module gaining `efibootmgr`, which is an *optional*
/// dependency of `grub` and so arrives in no image that does not name it. It is
/// what `grub-install` execs to write the UEFI boot entry, and Kiln expects the
/// installer run `grub-install` inside the deployment — so without it the one
/// step that makes a freshly built system bootable fails. The same three
/// fixtures moved, for the same reason.
///
/// The fourth was `@kiln/desktop/gnome` gaining the "normal" desktop tier's
/// apps (`gnome-console`, `gnome-calculator`, `gnome-calendar`) when the
/// desktop modules split into minimal/normal/full — again only `workstation`,
/// the one fixture that includes `@kiln/desktop/gnome`.
const FROZEN: &[(&str, &str)] = &[
    (
        "four-lines",
        "b3:150437794c6339df1f69af4144e844b3052eec6221ab06ba7655c95abb3b5f78",
    ),
    (
        "minimal",
        "b3:c092f3b179abc78122d3e518e1fc2dd51190257b6758222efc41c4ddb01f4f49",
    ),
    (
        "order-independence-a",
        "b3:028805bc2257ffae05310bc2380208ebbed63ce6783b3b8f1db6e7d14e103a79",
    ),
    (
        "order-independence-b",
        "b3:028805bc2257ffae05310bc2380208ebbed63ce6783b3b8f1db6e7d14e103a79",
    ),
    (
        "workstation",
        "b3:fc7795bf3eb35590274d691388ba97327a01ad458fbf3aa303ccc9d4f35c35e1",
    ),
];

/// The epoch the values above were taken at. Changing `HASH_EPOCH` without
/// changing this is the mistake this constant exists to catch.
const FROZEN_AT_EPOCH: u32 = 6;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/kiln-config has a workspace root")
        .to_path_buf()
}

#[test]
fn config_ids_are_frozen() {
    assert_eq!(
        kiln_manifest::HASH_EPOCH,
        FROZEN_AT_EPOCH,
        "\n\nHASH_EPOCH moved from {FROZEN_AT_EPOCH} to {}. That is a deliberate act, so \
         update FROZEN and FROZEN_AT_EPOCH in this file in the same commit, and say in the \
         commit message why every cached identity had to be invalidated.\n",
        kiln_manifest::HASH_EPOCH
    );

    let opts = Options {
        allow_external_sources: false,
        module_root: Some(repo_root().join("modules")),
    };

    let mut drifted = Vec::new();
    for (name, expected) in FROZEN {
        let dir = repo_root().join("tests/corpus/valid").join(name);
        let fe = kiln_config::load(Some(&dir), &opts)
            .unwrap_or_else(|e| panic!("{name} did not load:\n{}", kiln_diag::render_all(&e)));
        let got = fe.manifest.config_id().to_string();
        if got != *expected {
            drifted.push(format!("  {name}\n    was {expected}\n    now {got}"));
        }
    }

    assert!(
        drifted.is_empty(),
        "\n\n{} frozen config_id(s) changed:\n\n{}\n\n\
         Exactly one of these is true:\n\n\
         (a) This was meant to be a refactor. Then it is a bug: something that should not \
         affect identity did. Find it — a reordered field in a `canon()` impl, a changed \
         default, a key that started or stopped being hashed — and fix that, not this file.\n\n\
         (b) The hashing model changed on purpose. Then bump `HASH_EPOCH` in kiln-manifest, \
         update FROZEN and FROZEN_AT_EPOCH here, and do it all in one commit. Every user's \
         build cache is about to miss, and that should be a decision somebody made rather \
         than something that happened.\n\n\
         (c) A shipped module one of these fixtures includes changed what it installs. Then \
         the fixture's *input* changed and its identity was supposed to move — but only for \
         the fixtures that include the module, and the module's diff has to be in this same \
         commit. If every value moved, it is not this; go back to (a).\n",
        drifted.len(),
        drifted.join("\n\n")
    );
}

/// Local file contents are part of the configuration identity, which is
/// the whole reason `local_digests` exists. If this stops being true, editing
/// `files/motd` would silently produce the same image.
#[test]
fn editing_a_local_file_changes_config_id() {
    let src = repo_root().join("tests/corpus/valid/workstation");
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("cfg");
    copy_tree(&src, &dst);

    let opts = Options {
        allow_external_sources: false,
        module_root: Some(repo_root().join("modules")),
    };
    let before = kiln_config::load(Some(&dst), &opts)
        .unwrap()
        .manifest
        .config_id();

    std::fs::write(dst.join("files/motd"), "a different motd\n").unwrap();
    let after = kiln_config::load(Some(&dst), &opts)
        .unwrap()
        .manifest
        .config_id();

    assert_ne!(
        before, after,
        "editing a file referenced by [[file]] did not change config_id"
    );
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}
