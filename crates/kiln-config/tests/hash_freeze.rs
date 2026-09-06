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

/// Frozen at hash epoch 4 — `kernel.dracut_modules` joined `Kernel`'s
/// canonical encoding. dracut's default, non-hostonly module selection does
/// not include every module whose package is installed, so which dracut
/// modules a build requests is genuinely part of the image; that moved all
/// five fixtures together, which is what says it was a schema change rather
/// than a module content change (see cause (c) below).
/// Do not "fix" these by pasting new values.
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
        "b3:a33a09bf16a2108e5a57ec0f527101162a4da6cfc530b19292237a3cffa88fb6",
    ),
    (
        "minimal",
        "b3:7aef83aed85a842a86412e1a039f4d9725d43597dfed99ca5fca4133c34a364d",
    ),
    (
        "order-independence-a",
        "b3:dce901213dac0e3dd9eb9af128da296fa3658c48bd65c6b2ca2ba48e841c1f82",
    ),
    (
        "order-independence-b",
        "b3:dce901213dac0e3dd9eb9af128da296fa3658c48bd65c6b2ca2ba48e841c1f82",
    ),
    (
        "workstation",
        "b3:16f6363f65a3425f29b53c2c41606a8267bdd8bd83f9ba52c7cd5af58a9c389b",
    ),
];

/// The epoch the values above were taken at. Changing `HASH_EPOCH` without
/// changing this is the mistake this constant exists to catch.
const FROZEN_AT_EPOCH: u32 = 4;

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
