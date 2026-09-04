# CLAUDE.md

This file orients Claude Code (claude.ai/code), and any other contributor, working in this
repository. `README.md` and `docs/GUIDE.md` are the user-facing docs — written for someone
*running* Kiln. This file, and the conventions below, are for someone *changing* it.

## What Kiln is

A declarative Linux system image builder: TOML in `/etc/kiln/` → an OSTree commit →
a bootable deployment. It is **a distribution's build tool, not an image-shipping pipeline**.
The loop is: write config on your system → build on your system → deploy on your system →
use your system.

Kiln answers exactly one question: *what is inside the image?* The test for whether something
belongs: **if it changes, do you need a new image and a reboot?** If not, it is out of scope.

## Non-negotiable scope boundaries

These are closed decisions. Do not reintroduce them, and push back if asked to add them
casually:

- **No login accounts / users / dotfiles / desktop settings.** Image content only.
- **No live-apply.** One image, one reboot. No `/etc`-only fast path.
- **No container image export**, no remotes, no `push`/`pull`, no commit signing, no fleet
  management or templating one config into many images.
- **No installation.** There is no `kiln install`, no ISO, no partitioning. Kiln exposes
  `--sysroot` and `kiln sysroot init` so a separate installer can be written against it.
- **No implicit base.** An empty config produces an empty image; `@kiln/profiles/minimal` is
  the one-line ergonomic answer.
- **TOML is data, not a language.** No variables, interpolation, conditionals, functions, or
  inheritance keywords. `include` is the only composition operator. Resisting "NixOS but with
  TOML" is a primary design goal.

## Architecture: six stages, in order

```
frontend      discovery → parse → include graph → merge → validate → Manifest   ⇒ config_id
resolution    alpm syncdb · AUR RPC · git ls-remote · local hashing → BuildPlan  ⇒ plan_id
              (plan_id == deployed?  stop, unless --force)
realization   fetch packages · build PKGBUILDs in sandboxes → artifact store
assembly      alpm transaction into staging root · overlay files · unit state
normalization usr-merge · /etc→/usr/etc · /var drain · depmod · initramfs
commit&deploy ostree commit (metadata: plan_id, generation, build record) · deploy
```

The **plan/realize split** is the load-bearing idea: resolution is cheap, networked, and
metadata-only; realization is expensive and sandboxed. That split is what makes `kiln check`
possible without building, what lets `kiln build` refuse a no-op, and what keeps the two
halves separable. Do not let realization work leak upward into resolution.

### Three identities

- `config_id` — blake3 of the canonical `Manifest`, including digests of local files.
- `plan_id` — `config_id` + all resolved external inputs. **This is the build identity**;
  change detection compares it against the booted deployment's recorded `plan_id`. It
  includes a deliberately-bumped `hash_epoch`, *not* Kiln's version number, so a point
  release does not force a global rebuild.
- The OSTree commit checksum.

**Volatile inputs** (VCS `pkgver()`, `SKIP` checksums) cannot be resolved without fetching.
They are excluded from `plan_id`, reported separately, and resolvable with `--deep`. Never
guess them — an untrustworthy `kiln check` is worse than no `kiln check`.

### There is no lockfile

Resolution is never persisted to the config directory and never goes in git. Every commit
carries its own build record in commit metadata (`kiln.record`, zstd JSON) and at
`/usr/lib/kiln/record.json`. OSTree is already a versioned content-addressed store; a
parallel lockfile would be a second source of truth that can disagree with the first. The
record is internal machinery for update checking, `kiln diff`, and `kiln rebuild` — not a
user-facing file.

## The Arch→OSTree contract

Arch is not an OSTree distribution, and most of what could go wrong here lives in this
section — read it before writing image code. The shape:

- `/var` must not exist in the commit — drain it into `tmpfiles.d` + `/usr/share/factory`.
  Symlinks need tmpfiles `L` lines, not a factory copy. Logs and caches are excluded.
- `/etc` becomes `/usr/etc`; the live `/etc` is 3-way merged at deploy time. `pacman.conf`
  and `machine-id` need explicit handling before the move.
- usr-merge top-level symlinks, done *after* the transaction (the `filesystem` package owns
  those directories). Plus `/ostree → sysroot/ostree`, without which libostree cannot read
  its own sysroot from inside the booted image.
- pacman DB at `/usr/lib/sysimage/pacman` — and `/var/lib/pacman` explicitly dropped, since
  the `pacman` package owns it.
- Kernel at `/usr/lib/modules/$kver/{vmlinuz,initramfs.img}`; dracut with the `50ostree`
  module, verified with `lsinitrd` rather than trusted.
- UID/GID pinned via `sysusers.d`, seeded between two alpm transactions.
- Package-shipped alpm hooks always run and can only be *shadowed* by filename.
- Determinism needs `%INSTALLDATE%` pinned and `machine-id` truncated.

**Bootloader: GRUB2**, through libostree's own `sysroot.bootloader=grub2` backend, with
`/boot` on ext4 and the ESP at `/boot/efi`. This is Fedora Silverblue's arrangement and the
best-tested path libostree has. It is not systemd-boot: libostree keeps `/boot/loader` as a
symlink pair for atomic entry swaps, vfat has no symlinks, and UEFI firmware reads only FAT —
so `/boot` cannot be the ESP and systemd-boot cannot read the ext4 `/boot` libostree needs.
libostree has no systemd-boot backend at all. `boot.loader` defaults to `"grub2"` and takes
no other value — writing `systemd-boot` gets a diagnostic explaining the conflict.

Automatic rollback on boot failure is counted by GRUB's own `grubenv`, not BLS boot counting
— BLS counting is decremented by the *bootloader*, and the GRUB2 backend does not implement
it. Read `kiln-image/src/bootcount.rs` and `kiln-ostree/src/grubenv.rs` before touching
anything near the bootloader: `grub-mkconfig` runs *chrooted into the deployment* with a
*host-absolute* output path, so the grub2 backend cannot run against a sysroot that is not
`/` — that constraint shapes how `kiln apply`/`kiln deploy` are structured.

## Crate layout (workspace)

Split along *testability* boundaries — the top crates are pure and snapshot-testable, the
bottom ones need root and run only in privileged CI containers.

- `kiln-diag` — `SourceFile`/`Origin`/`Spanned`/`Provenance`, the error taxonomy, exit codes,
  and deterministic miette rendering. Its own crate because error quality is a feature, and a
  feature with no home crate decays. `render()` is nocolor and fixed-width so *rendered*
  diagnostics can be snapshotted.
- `kiln-config` — discovery, `toml_edit` parsing into a spanned generic `Node` tree, shorthand
  expansion, structure checks, the include graph, the merge algebra, and validation into a
  `Manifest`. Merge operates on the generic tree, not on typed structs, so it stays
  property-testable and every key keeps its provenance.
- `kiln-manifest` — the canonical IR, its hand-written canonical encoding, and `config_id`.
  No dependency on `kiln-config`.
- `kiln-cli` — bin `kiln`. Hand-written argument parsing; the surface is small and fixed.
- `kiln-alpm` — libalpm: solver + transaction, including `.pkg.tar.zst` files loaded from
  disk, and the `owns`/`installed_package` queries behind `kiln owns` and `kiln why`.
- `kiln-resolve` — `BuildPlan`, `plan_id`.
- `kiln-sandbox` — `Sandbox` trait; bwrap + nspawn.
- `kiln-image` — all eleven assembly steps, normalization, and `bootcount`.
- `kiln-build` — recipes, `build_key`, the build cache, the two-phase build, the build root,
  synthesized module recipes.
- `kiln-aur` — RPC, commit identity, the dependency closure, the clone.
- `kiln-record` — the build record.
- `kiln-ostree` — commit, deploy, generations, rollback, `grubenv`, the `Removal` policy
  `kiln rm`/`kiln clean` are written against, and `drift` — the `/usr/etc` vs `/etc` walk
  behind `/etc` drift detection.
- `kiln-state` — planned, not yet built.

Also: `modules/` (the shipped TOML module library → `/usr/share/kiln/modules`; a fixed set of
files across a dozen namespaces — one decision per file, a 25-line cap, only profiles
compose, and every unit a module names must come from a package that module installs — all
four CI-enforced by `crates/kiln-config/tests/modules.rs`, the last of them against the
host's pacman file database, skipping with a message where there is none),
`tests/corpus/` (valid and invalid configs), `tests/repo-fixture/` (a real tiny pacman repo
built in-tree), and `tests/vm/` (the boot acceptance fixture). Packaging lives in its own
repository, not here.

Installation is not this project's job. `--sysroot` and `kiln sysroot init` are the only
surface Kiln exposes for building into a target that is not the running root; nothing beyond
that — no install verb, no ISO, no partitioning, no bootloader install — belongs in this
workspace. A separate program can be written against that seam, but it is not part of Kiln
and Kiln's docs do not describe it.

`README.md` and `docs/GUIDE.md` are the *user*-facing docs. A change to the CLI surface, the
schema, or a command's output is a change to the guide in the same commit — it quotes real
output in a dozen places, and a guide that quotes output the binary no longer produces is
worse than none.

## Implementation conventions that carry design weight

- **Order never matters.** Collections are `BTreeMap`/`BTreeSet` and canonically sorted before
  hashing. Reordering lines in a TOML file must never change `config_id`. `files` is keyed by
  target path (collisions become impossible); `scripts` is keyed by name (ordering is
  content-determined, not file-order-determined).
- **Ambiguity is an error, not a coin flip.** Two included files setting the same scalar to
  different values is a hard error naming both files and lines — never last-wins. The includer
  wins over what it includes; siblings conflict.
- serde with `deny_unknown_fields`; spans carried through the whole frontend so
  `kiln explain kernel.cmdline` can answer "set in `hardware.toml:14`, overriding
  `@kiln/hardware/nvidia:9`".
- **The config root is a security boundary.** Every `source`/`path` must resolve, after symlink
  resolution, inside it; escaping requires `--allow-external-sources`.
- **Network is on only during resolution and source fetching** — never during a build phase or
  a build script. That constraint is what makes script output a pure function of hashed inputs.
  Realization is where a build's network lives, and it all happens before assembly starts:
  the AUR clone, `makepkg --verifysource`, the build root's own transaction, and the package
  download. Assembly's `Options.artifacts` is the promise that everything is already here.
- **The build never touches the live root.** Everything happens in a staging root under
  `/var/lib/kiln`.
- **The user should never have to type `ostree`.** Users write `target = "/etc/motd"`; Kiln owns
  the `/usr/etc` translation, the `/var` drain, kernel placement, and BLS entries.
- Exit codes are meaningful: `0` ok, `1` config error, `2` resolution failure, `3` build
  failure, `4` system/permission, `10` `kiln check` found changes.

## Frontend implementation notes

Things that are not obvious from reading the crates:

- **Merge runs on the generic spanned tree, not on typed structs.** Typed extraction happens
  exactly once, afterwards, in `validate`. This keeps the merge algebra property-testable and
  is why provenance survives.
- **Provenance is computed by walking the include tree, not threaded through merge.** The
  shallowest file that sets a key wins and disagreeing siblings are already an error, so the
  tree alone determines the answer. `Provenance.is_list` matters: a list has contributors, not
  a winner, and `kiln explain` must not call that "overriding".
- **Cycle detection must precede include deduplication.** Otherwise a cycle reached through
  an already-included file is silently deduplicated into a no-op. This was a real bug the
  corpus caught.
- **Scalar type errors belong to the structure phase**, not the semantic one, so a file with
  three type errors reports all three in one run.
- **The canonical encoding is hand-written on purpose** (`kiln-manifest/src/canon.rs`). The
  byte stream *is* the hash input, so stability must not depend on a dependency's formatting.
- `kiln-diag::SourceFile` carries a `miette::NamedSource` so rendered diagnostics show
  `╭─[desktop.toml:4:11]`. Without it a multi-file conflict names no files.

## Testing model

- `insta` snapshot tests over `tests/corpus/` (valid and invalid configs), including invalid
  configs whose *rendered diagnostics* are snapshotted. When you change a diagnostic, read the
  snapshot diff as a user would — that is the review, not a formality.
- `proptest` for the merge algebra: sibling order does not change `config_id` *or* validity,
  list order within a file does not change `config_id`, union is associative.
- **Boot acceptance** (`tests/vm/`, driven by `crates/kiln-cli/tests/boot.rs`) is the only
  test that proves the project works, and the only one that uses the network: gen 1 boots,
  gen 2 boots, `kiln rollback` boots back to gen 1, all asserted from inside a qemu VM by a
  probe the configuration itself ships. `sudo -E cargo test -p kiln-cli --test boot --
  --ignored --nocapture`, about twenty minutes. Read `tests/vm/README.md` before touching it —
  particularly the part about the host and the guest never holding the disk, or a cached view
  of it, at the same time.
- **Build scripts run against a real overlayfs, with a fake sandbox**
  (`crates/kiln-image/tests/scripts.rs`). The overlay is real — whiteouts, opaque
  directories and copy-up all come from the kernel — and the *sandbox* is a closure that
  writes into the merged mount. Standing a shell up inside a staging root to reach the same
  overlay would be testing bubblewrap, which `kiln-sandbox`'s live tests already do.
- **Hash-freeze tests** (`crates/kiln-config/tests/hash_freeze.rs`): committed expected
  `config_id`s. A refactor must not change them; a deliberate change requires bumping
  `HASH_EPOCH` **and** `FROZEN_AT_EPOCH` in the same commit. Never "fix" a failure by pasting
  the new values — the test's own failure message explains the two legitimate causes.
- Solver/transaction tests use `tests/repo-fixture` (a real tiny local pacman repo built
  in-tree) — never the network. AUR uses recorded HTTP fixtures. The fixture ships a package
  genuinely called `base-devel`, so `BuildRoot::assemble` is testable offline
  (`crates/kiln-build/tests/root.rs`); what it cannot test is a `makepkg` run, because a
  four-package fixture has no toolchain — that is the boot test's job.
- Sandbox tests assert on the exact `SandboxSpec` (e.g. that the build phase really has
  `Network::Disabled`).

## Local environment and commands

`cargo` 1.98, `pacman`/`pacstrap`, `bwrap`, `makepkg`, `systemd-nspawn`, `qemu-system-x86_64`,
`edk2-ovmf`, `ostree` 2026.4, `dracut` 111, `grub` 2.14 and `gptfdisk` are all present, KVM
works, and sudo is passwordless. `gcc` is needed too — `tests/repo-fixture` builds one
static helper binary with it.

```
cargo test                          # none privileged
sudo -E cargo test -- --ignored     # transactions, assembly, scripts, ostree: need root
./tests/repo-fixture/build.sh       # the hermetic pacman repo (tests call it themselves)
cargo test -p kiln-config --test corpus
cargo clippy --all-targets          # currently zero warnings; keep it that way
cargo fmt
cargo insta review                  # after a deliberate diagnostic change

cargo run --bin kiln -- --config <dir> --module-root ./modules check --offline
cargo run --bin kiln -- --config <dir> --module-root ./modules explain boot.timeout

sudo -E cargo test -p kiln-cli --test boot -- --ignored --nocapture   # boot acceptance, ~20 min
```

`--module-root ./modules` is needed until the library is installed to
`/usr/share/kiln/modules`; `KILN_MODULE_DIR` and `KILN_CONFIG_DIR` do the same job.

Privileged tests (assembly, normalization, build scripts, build roots, OSTree) are
`#[ignore]`d by default with a reason string, and skip with a message rather than failing
when run without root. Anything new that needs root goes the same way.
