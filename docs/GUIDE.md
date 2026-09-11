# 📖 The Kiln Guide

Kiln compiles a TOML description of a system into an OSTree commit and boots it. This is the
complete guide: what Kiln is, how to run it, every configuration key, every command, how the
implementation is put together, and what to do when something goes wrong.

Read this if you are **running** Kiln or **changing** it. [`CLAUDE.md`](../CLAUDE.md) is the
short companion for contributors: the design boundaries and the conventions the codebase
relies on.

---

## Contents

1. [Introduction](#1-introduction)
2. [Core concepts](#2-core-concepts)
3. [Architecture](#3-architecture)
4. [Installation](#4-installation)
5. [CLI reference](#5-cli-reference)
6. [The configuration system](#6-the-configuration-system)
7. [Packages](#7-packages)
8. [Files and the filesystem](#8-files-and-the-filesystem)
9. [systemd](#9-systemd)
10. [Kernels and kernel modules](#10-kernels-and-kernel-modules)
11. [OSTree and deployments](#11-ostree-and-deployments)
12. [Generations](#12-generations)
13. [The build pipeline](#13-the-build-pipeline)
14. [Includes, composition, and the module library](#14-includes-composition-and-the-module-library)
15. [Errors and troubleshooting](#15-errors-and-troubleshooting)
16. [Development](#16-development)
17. [Crate reference](#17-crate-reference)
18. [Extending Kiln](#18-extending-kiln)
19. [Examples](#19-examples)
20. [FAQ](#20-faq)
21. [Contributing](#21-contributing)
22. [Issues and bug reports](#22-issues-and-bug-reports)
23. [AI assistance](#23-ai-assistance)
24. [License](#24-license)

---

## 1. Introduction

### 1.1 What Kiln is

Kiln is a **declarative system image builder for Arch Linux**. You write TOML describing what
should be inside your operating system image. Kiln resolves that description against real
pacman repositories, builds anything that needs building, assembles a filesystem tree, commits
it to an [OSTree](https://ostreedev.github.io/ostree/) repository, and stages that commit for
your next boot.

The result is an immutable, atomically updated Arch system: `/usr` is read-only, every build
is a numbered generation you can boot back into, and nothing you did by hand on the running
machine is part of the definition.

### 1.2 The problem it solves

An ordinary Arch install accumulates history. Packages installed once and forgotten, config
files edited at 2am, a driver built by hand two kernels ago. Nothing records *why* the system
is what it is, and there is no way back from a bad upgrade except a backup.

Kiln makes the system a function of a file you can read, diff and keep in git. A broken
upgrade is a reboot away from being undone, because the previous image is still on disk, byte
for byte.

### 1.3 The philosophy

Kiln answers exactly one question: **what is inside the image?**

The test for whether something belongs in Kiln: *if it changes, do you need a new image and a
reboot?* If not, it is out of scope. That single rule produces the whole scope boundary:

| Deliberately absent | Reason |
| --- | --- |
| Login accounts, dotfiles, desktop settings | Those change without a reboot. `/var` and `/home` are yours |
| Live apply | One image, one reboot. A system that is sometimes the image and sometimes something else cannot answer the question Kiln exists to answer |
| Container export, remotes, `push`/`pull`, fleet management | Kiln is a distribution's build tool, not an image-shipping pipeline |
| Installation, ISOs, partitioning | Kiln exposes `--sysroot` and `kiln sysroot init` so a separate installer can be written against it |
| An implicit base image | An empty configuration produces an empty image. `@kiln/profiles/minimal` is the one-line ergonomic answer |
| Variables, interpolation, conditionals, functions, inheritance | TOML here is data. `include` is the only composition operator |

That last row matters more than it looks. Resisting "NixOS but with TOML" is a primary design
goal. Kiln has no language, and it is not going to grow one.

### 1.4 Where Kiln sits in a Linux system

```text
        your TOML                    Kiln                     the machine
   ┌────────────────┐        ┌──────────────────┐        ┌──────────────────┐
   │  /etc/kiln/    │───────▶│  resolve · build │───────▶│  OSTree commit   │
   │  system.toml   │        │  assemble·commit │        │  deployment      │
   └────────────────┘        └──────────────────┘        │  GRUB2 entry     │
                                      │                  └──────────────────┘
                              pacman · AUR · your                   │
                              PKGBUILDs · dracut                 reboot
```

Kiln owns the image. It does not own the machine. `/var` (which holds `/home`, `/root`,
`/opt` and `/srv` through symlinks) persists across every generation and is never rolled back.

### 1.5 Who should use it

- You run Arch and want atomic, reversible system updates without leaving the Arch ecosystem.
- You maintain more than one machine and want them described by files rather than by memory.
- You are building a distribution on top of Arch and need a reproducible image build.

You should probably not use Kiln if you want to configure users, dotfiles or desktop settings
declaratively. Those are not image content, and Kiln will never manage them.

### 1.6 Terminology

| Term | Meaning |
| --- | --- |
| **Config root** | The directory holding the entry point (`/etc/kiln` by default). A security boundary: every referenced file must resolve inside it |
| **Entry point** | `system.toml` inside the config root, or whatever `--config` names |
| **Module** | A small shipped TOML file under `/usr/share/kiln/modules`, referenced as `@kiln/<namespace>/<name>` |
| **Manifest** | The merged, validated, canonical description of one image. Hashes to `config_id` |
| **BuildPlan** | The manifest plus every resolved external input. Hashes to `plan_id` |
| **Generation** | A monotonic number assigned to each commit. Stable forever, unlike an OSTree deployment index |
| **Deployment** | A checked-out generation, bootable, with a GRUB entry |
| **Baseline** | Generation 1. Protected from removal, because automatic rollback needs a known-good floor |
| **Record** | The build record: what a generation actually resolved to. Lives in the commit, not on disk |
| **Volatile input** | An input whose value cannot be known without fetching (a VCS `pkgver()`, a `SKIP` checksum) |
| **Staging root** | The tree being assembled under `/var/lib/kiln/build/<plan_id>/root` |

---

## 2. Core concepts

### 2.1 The loop

```text
edit /etc/kiln  →  kiln check  →  kiln apply  →  reboot
                        ↑                          │
                        └──────  kiln rollback  ────┘
```

Four things are worth internalizing before anything else.

**There is no live apply.** Every change is a new image and a reboot. Kiln has no fast path
that edits `/etc` in place, and will not grow one.

**Building is not deploying.** `kiln build` produces a commit and stops. `kiln apply` builds
*and* stages the result for the next boot. If you are unsure which you want, you want `apply`.

**Nothing is thrown away.** Every build is a numbered generation, kept until `kiln clean`
takes it. Rolling back reorders a list; it does not restore a backup.

**Kiln owns the image, not the machine.** `/var` persists across every generation: your home
directory, your databases, your container images, your logs. Rolling back restores the
*image* and not your application state. That is the single biggest surprise in systems of this
shape.

### 2.2 Configuration

Your configuration is one or more TOML files rooted at `/etc/kiln/system.toml`. Every file
starts with `kiln = 1` and may `include` others. Nothing is glob-loaded: every file that
participates is reachable through an explicit `include` chain, so the chain *is* the
documentation of what your system is made of.

See [section 6](#6-the-configuration-system) for the whole schema.

### 2.3 The three identities

Every image carries three identities, and they answer different questions.

| Identity | What it covers | What it answers |
| --- | --- | --- |
| `config_id` | blake3 of the canonical `Manifest`, including digests of every local file it references | Did the configuration change? |
| `plan_id` | `config_id` plus every resolved external input (package versions, AUR commits, build keys) | Would a build produce something different from what is deployed? |
| commit checksum | The OSTree commit | Which exact tree is this? |

`plan_id` is **the build identity**. `kiln check` compares it against the deployed
generation's recorded `plan_id`, and `kiln build` refuses a no-op when they match.

`config_id` includes a deliberately-bumped `hash_epoch` constant, *not* Kiln's version number,
so a point release does not force a global rebuild.

### 2.4 Volatile inputs

Some inputs genuinely cannot be resolved without fetching bytes: a VCS package whose
`pkgver()` runs upstream code, or a `source=()` entry with a `SKIP` checksum. Kiln does not
guess. Such inputs are **excluded from `plan_id`**, reported separately, and resolvable with
`kiln check --deep`, which runs `makepkg --verifysource` and nothing else.

An untrustworthy `kiln check` is worse than no `kiln check`.

### 2.5 There is no lockfile

Resolution is never persisted into the config directory and never goes into git. Every commit
carries its own **build record** in commit metadata (`kiln.record`, zstd-compressed JSON) and
at `/usr/lib/kiln/record.json` inside the image.

OSTree is already a versioned content-addressed store. A parallel lockfile would be a second
source of truth that can disagree with the first. The record is internal machinery for update
checking, `kiln diff` and `kiln rebuild`; it is not a file you edit.

### 2.6 Generations and rollback

Every build is a numbered generation, assigned at commit time. The number means the same thing
forever, unlike an OSTree deployment index which renumbers as deployments come and go. Every
Kiln command that names an image names a generation.

A generation staged by `kiln apply` boots **on probation**. GRUB counts attempts in its own
`grubenv`, a unit in the image clears the counter once the system is up, and a generation that
fails to come up three times is demoted automatically. See [section 12](#12-generations).

### 2.7 The plan/realize split

Resolution is cheap, networked and metadata-only: it refreshes sync databases, asks the AUR
RPC for commits, hashes local files, and produces a plan. It downloads no package and unpacks
nothing.

Realization is expensive and sandboxed: it fetches packages, clones AUR repositories, and
compiles things in bubblewrap with the network turned off.

That boundary is what makes `kiln check` possible without building, what lets `kiln build`
refuse a no-op, and what keeps the two halves separable.

---

## 3. Architecture

### 3.1 The six stages

```text
  ┌──────────────┐
  │   frontend   │  discovery → parse → include graph → merge → validate
  └──────┬───────┘  ⇒ Manifest ⇒ config_id
         ▼
  ┌──────────────┐
  │  resolution  │  alpm syncdb · AUR RPC · local hashing · build keys
  └──────┬───────┘  ⇒ BuildPlan ⇒ plan_id     (deployed already? stop)
         ▼
  ┌──────────────┐
  │ realization  │  AUR clone · makepkg in sandboxes · package download
  └──────┬───────┘  ⇒ artifact store          (the only network in a build)
         ▼
  ┌──────────────┐
  │   assembly   │  alpm transaction · UID seed · [[file]] · unit state · scripts
  └──────┬───────┘
         ▼
  ┌──────────────┐
  │normalization │  /etc→/usr/etc · /var drain · usr-merge · depmod · initramfs
  └──────┬───────┘
         ▼
  ┌──────────────┐
  │commit&deploy │  ostree commit (metadata: plan_id, generation, record) · deploy
  └──────────────┘
```

### 3.2 Frontend data flow

```text
system.toml ──parse──► Node (spanned)
     │                   │
  include            structure checks
     ▼                   ▼
  Unit tree ──merge──► Node + OriginMap ──validate──► Manifest ⇒ config_id
```

Four implementation facts that are not obvious from reading the crates:

- **Merge runs on the generic spanned tree, not on typed structs.** Typed extraction happens
  exactly once, afterwards, in `validate`. This keeps the merge algebra property-testable and
  is why provenance survives.
- **Provenance is computed by walking the include tree, not threaded through merge.** The
  shallowest file that sets a key wins, and disagreeing siblings are already an error, so the
  tree alone determines the answer.
- **Cycle detection precedes include deduplication.** Otherwise a cycle reached through an
  already-included file is silently deduplicated into a no-op. This was a real bug the test
  corpus caught.
- **Scalar type errors belong to the structure phase**, not the semantic one, so a file with
  three type errors reports all three in one run.

### 3.3 Diagnostics

Every value carries the file and byte range it came from, all the way through. `kiln-diag`
owns `SourceFile`/`Origin`/`Spanned`/`Provenance`, the error taxonomy, exit codes, and
deterministic [miette](https://docs.rs/miette) rendering.

Phases are named: `discovery`, `syntax`, `structure`, `graph`, `merge`, `semantic`,
`resolution`, `assembly`. Every phase reports **all** of its problems before the next one
starts, so three type errors in a file come out in one run rather than three.

### 3.4 The Arch → OSTree contract

Arch is not an OSTree distribution. Making it one is where most of the difficulty lives.
`kiln-image` implements the contract:

| Requirement | How Kiln satisfies it |
| --- | --- |
| `/var` must not exist in the commit | Drained into `tmpfiles.d` lines plus `/usr/share/factory` copies. Symlinks get `L` lines, not factory copies. Logs and caches are excluded |
| `/etc` becomes `/usr/etc` | Moved wholesale after the transaction; the live `/etc` is 3-way merged at deploy. `pacman.conf` and `machine-id` are fixed first, while still reachable |
| usr-merge top-level symlinks | Laid down *after* the transaction, because the `filesystem` package owns those directories as real ones. Plus `/ostree → sysroot/ostree`, without which libostree cannot read its own sysroot from inside the booted image |
| pacman database location | `/usr/lib/sysimage/pacman`, with `/var/lib/pacman` explicitly dropped |
| Kernel placement | `/usr/lib/modules/$kver/{vmlinuz,initramfs.img}` |
| Initramfs | dracut with the upstream `50ostree` module, verified with `lsinitrd` rather than trusted |
| Stable service-account IDs | Pinned via `sysusers.d`, seeded between two alpm transactions |
| Package alpm hooks | They always run; the only lever is same-filename shadowing, so Kiln shadows the ones that write runtime state and keeps the rest |
| Determinism | `%INSTALLDATE%` pinned, `machine-id` truncated, `SOURCE_DATE_EPOCH=0` in sandboxes |

**Bootloader: GRUB2**, through libostree's own `sysroot.bootloader=grub2` backend, with
`/boot` on ext4 and the ESP at `/boot/efi`. This is Fedora Silverblue's arrangement and the
best-tested path libostree has. systemd-boot is not an option: libostree keeps `/boot/loader`
as a symlink pair for atomic entry swaps, vfat has no symlinks, and UEFI firmware reads only
FAT, so `/boot` cannot be the ESP. libostree has no systemd-boot backend at all.

Automatic rollback is counted by GRUB's own `grubenv`, not by Boot Loader Specification boot
counting, because BLS counting is decremented by the *bootloader* and the GRUB2 backend does
not implement it.

### 3.5 Crate map

```text
kiln-cli ──┬──► kiln-config ──► kiln-manifest ──► kiln-diag
           │                          ▲
           ├──► kiln-resolve ─────────┤──► kiln-alpm
           │        │                 │
           │        ├──► kiln-aur     │
           │        └──► kiln-build ──┴──► kiln-sandbox
           │
           ├──► kiln-image ──► kiln-record
           └──► kiln-ostree ──┘
```

Full descriptions are in [section 17](#17-crate-reference).

---

## 4. Installation

### 4.1 Requirements

Kiln runs on Arch or an Arch-derived system. Building and deploying an image needs root.

**Runtime dependencies** (the package declares them):

| Dependency | Used for |
| --- | --- |
| `pacman` | libalpm: the solver and the transaction |
| `bubblewrap` | The build sandbox |
| `ostree` | The commit, the deployment, and the `50ostree` dracut module |
| `dracut` | The initramfs |
| `grub` | `grub-mkconfig` inside the deployment, and boot counting |
| `git` | Cloning AUR package bases |

**Build dependencies:** `cargo` (Rust 1.85 or newer), `git`, `glib2`, `pkgconf`. Kiln links
libostree through its GObject-introspection bindings, so `glib2` development files must be
present.

**For the test suite:** `pacstrap`, `systemd-nspawn`, `qemu-system-x86_64`, `edk2-ovmf`,
`gptfdisk`, `gcc`, and a working KVM.

### 4.2 Installing the package

The Arch package is `terracotta-kiln`. Its `PKGBUILD` lives in `packaging/` and is standalone:
it clones the tag `v$pkgver` from GitHub rather than assuming it sits inside a checkout.

```console
$ git clone https://github.com/Terracotta-Linux/kiln.git
$ cd kiln/packaging
$ makepkg -si
```

That installs:

| Path | Contents |
| --- | --- |
| `/usr/bin/kiln` | The binary |
| `/usr/share/kiln/modules/` | The shipped module library |
| `/usr/share/doc/terracotta-kiln/` | `README.md` and `GUIDE.md` |
| `/usr/share/bash-completion/completions/kiln` | bash completion |
| `/usr/share/zsh/site-functions/_kiln` | zsh completion |
| `/usr/share/fish/vendor_completions.d/kiln.fish` | fish completion |

Prebuilt packages are attached to every
[GitHub release](https://github.com/Terracotta-Linux/kiln/releases) as
`terracotta-kiln-x86_64.pkg.tar.zst` with a `.sha256` beside them. To install Kiln *into* an
image you are building, include `@kiln/terracotta/kiln`, which names exactly that release URL
and its checksum file.

### 4.3 Building from source

```console
$ cargo build --release          # target/release/kiln
$ cargo test                     # nothing privileged
$ sudo -E cargo test -- --ignored
```

Without an installed module library, point at the tree:

```console
$ cargo run --bin kiln -- --config ./myconfig --module-root ./modules check --offline
```

`KILN_MODULE_DIR` and `KILN_CONFIG_DIR` do the same job as `--module-root` and `--config`.

### 4.4 Directories Kiln uses

| Path | Purpose |
| --- | --- |
| `/etc/kiln/` | Your configuration. `system.toml` is the entry point |
| `/usr/share/kiln/modules/` | The shipped module library |
| `/var/lib/kiln/` | All state: the artifact cache, staging roots, the pacman keyring, syncdb cache. Everything here is cache or history, so deleting it costs time and never correctness |
| `/var/lib/kiln/build/<plan_id>/` | One build's staging root and work directory |
| `/var/lib/kiln/cache/pkg/` | The package artifact cache |
| `/ostree/repo` | The OSTree repository (under `--sysroot` if given) |
| `/ostree/deploy/kiln/` | The stateroot every deployment lives under |
| `/usr/lib/kiln/` | Inside the image: `manifest.json`, `record.json`, `boot-success` |

### 4.5 Permissions

- `kiln check`, `explain`, `show`, `list`, `status`, `diff`, `why`, `owns` run as an ordinary
  user.
- `kiln build`, `apply` and `rebuild` **require root**. As an ordinary user libalpm extracts
  archives, fails every `chown`, logs a warning, and reports success, producing a tree whose
  ownership, setuid bits and capabilities are all wrong. Kiln refuses rather than allow that.
- `kiln deploy`, `rollback`, `pin`, `rm`, `clean` and `sysroot init` need write access to the
  sysroot, which in practice means root.

### 4.6 Verifying the installation

```console
$ kiln --version
kiln 0.1.13 (schema 1, hash epoch 7)

$ kiln help
$ sudo kiln init            # scaffolds /etc/kiln/system.toml
$ kiln check --offline      # validates the configuration, no network
```

`kiln check --offline` exercising cleanly means discovery, parsing, the include graph, the
merge algebra and every semantic rule are all working against your configuration.

### 4.7 Getting an actual Kiln machine

Kiln has no installer. To put a Kiln-built system on hardware:

- **[terracotta-installer](https://github.com/Terracotta-Linux/terracotta-installer)**
  partitions, formats and builds the image onto the disk.
- **[terracotta-iso](https://github.com/Terracotta-Linux/terracotta-iso)** is the live ISO
  that boots the installer.

To build into a mounted target yourself, see
[building into an unmounted target](#116-building-into-an-unmounted-target).

---

## 5. CLI reference

```text
kiln [global flags] <command> [arguments] [command flags]
```

Arguments are parsed by hand; the surface is small and fixed. `--help`/`-h` and
`--version`/`-V` are recognized at any position, not only as the first word.

### 5.1 Global flags

| Flag | Meaning |
| --- | --- |
| `--config <path>`, `-c <path>` | Entry point, or a directory containing `system.toml`. Default: `$KILN_CONFIG_DIR`, then `/etc/kiln` |
| `--sysroot <path>` | Operate on another root instead of `/`. The installer seam |
| `--module-root <path>` | Override `/usr/share/kiln/modules`. Also `$KILN_MODULE_DIR` |
| `--allow-external-sources` | Permit `source`/`path` values that resolve outside the config root. Warns once per path |
| `-v`, `--verbose` | More detail: OSTree checksums, full records, every drifted file, per-file digests |
| `-V`, `--version` | Print `kiln <version> (schema <n>, hash epoch <n>)` and exit |
| `-h`, `--help` | Print the command summary and exit |

### 5.2 Building

#### `kiln check [--offline] [--deep]`

Resolves the configuration and reports what a build would change, without building anything.

| Flag | Effect |
| --- | --- |
| `--offline` | Validate the configuration only, resolving against cached metadata and refreshing nothing. The fastest way to check that TOML is correct |
| `--deep` | Additionally fetch and resolve volatile inputs (VCS `pkgver()`, `SKIP` checksums) by running `makepkg --verifysource`. Builds nothing |

`--deep` and `--offline` together are refused: they ask for opposite things.

**Exit code is the answer**: `0` when nothing would change, `10` when something would. That
is what makes `kiln check && echo current` work in a timer or a shell prompt.

```console
$ kiln check
workstation  x86_64  config b3:9f2c11a4
  packages     41 repo, 2 aur, 1 build
  kernel       linux, 3 cmdline, 2 modules
  systemd      4 enabled, 1 masked
  content      3 files, 1 script, 5 hashed inputs
  repos        rolling
  sources      6

Update available.  gen 43 → pending  (plan b3:11c4de8a… → b3:99887766…)

  repo packages          4 changed
    linux                  6.19.2-1     →  6.19.3-1
    mesa                   26.1.4-1     →  26.1.5-1
    ripgrep                —            →  14.1.1-1
    vim                    9.1-1        →  removed
  aur                    1 changed
    zen-browser-bin        1.16.3       →  1.17.0         (commit 3f1a9c → 88bd02)
  built packages         1 rebuild
    v4l2loopback                                          (kernel 6.19.2-1 → 6.19.3-1)
  files                  1 changed
    files/myapp.conf       b3:aaaa1111  →  b3:bbbb2222

Build it with:  kiln apply
```

The report categories match the input taxonomy exactly, so nothing can change invisibly:
`repo packages`, `aur`, `built packages`, `local packages`, `files`, `scripts`,
`service accounts`, and a `configuration` fallback that fires only when `config_id` moved and
no other category explains it.

#### `kiln build [--force] [--offline] [--keep-failed]`

Builds an image and commits it. Does **not** deploy.

| Flag | Effect |
| --- | --- |
| `--force` | Build even when the plan already matches what is deployed |
| `--offline` | Resolve against cached metadata; realization still needs the network unless everything is cached |
| `--keep-failed` | Keep the staging root of a failed build for inspection. Successful builds always remove it |

Refuses a no-op by default:

```console
$ sudo kiln build
Nothing to do: the newest generation already matches this configuration.
  plan b3:11c4de8a3f5b02e7c94daf61b8035e2ad7c1904fe6b83d20a5c7194e3f8b6d02

`kiln build --force` rebuilds anyway.
```

Warns when the target sysroot has never been initialized, because building into an
uninitialized target succeeds and then fails several hundred megabytes later at deploy.

#### `kiln apply [--force] [--offline] [--keep-failed]`

`kiln build`, then stage the result for the next boot. This is the command you usually want.

```console
$ sudo kiln apply
Building workstation (b3:99887766abcd)
  312 packages fetched
  ...
Generation 44 committed as c07be1d4f9a2.
Generation 44 is staged for the next boot.
Reboot to use it. `kiln rollback` returns to the previous one.
If it does not reach boot-complete.target in 3 attempts, the previous generation boots instead.
```

#### `kiln rebuild <gen>`

Reconstructs a past generation from its own commit, not from `/etc/kiln`. The recorded
snapshot date drives the Arch Archive mirrors; the recorded checksums and AUR commits pin
everything else.

It reads neither `/etc/kiln`'s TOML nor today's package versions, which is the whole point: a
configuration that has since been edited or deleted is the normal case for the questions a
rebuild answers. It still needs the config root for `[[file]]` and script `source` bytes.

It reports precisely what came back differently:

- a different `plan_id` means the inputs no longer resolve to what they did (usually a package
  the Archive no longer serves at that version);
- a different changeset for a build script means that script is not a pure function of its
  inputs, which is the only audit that finds an unreproducible script;
- a different commit checksum with everything else equal is a bug in Kiln.

Requires both the record and the manifest in the commit; a generation built by an older Kiln
that wrote only one of the two says so and stops.

### 5.3 Inspection

#### `kiln explain <key>`

Which file set a value, and what it overrode. Four things can be asked about, and the argument
alone says which.

**An exact key:**

```console
$ kiln explain boot.timeout
boot.timeout
  value       0
  set in      system.toml:30
  overriding  @kiln/boot/grub2:18

  The includer wins over what it includes (rule 2). Two files
  at the same depth disagreeing would have been an error, not this.
```

**A group** lists every key underneath it, set or not:

```console
$ kiln explain boot
boot
  a group of keys, not a value of its own

  boot.loader     "grub2"
                  set in @kiln/boot/grub2:17
  boot.timeout    0
                  set in system.toml:30
  boot.initramfs  "dracut"
                  Kiln's default  — the only supported value

  `kiln explain <one of these>` for the whole story of one of them.
```

**A list** shows who asked for each element:

```console
$ kiln explain packages.repo
packages.repo
  kind        a list — 9 files unions into it (rule 1)
  22 elements, and who asked for each:
    fish            system.toml:18
    gdm             @kiln/desktop/gnome:5
    gnome-console   @kiln/desktop/gnome:6
    nvidia-open     @kiln/gpu/nvidia-open:11
    ...

  Order does not matter: every contributor's elements are in the image,
  deduplicated. `kiln explain packages.repo/<element>` asks about one of them.
```

**An element** answers "which file put this here":

```console
$ kiln explain packages.repo/gnome-shell
packages.repo/gnome-shell
  asked for   @kiln/desktop/gnome:5
  in          packages.repo

  That is where it was written down. `kiln why gnome-shell` answers the other
  half — whether a built image contains it because you asked, or because
  something else depends on it.
```

`kiln explain include` is the odd one out. There is no value to print, because the graph
consumes the key, so it prints the graph:

```console
$ kiln explain include
include
  kind        the include graph, not a value
  10 files, entry point first:
    system.toml
    hardware.toml
    @kiln/gpu/nvidia-open
    @kiln/profiles/minimal
    ...
```

An unknown key is the only one of the four answers that is a mistake, and the only one that
exits nonzero (`1`) with a did-you-mean suggestion.

#### `kiln show [<gen>]`

With no argument: the merged manifest built from the configuration on disk, in summary and in
full, ending with `config_id`.

With a generation: the same thing read out of that generation's **commit**, plus its record.
Works on a generation whose configuration has since been edited or deleted, and on one that
was committed and never deployed.

```console
$ kiln show 42
generation  42
image       workstation x86_64
built       2026-08-28T09:41:15Z on forge
commit      7f2a04e19bc8...
plan        b3:11c4de8a...
config      b3:9f2c11a4...
...
record
  snapshot        2026-08-28
  repo packages   412
  aur packages    2
  scripts         1

  `kiln show 42 --verbose` prints the whole record.
```

#### `kiln diff [<gen>] [<gen>]`

What changed between two generations, read from their commits.

| Typed | Compares |
| --- | --- |
| `kiln diff` | The booted generation against the one that boots next |
| `kiln diff 41` | Generation 41 against the booted one |
| `kiln diff 39 41` | Exactly those two |

With nothing pending, it declines and points at `kiln check`, which answers the question you
were probably asking. Two generations with the same `plan_id` are reported as identical
builds of the same plan rather than as an empty table.

#### `kiln why <package> [<gen>]`

What pulled a package into the image. Answered from the image's own pacman database, not from
the plan, because the plan names what the configuration asked for while the image contains the
whole dependency closure.

```console
$ kiln why mesa
mesa 26.1.5-1
  from the extra repository
  required by gnome-shell, xdg-desktop-portal-gnome
```

It also resolves virtual names, reports AUR provenance with the commit, reports built packages
with their build key and kernel, and says plainly when a package is neither named nor required
by anything, which is the answer to "why is this still here" after an `exclude`.

The optional trailing generation asks a past image instead of the booted one. That generation
must be **deployed**, because these commands read a checked-out tree.

#### `kiln owns <path> [<gen>]`

Which package owns a file in the image. Both the path you typed and its `/usr/etc` spelling
are tried, so `/etc/pacman.conf` works on a booted machine:

```console
$ kiln owns /etc/pacman.conf
pacman
  /etc/pacman.conf is usr/etc/pacman.conf in the image: Kiln moves /etc to
  /usr/etc and the live /etc is merged onto it at deploy.
  pacman 7.1.0-1
```

An unowned path is a real answer, not a failure: it may be a `[[file]]` Kiln placed, a build
script's output, or runtime state under `/var`.

### 5.4 Deployments

Every one of these takes a **generation**, never an OSTree deployment index.

#### `kiln list`

```console
$ kiln list
 GEN  STATUS                    COMMIT         GENERATED           IMAGE
  44  boots next                c07be1d4f9a2   2026-09-03 14:50:11 workstation
  43  ● booted, rollback target 9d41af02c3b1   2026-09-01 14:22:07 workstation
   1  baseline                  31c9be750ad4   2026-07-02 11:03:44 workstation
```

Status carries the three facts you act on: which generation you are running (`● booted`),
which one `kiln rollback` would take you to (`rollback target`), and which ones `kiln clean`
will not remove (`baseline`, `pinned`). `boots next` marks a generation `kiln apply` staged
that you have not booted yet.

#### `kiln status`

```console
$ kiln status
generation  43
image       workstation
built       2026-09-01 14:22:07
state       booted
pending     generation 44 boots next — reboot to use it
rollback    generation 42
boot        attempt 1 of 3 — this generation has not been marked good yet
```

`--verbose` adds the commit checksum and the full `kiln list` table. `kiln status` is also
where `/etc` drift is reported; see [section 11.5](#115-etc-drift).

#### `kiln rollback`

Boots the previous generation. Reorders the deployment list; the old image is still on disk,
byte for byte. It restores the **image** and not `/var`.

#### `kiln deploy <gen>`

Makes a specific generation boot next. Two jobs behind one verb: a generation that is already
deployed is reordered, and one that has only ever been *committed* (which is everything
`kiln build` produces) is deployed here and now, using the kernel command line out of its own
commit metadata.

#### `kiln pin <gen>` / `kiln unpin <gen>`

Keeps a generation through `kiln clean`, or stops doing so.

#### `kiln rm <gen>...`

Undeploys one or more generations. Refuses generation 1 without `--remove-baseline`. Naming a
generation that does not exist is a different mistake from naming a protected one, and gets a
different message. If every generation named was protected, the command exits nonzero, because
a script running `kiln rm 1 && ...` should not proceed.

#### `kiln clean [--keep N] [--dry-run] [--remove-baseline]`

Trims both budgets, because they are the same question: this machine has one disk.

- **Deployments**: keeps `N` (default 3), plus the baseline, plus anything pinned, plus what
  is booted.
- **Artifact cache**: budget is `min(20 GiB, 10% of the filesystem)`. Eviction is oldest
  first and stops the moment it is under budget, so the next build does not re-download
  packages it had a minute ago.

```console
$ kiln clean --dry-run
Removing generation 38, 39.
  keeping generation 1: it is the baseline; `--remove-baseline` overrides
Artifact cache 3.2 GiB over its 20.0 GiB budget: dropping 41 oldest packages, 3.4 GiB.

Nothing was removed. Run without `--dry-run` to do it.
```

### 5.5 Storage

#### `kiln init`

Scaffolds `/etc/kiln/system.toml` (or `--config <path>`). Refuses to overwrite an existing
file. The scaffold is four lines plus a comment on purpose; a scaffold that opens with thirty
commented-out keys teaches the opposite of what Kiln is.

#### `kiln sysroot init [<path>]`

Creates the OSTree layout Kiln deploys into: the repository plus
`ostree/deploy/kiln`. Like `git init`, it makes a directory into something Kiln can work with.
The path may be given positionally or with `--sysroot`; giving both with different values is
an error.

```console
$ sudo kiln sysroot init /mnt
Initialized an OSTree sysroot at /mnt.
  stateroot   kiln
  bootloader  BLS entries; grub2 once deploying to /
  boot        3 attempts before automatic rollback

`kiln build --sysroot /mnt` can now commit into it.
```

It is a separate verb from `kiln init` deliberately: conflating "make me a config" with "make
me a bootable root" is a mistake somebody makes exactly once.

### 5.6 Shell completions

```console
$ kiln completions bash    # or zsh, or fish
```

Prints a completion script to stdout. Source it for the current session:

```console
$ source <(kiln completions bash)
```

Or install it where the shell's loader already looks:

| Shell | Path |
| --- | --- |
| bash | `/usr/share/bash-completion/completions/kiln` |
| zsh | `/usr/share/zsh/site-functions/_kiln` |
| fish | `/usr/share/fish/vendor_completions.d/kiln.fish` |

The Arch package installs all three.

### 5.7 Commands that will never exist

Some verbs are recognized and answered rather than reported as unknown:

| Typed | Answer |
| --- | --- |
| `kiln upgrade` | There is exactly one way to get a new image: `kiln apply`, whether you edited TOML or want a new kernel |
| `kiln install` | Installation is an installer's job; Kiln exposes `--sysroot` and `kiln sysroot init` for one to build against |
| `kiln push`, `pull`, `remote` | Kiln is a distribution's build tool, not an image-shipping pipeline |

### 5.8 Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Fine, including "nothing to do" |
| `1` | Configuration error (discovery, syntax, structure, graph, merge, semantic) |
| `2` | Resolution failure |
| `3` | Build failure |
| `4` | System or permission error |
| `10` | `kiln check` found changes |

---

## 6. The configuration system

### 6.1 Discovery

```text
/etc/kiln/
├── system.toml         ← the entry point
├── hardware.toml
├── files/
├── units/
├── scripts/
└── pkgbuilds/
```

The entry point is `/etc/kiln/system.toml`, or whatever `--config` points at (a file, or a
directory holding `system.toml`). The directory containing it is the **config root**.

Two rules about the config root matter up front:

- **It is a security boundary.** Every `source` and `path` in the whole configuration must
  resolve, after following symlinks, inside it. `source = "../../home/you/.ssh/id_ed25519"` is
  a hard error, not a leak. Escaping needs `--allow-external-sources`, which warns once per
  path that did.
- **Nothing is glob-loaded.** There are no drop-in directories. Every file that participates
  is reachable through an explicit `include` chain from the entry point.

`/etc/kiln` is not required to be the source of truth. `kiln build --config ~/src/my-image`
works identically, which is what makes a git-tracked configuration and a build server
possible.

### 6.2 File shape

Every Kiln file:

```toml
kiln = 1                        # required, and must be the first key in the file

include = ["hardware.toml"]     # optional, top-level, before any table header

[section]
key = "value"
```

`kiln = 1` is required in **every** file, not only the entry point, and it must come first by
position. That rule exists for a reader scanning the top of a file.

`include` must be a top-level key **before any table header**. A bare `include` written after
`[packages]` silently becomes `packages.include` in TOML; Kiln catches that by span and tells
you to move it.

`kiln` and `include` describe the file, not the image. They are stripped before merging, so
two files both saying `kiln = 1` is not a disagreement.

### 6.3 The merge algebra

Three rules, and nothing else:

1. **Lists union.** Duplicates collapse, order is discarded. `packages.repo` contributed by
   six files is one deduplicated set. Reordering lines in a file can never change what you get.
2. **The includer wins.** If your `system.toml` includes `@kiln/boot/grub2` and both set
   `boot.timeout`, yours is used.
3. **Siblings conflicting is an error.** Two files at the *same* depth setting the same scalar
   to *different* values is a hard error naming both files and lines, never a silent
   last-one-wins. Set it yourself in the includer to resolve it. Identical values are fine.

Arrays of tables merge by an identity key rather than by position:

| List | Identity |
| --- | --- |
| `packages.repo`, `packages.aur`, `repos.extra`, `kernel.module`, `kernel.dkms`, `systemd.unit`, `script` | `name` |
| `packages.build`, `packages.file` | `path` |
| `file` | `target` |

Two files describing the same `[[file]]` target combine rather than duplicate, so collisions
are impossible by construction.

There is **no unset operator**. `packages.exclude`, `systemd.disable` and `systemd.mask` cover
the real cases. If you need to not have something, do not include the file that adds it.

### 6.4 Shorthand

Everywhere a table is accepted in a list, a bare string is too, meaning that table with only
its primary key set:

```toml
repo = ["firefox"]                       # ≡ [{ name = "firefox" }]
aur  = ["zen-browser-bin"]               # ≡ [{ name = "zen-browser-bin" }]
build = ["pkgbuilds/my-driver"]          # ≡ [{ path = "pkgbuilds/my-driver" }]
dkms = ["nvidia-open-dkms"]              # ≡ [{ name = "nvidia-open-dkms" }]
```

Shorthand is expanded *before* merging, so `"firefox"` and `{ name = "firefox" }` deduplicate
against each other. `kiln explain` collapses it back when printing, so you see what you wrote.

`[[script]]` is the one exception: it identifies by `name`, but shorthand writes only
`source`, so the name is derived from the source file's stem. A script with inline `content`
and no `source` must give an explicit `name`.

### 6.5 Validation

Validation runs in phases, and each reports everything it found before the next starts:

| Phase | Checks |
| --- | --- |
| `discovery` | The entry point exists and is readable |
| `syntax` | TOML parses |
| `structure` | `kiln = 1` present and first; `include` placement; unknown keys (with did-you-mean); scalar types; list shapes; per-entry keys of arrays of tables |
| `graph` | Include references resolve; no cycles; depth ≤ 32; no remote includes; boundary enforced |
| `merge` | Sibling conflicts |
| `semantic` | Typed extraction, enum values, unit names, file modes, target paths, `source` xor `content`, checksum presence, local file hashing and the config-root boundary |

### 6.6 The whole schema

Seven top-level tables and five array-of-table forms. This is all of it:

```toml
kiln = 1                                  # schema version. Required, first key.

include = ["hardware.toml", "@kiln/profiles/workstation"]

[image]
name = "workstation"                      # → ostree ref kiln/workstation/x86_64
arch = "x86_64"

[repos]
snapshot = "2026-08-24"                   # or omit: track live mirrors, like Arch
mirrors  = ["https://mirror.example.com/archlinux/$repo/os/$arch"]
extra    = [{ name = "myrepo", server = "https://pkgs.example.com/$arch",
              key = "keys/myrepo.gpg" }]

[packages]
repo    = ["base", "git", "neovim", "firefox"]
aur     = ["zen-browser-bin", { name = "foo-git", commit = "a81fc2e" }]
build   = ["pkgbuilds/my-driver"]
file    = [{ path = "packages/myapp-1.0-1-x86_64.pkg.tar.zst", sha256 = "9f2c…" }]
exclude = ["nano"]                        # must not appear, even as a dependency

[kernel]
package        = "linux"
headers        = false
cmdline        = ["quiet", "amd_iommu=on"]
dracut_modules = ["plymouth"]             # dracut modules to --add beyond ostree
dkms           = ["nvidia-open-dkms"]     # DKMS sources, compiled at build time

[kernel.modules]
load      = ["v4l2loopback"]
blacklist = ["nouveau"]
initramfs = ["i915"]                      # drivers to put *in* the initramfs
options   = { v4l2loopback = "devices=2 exclusive_caps=1" }

[[kernel.module]]                         # built from source, out of tree
name   = "my-module"
source = "kernel/my-module"

[[kernel.dkms]]                           # the long form: a DKMS tree of your own
name   = "my-driver"
source = "kernel/my-driver"

[boot]
loader    = "grub2"
timeout   = 5
initramfs = "dracut"

[systemd]
enable  = ["sshd.socket", "fstrim.timer"]
disable = ["systemd-resolved.service"]
mask    = ["NetworkManager-wait-online.service"]

[[systemd.unit]]
name   = "backup.timer"
source = "units/backup.timer"
enable = true

[[file]]
source = "files/motd"
target = "/etc/motd"

[[file]]
target  = "/usr/lib/tmpfiles.d/scratch.conf"
content = "d /var/scratch 0755 root root 30d\n"

[[script]]                                # the escape hatch
source = "scripts/20-locale.sh"
after  = "files"

[system]
hostname = "forge"
timezone = "Asia/Riyadh"
keymap   = "us"
locale   = { lang = "en_US.UTF-8", generate = ["en_US.UTF-8 UTF-8"] }
```

Every table is optional. An empty configuration is valid and produces an empty image, which
will then fail resolution with "no kernel in this image" and "no init in this image", both
pointing at `@kiln/profiles/minimal`.

### 6.7 Key reference

#### Root

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `kiln` | integer | **required** | Schema version. Only `1` is understood. Must be the first key in every file |
| `include` | list of strings | `[]` | Files and modules to compose in. Top-level only, before any table header |

#### `[image]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `image.name` | string | `"system"` | Names the OSTree ref `kiln/<name>/<arch>`. Two configurations with different names build independent linear histories on one machine |
| `image.arch` | string | the host's architecture | Target architecture. Multi-arch is not implemented; this is the arch used for repository URLs and build keys |

#### `[repos]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `repos.snapshot` | string | `"latest"` | `"latest"` tracks live mirrors, like Arch. A `YYYY-MM-DD` date resolves everything from `archive.archlinux.org` instead. A pinned snapshot *replaces* the mirrors rather than adding to them, so an image is never half archived and half live |
| `repos.mirrors` | list of strings | the Arch geo mirror | Server URL templates using pacman's `$repo` and `$arch`. Nothing else is substituted; this is a URL template, not a language. Ignored when `snapshot` names a date |
| `repos.extra` | array of tables | `[]` | Additional repositories, in priority order after `core` and `extra` |

`repos.extra` entries take `name` (required), `server` (required, a URL template), and `key`
(optional, a path to a GPG key relative to the config root).

A repository with a `key` is treated as one whose package signatures must verify; one without
is accepted but marked **unsigned** rather than silently trusted. The key file is hashed into
`config_id` so a changed key changes the image identity. Kiln does not yet import it into its
own pacman keyring, so a signed third-party repository whose key the Arch keyring does not
carry will fail verification at fetch time.

An invalid date gets a diagnostic naming the two accepted forms.

#### `[packages]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `packages.repo` | list of names | `[]` | Packages from `core`, `extra`, and any `repos.extra` |
| `packages.aur` | list of names or tables | `[]` | AUR packages, built in a sandbox. Entry keys: `name`, `commit` |
| `packages.build` | list of paths | `[]` | Directories in the config tree holding a `PKGBUILD` and a `.SRCINFO`. Entry key: `path` |
| `packages.file` | list of tables | `[]` | A `.pkg.tar.zst` by path or URL. Entry keys: `path` (required), `sha256` (required) |
| `packages.exclude` | list of names | `[]` | Packages that must not appear in the image, even as a dependency |

See [section 7](#7-packages).

#### `[kernel]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `kernel.package` | string | `"linux"` | Which kernel package the image is built around. Kiln locates it by `/usr/lib/modules/*/pkgbase` and refuses to guess if it finds two |
| `kernel.headers` | boolean | `false` | Declares whether the image should carry the kernel's headers. See the note below |
| `kernel.cmdline` | list of strings | `[]` | Kernel command line. **Fully declarative**: Kiln passes exactly this set at every deploy and keeps no hidden additions |
| `kernel.dracut_modules` | list of strings | `[]` | dracut modules to `--add` beyond the `ostree` one Kiln always requests. Needed because dracut's non-hostonly selection does not pull in every module whose package is installed |
| `kernel.modules.load` | list of strings | `[]` | Modules the booted system should load. Written to `/etc/modules-load.d/kiln.conf`, one name per line, for `systemd-modules-load.service` |
| `kernel.modules.blacklist` | list of strings | `[]` | Modules the booted system should not load. Written to `/etc/modprobe.d/kiln.conf` as `blacklist <name>` lines |
| `kernel.modules.initramfs` | list of strings | `[]` | Drivers to put *in the initramfs* (dracut's `--add-drivers`). Verified after generation: a driver that did not make it in fails the build |
| `kernel.modules.options` | table of string → string | `{}` | Module options. Keys are module names you choose, so the schema enumerates the table but never its contents. Written to `/etc/modprobe.d/kiln.conf` as `options <name> <value>` lines, alongside `blacklist` |
| `kernel.module` | array of tables | `[]` | Out-of-tree modules built from source with `make`. Entry keys: `name`, `source` (both required) |
| `kernel.dkms` | list of names, or array of tables | `[]` | DKMS drivers compiled at build time. Entry keys: `name` (required), `source` (optional) |

**On `kernel.headers`.** It defaults to `false`, is part of `config_id`, and is reported by
`kiln show` and `kiln explain`. Nothing in assembly currently reads it: module and DKMS builds
install `<kernel>-headers` into their build root from the resolved kernel regardless, and that
root is never the image. If you genuinely want headers *inside* the image, name
`linux-headers` in `packages.repo`. Shipping ~150 MB of headers in an immutable system that
never rebuilds modules at runtime is usually waste, which is why the default is what it is.

**On `kernel.cmdline`.** Because kargs are fully declarative, a deploy without `root=`
produces a machine that boots exactly once. `@kiln/boot/grub2` contributes `rw`, without which
the deployment's root is mounted read-only and the first boot reaches an emergency shell.

**`kernel.modules.load`/`blacklist`/`options`.** Assembly writes these as ordinary generated
image content — `/etc/modules-load.d/kiln.conf` for `load`, `/etc/modprobe.d/kiln.conf` for
`blacklist` and `options` together — during the same step that builds the initramfs (step 9),
so `/etc`'s move to `/usr/etc` (step 10) carries them along like anything else assembly writes.
An empty list writes no file at all. If you need something these three keys cannot express —
multiple `modprobe.d` fragments, a fourth directive — a `[[file]]` targeting the same paths
still works; the two mechanisms write the same files and the last one to run wins.

`kernel.dracut_modules` and `kernel.modules.initramfs` are different from all three: those feed
the initramfs itself, not the booted system's `modules-load.d`/`modprobe.d`, and are what a
shipped module such as `@kiln/boot/plymouth` relies on.

#### `[boot]`

| Key | Type | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `boot.loader` | string | `"grub2"` | `"grub2"` | The bootloader. One value, because there is exactly one supported answer |
| `boot.timeout` | integer | `5` | `0` and up | GRUB menu delay in seconds. `0` means no delay. Negative is an error |
| `boot.initramfs` | string | `"dracut"` | `"dracut"` | The initramfs generator |

Writing `boot.loader = "systemd-boot"` or `boot.initramfs = "mkinitcpio"` gets a diagnostic
explaining exactly why neither can work, rather than an unknown-value message. See
[section 3.4](#34-the-arch--ostree-contract).

#### `[systemd]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `systemd.enable` | list of unit names | `[]` | Units to enable |
| `systemd.disable` | list of unit names | `[]` | Units to disable |
| `systemd.mask` | list of unit names | `[]` | Units to mask |
| `systemd.unit` | array of tables | `[]` | Unit files to ship. Entry keys: `name` (required), `source` or `content` (exactly one), `enable` (boolean, default `false`) |

Unit names must end in a real unit suffix: `service`, `socket`, `timer`, `target`, `mount`,
`automount`, `path`, `slice`, `scope`, `swap`, `device`. A name with no dot gets told to write
`<name>.service` if that is what it meant.

A unit may appear in more than one list. Masking is the strongest statement and wins;
disabling beats enabling. That is resolved during resolution, so the plan says what the image
will do rather than what the configuration said.

See [section 9](#9-systemd).

#### `[[file]]`

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `target` | string | yes | Absolute, normalized system path as you would see it on a running machine |
| `source` | string | one of the two | Path relative to the config root. A trailing slash means a recursive tree copy |
| `content` | string | one of the two | Inline file contents |
| `mode` | string | no | Three or four octal digits, **as a string**. TOML has no octal literal, and `0755` would be decimal 755 |

See [section 8](#8-files-and-the-filesystem).

#### `[[script]]`

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `name` | string | yes, but derived from `source`'s file stem when absent | Identity, and therefore the run order |
| `source` | string | one of the two | Path relative to the config root |
| `content` | string | one of the two | Inline script text |
| `after` | string | no | `"packages"` or `"files"` (the default). Which assembly slot it runs in |

#### `[system]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `system.hostname` | string | unset | The image's hostname. Unset means systemd's own default applies |
| `system.timezone` | string | `"UTC"` | Timezone name, as under `/usr/share/zoneinfo` |
| `system.keymap` | string | `"us"` | Console keymap |
| `system.locale.lang` | string | `"C.UTF-8"` | The default `LANG` |
| `system.locale.generate` | list of strings | `[]` | Locales to generate, written the way `locale.gen` wants them (`"en_US.UTF-8 UTF-8"`) |

`system.timezone`, `system.keymap` and `system.locale.lang` always have a concrete value —
`"UTC"`, `"us"` and `"C.UTF-8"` are defaults, not an unset state — so `/etc/localtime`,
`/etc/vconsole.conf` and `/etc/locale.conf` are written for every image, whether or not the
configuration has a `[system]` table at all. `system.hostname` is the one key with a real
unset (`None` means systemd's own default applies), and `system.locale.generate` only ever
names locales beyond the ones glibc's stock archive already carries, so `/etc/hostname` and
`/etc/locale.gen` are written only when the configuration actually sets them; when
`system.locale.generate` is non-empty, `locale-gen` also runs, chrooted into the image, to
compile the archive.

Because the first three are always written, a `[[file]]` targeting `/etc/localtime`,
`/etc/vconsole.conf` or `/etc/locale.conf` is refused outright — there is no `[[file]]` route
to them. A `[[file]]` targeting `/etc/hostname` or `/etc/locale.gen` is refused only once the
matching `[system]` key is actually set, since the two would otherwise race:

```toml
[system]
hostname = "forge"
timezone = "Asia/Riyadh"
keymap   = "us"
locale   = { lang = "en_US.UTF-8", generate = ["en_US.UTF-8 UTF-8"] }
```

### 6.8 Ordering never matters

Every collection in the manifest is a `BTreeMap` or `BTreeSet` and is canonically sorted
before hashing. Reordering lines in a TOML file cannot change `config_id`. This is
property-tested: sibling order does not change `config_id` or validity, list order within a
file does not change `config_id`, and union is associative.

---

## 7. Packages

Six kinds of input, all of which end up as a `.pkg.tar.zst` going through pacman:

| Key | What it is |
| --- | --- |
| `packages.repo` | Official Arch repositories, and any `repos.extra` you add |
| `packages.aur` | AUR packages, built in a sandbox |
| `packages.build` | Your own PKGBUILDs, from a directory in your config tree |
| `packages.file` | A `.pkg.tar.zst`, local or by URL, with a required `sha256` |
| `[[kernel.module]]` | An out-of-tree module, built against the image's kernel |
| `kernel.dkms` | DKMS sources (a package, or a tree of your own) compiled against the image's kernel |

### 7.1 Repository packages

```toml
[packages]
repo = ["base", "git", "neovim", "firefox"]
```

Resolved by libalpm against `core`, `extra` and anything in `repos.extra`, in priority order.
The full dependency closure comes with them; you name only what you actually want.

Packages you named are recorded as **explicitly installed**, so `pacman -Qe` on the booted
image means something.

Two bootability checks run during resolution, not at assembly, so `kiln check` catches them
without building anything:

- the image must contain `kernel.package`;
- something must provide `init` (on Arch, `systemd`).

Both point at `@kiln/profiles/minimal`.

### 7.2 Excluding packages

```toml
[packages]
exclude = ["nano"]
```

An excluded package must not appear in the image, **even as a dependency**. The check runs
*after* the solve, deliberately: telling libalpm to assume it installed would make the
dependency vanish and produce an image missing a library something links against. Instead
resolution fails and names what pulled the package in, leaving the decision with whoever wrote
the configuration.

### 7.3 AUR packages

```toml
[packages]
aur = [
  "zen-browser-bin",
  { name = "foo-git", commit = "a81fc2e" },
]
```

**Identity is the git commit**, not the version string. A maintainer force-pushing a different
PKGBUILD at the same `pkgver` is a detected change, not an invisible one. Kiln also hashes the
`.SRCINFO` the RPC reports, so a metadata change at an unchanged commit is visible too.

Pin a commit to freeze a package; leave it out to track the AUR's current `HEAD`.

**The dependency closure is recursive**, over `Depends` and `MakeDepends` that are not in the
official repositories, with a cycle check and a depth cap of 10. Every transitively pulled AUR
package records what required it: nothing enters the image anonymously.

**An AUR package's repo dependencies are not in the plan.** If `zen-browser-bin` needs
`qt6-base`, that comes from the official repositories at build time and is not something
`kiln check` reports moving. Asking the AUR for `qt6-base` would fail with a message about a
package nobody wrote down, so the closure deliberately stops at the AUR boundary.

**VCS packages are volatile.** A `-git` package's version comes from running `pkgver()`
against upstream, which resolution will not do. It is excluded from `plan_id`, reported
separately, and answered by `kiln check --deep`.

Packages the AUR itself has flagged out of date are reported once. That is not an error;
plenty of working packages are flagged.

### 7.4 Your own PKGBUILDs

```toml
[packages]
build = ["pkgbuilds/my-driver"]
```

```text
pkgbuilds/my-driver/
├── PKGBUILD
├── .SRCINFO          ← required
└── ...
```

**A `.SRCINFO` is required.** Kiln reads what a recipe *declares* rather than running it, so
that `kiln check` stays a metadata query rather than a shell script execution. If the
directory has none, you get:

```text
× `pkgbuilds/my-driver` has no .SRCINFO
help: Kiln reads what a recipe declares rather than running it, so that `kiln check`
      stays a metadata query. Commit one beside the PKGBUILD:

        cd pkgbuilds/my-driver && makepkg --printsrcinfo > .SRCINFO
```

A recipe may be a **split package**; the first `pkgname` keys the plan, and the transaction
gets every package the build produces.

The build is cached on a `build_key`:

```text
build_key = blake3(
    recipe_tree_hash      # blake3 of the directory, VCS metadata excluded
  ⧺ sorted(source_pins)   # sha256 of every fetched source
  ⧺ sorted(makedep_evrs)  # exact versions of the build-time dependency closure
  ⧺ target_arch
  ⧺ builder_version       # bumped when sandbox or toolchain semantics change
)
```

Including `makedep_evrs` is what makes the cache **correct** rather than merely fast: a
package built against `gcc 15.1` is not the same artifact as one built against `gcc 15.2`.
Build-time dependencies resolve against the same repository snapshot as the image, and
`base-devel` is always in the closure because it is always in the build root.

### 7.5 Local and remote package files

```toml
[packages]
file = [
  { path = "packages/myapp-1.0-1-x86_64.pkg.tar.zst", sha256 = "9f2c…" },
  { path = "https://example.com/myapp.pkg.tar.zst", sha256 = "https://example.com/myapp.pkg.tar.zst.sha256" },
]
```

**The checksum is required, not optional.** An optional integrity guarantee is not a
guarantee. For a local file, resolution verifies the bytes on disk against the declared
`sha256` and reports a mismatch with the actual digest so you can decide whether the change
was intended.

**`path` may be a URL.** Only `http://` and `https://` are accepted; any other scheme is
refused at parse time rather than silently treated as a path with a colon in it. A URL's bytes
are downloaded during `kiln build`, not during `kiln check`; the URL and its `sha256` carry
the identity into `plan_id` the same way an AUR commit does. Downloads are cached under
`/var/lib/kiln/cache/file-packages`, keyed by checksum.

**`sha256` may be a URL too**, pointing at a `.sha256` file rather than naming the digest.
Unlike the package itself, `kiln check` **does** fetch this: it is a few bytes, the same kind
of call resolution already makes to the AUR RPC, and it resolves to a concrete digest before
reaching the plan. The file is parsed the way `sha256sum` writes one: 64 hex characters,
optionally followed by whitespace and a filename.

This is how `@kiln/terracotta/kiln` installs Kiln itself from its GitHub release.

### 7.6 The network rule

**The network is on during resolution and source fetching, and off from the moment a build
phase starts.** That constraint is what makes a build's output a pure function of things Kiln
has hashed, and it means a PKGBUILD that downloads something in `build()` will fail, which is
correct.

The two-phase build is how that is arranged:

```text
phase 1  fetch    network ON,  makepkg --verifysource: no build code beyond pkgver()
phase 2  build    network OFF, sources bind-mounted from the cache
```

### 7.7 Snapshots and rebuilds

`repos.snapshot` is **opt-in**. By default Kiln tracks live mirrors, like Arch:

```toml
[repos]
snapshot = "2026-08-24"   # or omit entirely
```

Even in rolling mode, every build **records the date it resolved on**. That single field is
what makes a past image reconstructible without anyone having pinned anything in advance:
`kiln rebuild <gen>` replaces the manifest's snapshot with the recorded date and resolves from
the Arch Archive.

---

## 8. Files and the filesystem

### 8.1 `[[file]]`

```toml
[[file]]
source = "files/motd"
target = "/etc/motd"          # Kiln owns the /usr/etc translation; you write /etc

[[file]]
source = "bin/mytool"
target = "/usr/bin/mytool"
mode   = "0755"

[[file]]
source = "files/sysctl/"      # trailing slash = recursive tree copy
target = "/usr/lib/sysctl.d/"

[[file]]
target  = "/usr/lib/tmpfiles.d/scratch.conf"
content = "d /var/scratch 0755 root root 30d\n"
```

`source` or `content`, never both and never neither. A file with neither is empty by accident;
a file with both is ambiguous, and Kiln will not guess.

Modes are strings, three or four octal digits. Writing `mode = 755` gets a diagnostic saying
that is the decimal number 755 and suggesting `mode = "0755"`.

### 8.2 Where a target may land

The routing table is the interesting part, because every wrong answer here is a file that is
silently not in the image, or in it and doing nothing.

| Target | Result |
| --- | --- |
| `/usr/**` | Written straight into the commit |
| `/etc/**` | Written to `etc/` in the staging root, moved to `/usr/etc` by normalization, 3-way merged at deploy |
| `/var/**`, `/opt/**`, `/srv/**` | **Seeded**, not written: the bytes go to `/usr/share/factory` and a `tmpfiles.d` `C` line restores them on a machine that has no copy of its own |
| `/usr/etc/**` | Refused. That is where Kiln puts `/etc`; write `/etc/...` instead |
| `/boot/**` | Refused. OSTree owns `/boot`; the kernel and boot entries are Kiln's to place |
| `/home/**`, `/root/**` | Refused. Home directories are machine state, and Kiln does not manage login accounts |
| `/proc`, `/sys`, `/dev`, `/run`, `/tmp` | Refused. Nothing written there would survive a boot |
| `/sysroot/**`, `/ostree/**` | Refused. OSTree's own storage |
| `/mnt/**` | Refused. Not a directory the image has |
| A relative path, a trailing slash with no filename, `..` components, a top-level file | Refused, each with its own message |

Every refusal in a configuration is reported at once, not one per run.

### 8.3 Seeded targets

A `[[file]]` targeting `/var`, `/opt` or `/srv` is **accepted with a note**:

```text
⚠ `/var/lib/myapp/seed.db` is seeded, not written into the image
help: /var is not in the commit: this becomes a factory copy plus a tmpfiles `C`
      line, so it is restored on a machine that has no copy of its own and left
      alone on one that does
```

That is how you ship a default database or a seed file. It is a one-time restoration rather
than something the image re-asserts on every boot, which is the right semantics for state that
belongs to the machine.

`/opt` and `/srv` are relocated into `/var` before the drain, so they take the same route. If
you want something *in the image itself*, install it to `/usr`.

### 8.4 Modes and secrets

Kiln warns on a mode more restrictive than `0644`. `/usr` is world-readable and a commit
outlives the generation it was built for, so a mode signalling secrecy is a mode the image
cannot honor. Secrets belong in `/var` at runtime, put there by something else.

### 8.5 The `/var` drain

`/var` must not exist in the commit at all. Normalization drains it:

- a directory becomes a tmpfiles `d` line recreating it with the right mode and ownership;
- a file becomes a copy under `/usr/share/factory` plus a tmpfiles `C` line;
- a symlink becomes a tmpfiles `L` line, **not** a factory copy, because `C` stats the factory
  path and a relative link like `/var/lock → ../run/lock` would resolve inside the factory
  tree and dangle;
- logs and caches are excluded entirely.

Anything larger than 8 MiB going into the factory earns a warning: it usually means a package
is shipping data that belongs in `/usr`.

The result is that a freshly deployed machine with an empty `/var` comes up correctly, and a
machine with existing `/var` state keeps it.

`/root`, `/home`, `/opt` and `/srv` are moved into `/var/roothome`, `/var/home`, `/var/opt`
and `/var/srv` *before* the drain, so anything a package left in them goes through exactly one
code path, and then they become symlinks.

### 8.6 Ownership questions

```console
$ kiln owns /usr/bin/ls
coreutils
  coreutils 9.9-1
```

An unowned path is answered rather than reported as a failure: it may be a `[[file]]`, a build
script's output, or `/var` runtime state that is not image content at all.

---

## 9. systemd

### 9.1 Two separate concerns

Shipping a unit file and deciding whether a unit runs are separate, and the schema keeps them
separate. A package's unit can be enabled without Kiln shipping anything, and Kiln can ship a
unit without enabling it.

```toml
[systemd]
enable  = ["sshd.socket"]
disable = ["systemd-resolved.service"]
mask    = ["NetworkManager-wait-online.service"]

[[systemd.unit]]
name   = "backup.timer"
source = "units/backup.timer"
enable = true

[[systemd.unit]]
name    = "hello.service"
content = "[Unit]\nDescription=Hello\n[Service]\nExecStart=/usr/bin/true\n"
enable  = true
```

### 9.2 How enablement is realized

Not by running `systemctl enable` in a chroot. Kiln writes a **preset file**:

```text
/usr/lib/systemd/system-preset/20-kiln.preset
```

```text
# Generated by Kiln from [systemd] enable/disable.
# Read before every preset Arch ships, so these lines win.
enable sshd.socket
disable bluetooth.service
```

`20-` puts Kiln ahead of every preset Arch ships (`90-systemd.preset`, and its `disable *`
default). Preset files are read in lexicographic order and the first matching line wins, so
the number *is* the mechanism.

Then `systemctl preset-all --root=<staging>` materializes the `.wants` symlinks offline, with
no running systemd and no PID 1 in the staging root to talk to.

### 9.3 Where files land

| Content | Path |
| --- | --- |
| Units Kiln ships | `/usr/lib/systemd/system/` (image content, not `/etc`) |
| Masks | `/etc/systemd/system/` (the same place `systemctl mask` puts them, so `systemctl unmask` on the booted machine works normally) |
| The preset | `/usr/lib/systemd/system-preset/20-kiln.preset` |

A mask lands in `/usr/etc` after normalization and is 3-way merged at deploy, which means the
administrator can take it back.

### 9.4 Naming a unit nothing provides is a hard error

Unit state is image content. Naming a unit that nothing in the image provides fails the build,
in all three directions, with a near-miss suggestion. A typo in `enable` fails rather than
producing an image where the service silently is not running.

### 9.5 Verification

`systemd-analyze verify --root` runs over the units Kiln shipped. Its complaints are recorded
as **warnings**, not errors: it legitimately objects to units whose `Requires=` target lives in
a package it cannot see resolved, and failing the build on that would make the feature
unusable.

### 9.6 `systemctl enable` on the running machine is drift

Enabling a unit by hand writes a symlink into `/etc` that outlives every future generation's
preset. It will keep winning, forever, over any preset you build later. `kiln status` reports
it. Put it in `[systemd] enable` instead. See [section 11.5](#115-etc-drift).

---

## 10. Kernels and kernel modules

Four different things, four different keys.

```toml
[kernel]
package = "linux"                         # which kernel package
headers = false                           # see §6.7
dkms    = ["nvidia-open-dkms"]            # DKMS sources, compiled into the image

[kernel.modules]                          # in-tree modules, just configured
load      = ["v4l2loopback"]              # -> /etc/modules-load.d/kiln.conf
blacklist = ["nouveau"]                   # -> /etc/modprobe.d/kiln.conf
initramfs = ["i915"]                      # put these in the initramfs
options   = { v4l2loopback = "devices=2 exclusive_caps=1" }   # -> /etc/modprobe.d/kiln.conf

[[kernel.module]]                         # out-of-tree, built from source with make
name   = "my-module"
source = "kernel/my-module"
```

### 10.1 Which kernel

Kiln finds the kernel by the Arch convention: the directory under `/usr/lib/modules` that has
a `pkgbase` file. Package-shipped module directories from out-of-tree modules do not have one,
which is exactly what distinguishes them. Finding two is an error, not a guess, which is why
including two `@kiln/kernel/*` modules is a conflict on `kernel.package` reported against both
files.

Every shipped profile already includes a kernel module. To run a different kernel, copy a
profile's include list into your own configuration and swap the `@kiln/kernel/*` line; a
profile is five lines.

### 10.2 The initramfs

dracut, non-hostonly, reproducible, with the upstream `ostree` module:

```text
dracut --force --no-hostonly --no-hostonly-cmdline --reproducible \
       --kver <version> --add ostree /usr/lib/modules/<version>/initramfs.img
```

It runs **inside the staging root, against the staging root's kernel and modules**, never the
host's. An initramfs built from the host's kernel is an image that boots on exactly one
machine.

The result is then **verified** rather than trusted: `lsinitrd` must show
`ostree-prepare-root`. A silently-absent `50ostree` module produces an image that boots to an
emergency shell, which is the most expensive failure in the pipeline and the one furthest from
its cause.

`kernel.dracut_modules` adds modules beyond `ostree`. It is needed rather than optional for
things like Plymouth, whose dracut module is not pulled in just because the package is
installed.

`kernel.modules.initramfs` is different: those are *drivers* (dracut's `--add-drivers`). A
non-hostonly initramfs carries what is needed to reach the root filesystem and little else, so
a GPU driver is absent unless named there. `kernel.modules.load` is a statement about the
booted system and would be far too late for anything the initrd needs, so the two lists stay
separate. After generation, Kiln greps the initramfs listing and fails the build if a driver
you named is missing and is not built into the kernel.

`kernel.modules.load`, `blacklist` and `options` are written to `/etc/modules-load.d/kiln.conf`
and `/etc/modprobe.d/kiln.conf` respectively; see [section 6.7](#67-key-reference) for the exact
format.

### 10.3 Out-of-tree modules

```toml
[[kernel.module]]
name   = "my-module"
source = "kernel/my-module"
```

A plain Makefile tree. Kiln synthesizes a PKGBUILD-equivalent recipe and runs it through the
normal sandbox, so modules get the same caching, isolation and failure reporting as everything
else. There is no separate module builder.

The synthesized recipe declares `<kernel>-headers` as a makedepend and finds the kernel version
by looking for the headers in the build root, rather than being told it. The build root holds
exactly one `linux-headers`, installed from the same snapshot the image resolved from, so it
cannot disagree with the root it is running in.

The resolved kernel EVR is part of the build key, which is what makes "rebuild modules when
the kernel changes" fall out of the cache rather than being a special case. `kiln check`
reports it as `(kernel changed)` before you build.

### 10.4 DKMS

A DKMS package (`nvidia-open-dkms`, `v4l2loopback-dkms`, most of the AUR's drivers) ships no
compiled module. It ships sources under `/usr/src` and expects `dkms` to compile them *on the
machine, at install time, against the running kernel*. An immutable image has no such moment:
no install time on the target, no headers in the image, nothing writable under
`/usr/lib/modules` to write the result into.

`kernel.dkms` moves that moment into the build. Sources come from a **package** or from a
**tree of your own**, and after they arrive the two are identical:

```toml
[kernel]
dkms = ["nvidia-open-dkms"]               # a package: shorthand for { name = "..." }
```

```toml
[[kernel.dkms]]                           # a tree in your config, with a dkms.conf
name   = "my-driver"
source = "kernel/my-driver"
```

Either way Kiln puts the sources where `dkms` can reach them (a package into a **build root**
that is never the image; a tree copied into the build), runs `dkms build` there against the
exact kernel the plan resolved, and packages the resulting `.ko` files as `<name>-modules`.
That package is what goes into the image, so what ships is the driver and not the sources that
produced it. The image never has `dkms` installed and never runs it.

The build root sets `NoExtract` over the three `dkms` alpm hooks, which would otherwise compile
the module a second time as root, inside the transaction, before Kiln compiles it in a sandbox.

It is a build like any other: the same sandbox, the same build cache, the same failure report.
Bump the kernel and every DKMS driver rebuilds, because the kernel's version is in the build
key. Edit a file in your own tree and it rebuilds too, because the tree's digest is part of
`config_id`.

#### A DKMS tree of your own

The directory needs a `dkms.conf` at its root, and that file is the driver's own, the same one
you would hand to `dkms` on a mutable system:

```text
kernel/my-driver/
├── dkms.conf         PACKAGE_NAME, PACKAGE_VERSION, BUILT_MODULE_NAME[0], …
├── Makefile
└── my-driver.c
```

`name` is only a label; the driver names itself in `PACKAGE_NAME`, and that is what `dkms`
builds under. A tree with no `dkms.conf` is refused before a build root is assembled, with a
message pointing at `[[kernel.module]]`, which builds a plain Makefile tree with `make`. That
is the real difference between the two keys: a tool, not a preference.

#### Things worth knowing

**Prefer a prebuilt package when one exists.** `nvidia-open` is the same driver with the
compile already done. `@kiln/gpu/nvidia-open` names it, and `@kiln/gpu/nvidia-open-dkms` is for
kernels Arch ships no prebuilt module for (`linux-zen`, `linux-hardened`, `linux-rt`).

**A DKMS package from the AUR has to be in `packages.aur` too.** `kernel.dkms` says what to
build modules from; `packages.aur` is what tells Kiln to build the package itself first. A name
that resolves in neither place is an error at `kiln check`, pointing at the line that wrote it.

**One spelling per file.** TOML will not accept `dkms = [...]` and `[[kernel.dkms]]` in the
same document; that is a duplicate key. Use one form per file. Entries from different files
union, so a profile's packages and your own tree compose without either knowing about the
other.

### 10.5 Firmware and microcode

Firmware and CPU microcode are ordinary packages. `@kiln/hardware/firmware`,
`@kiln/hardware/intel-ucode` and `@kiln/hardware/amd-ucode` name them. Kiln does not treat
them specially.

---

## 11. OSTree and deployments

### 11.1 Why OSTree

OSTree gives four things Kiln needs and would otherwise have to invent: a content-addressed
store of complete filesystem trees, atomic deployment swaps, hardlink-based deduplication
between generations, and a 3-way merge of `/etc` at deploy time so a machine keeps its own
identity across image changes.

Kiln talks to libostree through its GObject-introspection bindings, not by shelling out to
`ostree(1)`. The two decisions this makes (which commit is which generation, and which
deployment boots next) are exactly the ones where a changed output format becomes a wrong boot.

### 11.2 The mapping

| Kiln concept | OSTree reality |
| --- | --- |
| Image | A ref, `kiln/<image.name>/<image.arch>`, with a linear history |
| Generation | A commit, with the number in its metadata |
| The build record | `kiln.record` in commit metadata (zstd JSON), and `/usr/lib/kiln/record.json` in the tree |
| The manifest | `kiln.manifest` in commit metadata, likewise |
| Deployment | A checked-out commit under `/ostree/deploy/kiln`, with a BLS entry |
| Rollback | Reordering the deployment list |

The record is in metadata *and* in the tree on purpose. Metadata is readable without a
checkout, which is what makes `kiln list` and `kiln check` fast; the in-tree copy survives an
export to a tarball or an inspection mount, where metadata does not.

Commit metadata also carries `plan_id`, `config_id`, the generation number, the image name and
arch, the build timestamp, and the hostname that built it (because `kiln diff` between two
generations built on different machines is a question people ask).

### 11.3 Booting

libostree writes BLS entries, one per deployment, and keeps `/boot/loader` as a symlink pair so
entry swaps are atomic. Boot order is the **inverse** of the entry filenames: the deployment
that boots is the entry with the highest BLS `version`, which is the highest-numbered file.

GRUB2 reads them through libostree's `grub2` backend, which runs `grub-mkconfig` *chrooted into
the deployment* with a host-absolute output path. That constraint is why the grub2 backend
cannot run against a sysroot that is not `/`, and it shapes how `kiln apply` and `kiln deploy`
are structured. Under `--sysroot`, BLS entries are written and no `grub.cfg` is generated;
that is expected, and the installer runs `grub-install` itself.

If a deployed image ships no `grub`, Kiln warns: BLS entries are written, nothing regenerates
`/boot/grub/grub.cfg`, and automatic rollback is off. `@kiln/boot/grub2` turns that on.

### 11.4 Deploying

```console
$ sudo kiln apply       # build, commit, deploy
$ sudo kiln deploy 41   # deploy an existing generation
$ sudo kiln rollback    # deploy the previous one
```

Kargs come from the generation's **own** commit metadata, not from `/etc/kiln`. That is not a
convenience: kargs are fully declarative, so deploying with the wrong set (or none) produces a
machine that boots once or not at all, and the only set that is right is the one that
generation was built from.

### 11.5 `/etc` drift

This is the one way a Kiln system can lie about itself, so it is worth understanding.

A file Kiln ships to `/etc` lives in the commit as `/usr/etc`. At every deploy, OSTree 3-way
merges the new commit's `/usr/etc` with your live `/etc`. That merge is what lets your machine
keep its own `fstab` and its own users across generations, and it is also what makes a
hand-edit permanent.

> If you edit a file in `/etc` that the image ships, the merge treats your version as a local
> modification and keeps it. **Forever.** Every future generation's version of that file loses,
> including one you built specifically to change it.

Rebuilding does not fix it. Rolling back does not fix it. `kiln diff` shows the change you
asked for, correctly, and the machine still has the old contents.

So `kiln status` reports it:

```text
/etc        3 local changes to files the image ships
            M /etc/motd   ← a [[file]] in this configuration
            M /etc/pacman.conf
            D /etc/issue
            plus 4 files the image does not ship, shadowing nothing

            OSTree 3-way-merges /etc at deploy, so these win over every
            future generation — including one built to change them. Put a
            file back under Kiln's control by restoring the image's copy:
              cp /usr/etc/<path> /etc/<path>
            The [[file]] entries above are the sharp case: editing the
            configuration and rebuilding will not change them on this
            machine until the live copy is restored.
```

Three things to read out of that:

- **A `[[file]]` you wrote down is the sharp case** and is always named in full, however long
  the list gets. You edited the config, Kiln built the file into the image, and the merge threw
  it away.
- **A locally created file shadows nothing** and is only counted; the image never had an
  opinion about that path. `kiln status --verbose` names them.
- **`systemctl enable` shows up here**, because unit state is image content and the symlink it
  writes outlives every future preset.

The report distinguishes what differs: contents, mode, owner, or the kind of file. A file whose
bytes are identical and whose mode is not says so, because otherwise it reads as a false
positive.

Files that a correct machine changes by itself are never reported: `machine-id`, the account
files, `fstab`, `crypttab`, SSH host keys, `resolv.conf`, `ld.so.cache`, and `/etc/kiln`
itself. That list is short and fixed on purpose. Each entry is something Kiln permanently gives
up the ability to warn about, and a warning nobody can act on is one people learn to scroll
past, including on the day it says something real.

There is no `kiln reset`. Deleting a file you chose to edit is destructive with no undo, and
the fix is a `cp` away.

### 11.6 Building into an unmounted target

Kiln has no `install` verb and no opinion about your disk. Partitioning, formatting, an initial
account, and a bootloader install onto media you are not currently running from are somebody
else's job. What Kiln gives that job is a seam: `--sysroot` and `kiln sysroot init` let `build`
and `deploy` target a mounted root other than `/`.

```console
# the installer partitions, formats and mounts the target at /mnt
$ kiln sysroot init /mnt                              # Kiln's storage, like `git init`
$ cp -r myconfig /mnt/etc/kiln
$ kiln build  --sysroot /mnt --config /mnt/etc/kiln
$ kiln deploy --sysroot /mnt 1                        # build does not deploy
$ grub-install --efi-directory=/mnt/boot/efi …        # the installer's job, not Kiln's
```

That order matters, and Kiln enforces it rather than trusting you to read it: building into a
target that was never `sysroot init`ed succeeds and then fails at deploy, so `build` warns when
its target is not initialized. If it happened anyway, nothing is lost: the commits are in the
repository with their generation numbers, and initializing the sysroot makes them deployable.

`grub-install` runs inside the deployment, so the binaries it needs have to be in the image
rather than on the installer's live medium. `@kiln/boot/grub2` installs both: `grub`, for the
`grub-mkconfig` every later deploy re-runs, and `efibootmgr`, which `grub-install` execs to
write the UEFI boot entry. `efibootmgr` is only an *optional* dependency of `grub`, so an image
that names neither builds and deploys perfectly and then cannot be made bootable.

One thing the installer must write into the configuration, because only it knows the answer:

```toml
[kernel]
cmdline = ["root=UUID=…", "rw"]
```

State lives under the sysroot, not at an absolute path: `--sysroot /mnt` puts the artifact
store at `/mnt/var/lib/kiln`, so half the operation cannot end up on the wrong machine.

**Disk encryption follows the same seam.** `cryptsetup luksFormat` and the resulting
`/etc/crypttab` entry are the installer's job, for the same reason partitioning is: Kiln does
not touch storage layout, and `/etc/crypttab` is one of the files `kiln status` never reports
on (§11.5) — whatever the installer writes there is left alone by every future deploy. What
the installer's generated configuration adds, alongside `root=UUID=…`, is the kernel side of
unlocking that root:

```toml
[kernel]
cmdline        = ["rd.luks.uuid=…", "rd.luks.name=…=root", "root=/dev/mapper/root", "rw"]
dracut_modules = ["crypt"]
```

---

## 12. Generations

### 12.1 Numbering

Generations are assigned at commit time and are monotonic. The number means the same thing
forever. OSTree deployment indices renumber as deployments come and go (today's index 1 is
tomorrow's index 0), which makes `kiln rm 1` a footgun, so Kiln never accepts one. Typing a
non-numeric generation gets that explanation rather than a parse error.

A generation exists as soon as it is **committed**. `kiln build` produces one without deploying
it; `kiln show <gen>` and `kiln rebuild <gen>` work on it, and `kiln deploy <gen>` will deploy
it. `kiln why` and `kiln owns` need it deployed, because they read a checked-out tree.

### 12.2 The baseline

**Generation 1 is the baseline** and is protected. It is a generation known to have booted on
this exact hardware, which is what automatic rollback needs a floor of. `kiln clean` and
`kiln rm` will not take it without `--remove-baseline`.

The listing says "baseline" rather than "pinned", because a listing that said "pinned" about
generation 1 would invite a `kiln unpin 1` that does not mean what the person typing it thinks.

### 12.3 Automatic rollback

A generation staged by `kiln apply` boots on probation:

1. Deployment arms a counter in GRUB's `grubenv` at 3 attempts.
2. `/etc/grub.d/09_kiln_boot_counter` decrements it on each boot and selects the rollback entry
   at zero. It is a ladder of comparisons rather than arithmetic, because GRUB's script language
   has no arithmetic and Arch's GRUB has no increment module.
3. `kiln-boot-success.service` runs `/usr/lib/kiln/boot-success` after `boot-complete.target`
   and clears the counter.

A generation that fails to come up three times is demoted; the machine boots the previous one
by itself, with no rescue USB. `kiln status` says so:

```text
boot        generation 44 failed to boot 3 times and was demoted;
            you are running generation 43. `kiln deploy 44` tries it again.
```

All three pieces are image content written by assembly, not decisions anybody makes in TOML.

### 12.4 Comparing and inspecting

```console
$ kiln diff 42 43            # between two generations, from their commits
$ kiln diff                  # booted vs pending
$ kiln show 42               # what generation 42 was asked to contain, plus its record
$ kiln why mesa 42           # ask a past (deployed) image
```

None of `diff`, `why`, `owns` or `show <gen>` reads `/etc/kiln`. They read the generation's own
commit, which is the point: the usual reason to ask is a generation whose configuration has
since been edited.

### 12.5 Cleanup and disk

```console
$ kiln clean --dry-run       # see the decision
$ kiln clean                 # keep 3 + the baseline + anything pinned + what is booted
$ kiln clean --keep 5
$ kiln pin 42                # keep this one regardless
```

`kiln clean` trims the artifact cache as well as the deployments, because they are the same
question. The cache budget is `min(20 GiB, 10% of the filesystem)`; neither number is right
alone, since 20 GiB is most of a 32 GiB disk and 10% of a 4 TiB one is 400 GiB of packages
nobody wants. A trim stops the moment it is under budget rather than clearing everything.

A build needs roughly twice the image size free, because the staging root is a full second copy
and the commit writes a third partial one. Kiln checks before starting rather than failing
halfway through a transaction, and it is a **warning** rather than a refusal: the estimate is
an estimate, and a wrong estimate that refuses to build is worse than a mid-build failure you
were warned about.

Failed staging roots are removed; `--keep-failed` keeps one for inspection.

### 12.6 What is stored per generation

| Where | What |
| --- | --- |
| Commit metadata | `kiln.version`, `kiln.plan-id`, `kiln.config-id`, `kiln.generation`, `kiln.image`, `kiln.arch`, `kiln.built-at`, `kiln.built-by`, `kiln.record`, `kiln.manifest`, plus libostree's own `ostree.bootable` (without which the deployment gets no BLS entry and the machine silently boots the previous generation) |
| The tree | `/usr/lib/kiln/manifest.json`, `/usr/lib/kiln/record.json`, `boot-success` |
| The record | Repo/AUR/built/local packages with versions and checksums, hashed local files, script texts *and* their changeset hashes, the UID map and the UID seed, the resolved snapshot date |

Two UID maps are recorded, and they answer different questions: `uid_map` is what the next
generation replays, and `uid_seed` is what this one replayed. Without the seed, `kiln check`
could not explain the one case where two builds of an unchanged configuration legitimately
differ (generation 1 allocates ids freely; generation 2 pins them).

---

## 13. The build pipeline

### 13.1 End to end

```text
TOML
  ↓  discover the entry point, enforce the config root boundary
Parse into a spanned Node tree
  ↓  expand shorthand
Structure checks (schema version, include placement, unknown keys, types)
  ↓
Resolve the include graph (cycles, depth cap, dedup)
  ↓
Merge (lists union · includer wins · siblings conflict)
  ↓
Validate into a Manifest, hashing every local file            ⇒ config_id
  ↓
Resolve: syncdb refresh · solve · AUR RPC · checksums · build keys
  ↓                                                            ⇒ plan_id
  ├─ plan_id == the deployed one?  stop, unless --force
  ↓
Realize: clone · makepkg phase 1 (network) · phase 2 (no network) · download
  ↓                                                            ⇒ artifact store
Assemble: 11 steps into a staging root
  ↓
Normalize: /etc→/usr/etc · /var drain · usr-merge · initramfs
  ↓
Commit to OSTree with the record and the manifest in metadata
  ↓
Deploy (kiln apply only): BLS entry · grub.cfg · arm the boot counter
```

### 13.2 Resolution in detail

Resolution refreshes libalpm's sync databases (building Kiln's own pacman keyring first if the
repositories require signatures), runs the solver, and then, in one pass that reports
everything wrong at once:

1. **Local packages**: verifies each `packages.file` against its declared `sha256`, fetching a
   checksum URL if given.
2. **Recipes**: reads each `packages.build` directory's `.SRCINFO`.
3. **Modules**: collects `[[kernel.module]]` trees.
4. **AUR**: resolves the closure through the RPC, pinning commits.
5. **Build keys**: resolves each build's `makedepends` closure against the same repository
   snapshot as the image, then computes `build_key` for every recipe, module and DKMS driver.

The plan is then canonicalized: inputs sorted by variant and identity, so two resolutions of
the same configuration hash the same regardless of what order libalpm answered in.

**`Provenance` is deliberately outside `plan_id`**: in rolling mode every build resolves on a
different date, and if the date were hashed, `kiln build` could never say "nothing to do". The
date is still recorded, because it is what makes a past image reconstructible.

### 13.3 Realization

The only network in a build, and it all happens before assembly starts.

```text
1. build   AUR clones, PKGBUILDs, kernel modules, DKMS drivers
             phase 1: makepkg --verifysource, network ON
             phase 2: makepkg, network OFF, sources bind-mounted
2. fetch   download every repository package into the artifact cache
```

Building comes before fetching, and that order is load-bearing: an AUR package's runtime
dependencies are named nowhere in the plan (the closure stops wherever the official
repositories can satisfy one), so they are only discoverable from the artifact itself. `fetch`
is handed the built packages and lets libalpm resolve what they need.

Assembly's artifact list is the promise that everything is already here.

### 13.4 Assembly, step by step

```text
 1 skeleton          usr/lib/sysimage/pacman and the mountpoints, nothing else
 2 base transaction  `filesystem` alone, so step 3 has an /etc/passwd to seed
 3 UID seed          replay the previous generation's ids
 4 transaction       everything else, package hooks shadowed
 5 scripts           after = "packages" — an overlayfs changeset
 6 overlay           [[file]], checked against the pacman file database
 7 unit state        presets, masks, systemctl preset-all --root
 8 scripts           after = "files" — an overlayfs changeset
 9 kernel            depmod, initramfs, /boot cleared
10 normalize         /etc, the /var drain, the top level
11 self-description  usr/lib/kiln/{manifest.json,record.json}
```

Every step depends on the one before it, and several of the orderings are load-bearing:

- **Steps 2 and 4 are split** because `filesystem` owns `/etc/passwd` and `/etc/group`. Seeding
  pinned IDs before it is installed makes pacman abort on a file conflict, and `--overwrite`
  would let the stock files clobber the pins.
- **Step 3 replays UIDs.** Arch allocates service-account IDs first come, first served, and
  nothing makes the order stable across generations. `systemd-journal` moving from gid 972 to
  973 means every file in the persistent `/var/log/journal` is owned by a group that no longer
  means what it did, and rolling back does not fix it because `/var` does not roll back. So the
  IDs a generation ends up with are captured and seeded into the next one, as a `sysusers.d`
  fragment processed before any package's own. The seed carries `home` and `shell` as well as
  the numbers, because whatever it does not say, it decides by omission.
- **Step 4 shadows hooks.** Package alpm hooks always run; `--hookdir` does not suppress them,
  and the only lever is same-filename shadowing. Kiln shadows the ones that write runtime state
  or that it owns (`21-systemd-tmpfiles`, the dracut pair, and others) and keeps the rest,
  because locale-gen, ldconfig, sysusers, ca-trust, hwdb and the rest write legitimate image
  content. Scriptlets get the same treatment through a shim directory on pacman's `PATH`, and
  every shimmed call is logged.
- **Steps 5 and 8 are two slots, not one**, because a script that needs `[[file]]` content
  already in place and a script that has to run before it are different jobs. `after` makes you
  say which.
- **Step 10's internal order** is also fixed: `pacman.conf` and `machine-id` are fixed while
  still in `/etc`; then `/root`, `/home`, `/opt` and `/srv` move into `/var`; then the drain
  runs over everything including what just moved; then `/etc` becomes `/usr/etc`; then the top
  level is rewritten with the usr-merge symlinks. Doing the move after the drain leaves `/home`
  pointing at nothing; moving `/etc` before fixing `pacman.conf` means the booted image's
  `pacman -Q` reports nothing and every machine deployed from it shares a machine-id.

### 13.5 Build scripts

`[[script]]` is the escape hatch, for things the schema has no key for.

```toml
[[script]]
source = "scripts/20-locale.sh"
after  = "files"            # or "packages"
```

A script runs chrooted in the image being assembled, with **no network, ever**, and its effect
is captured as an **overlayfs changeset**:

```text
lowerdir = the staging root, read-only from the script's point of view
upperdir = empty when the script starts; afterwards, exactly its changes
merged   = what the script sees as `/`, chrooted, with no network
```

The upper layer *is* the set of changes it made. That is not an optimization over diffing the
tree; it is what makes visibility, conflict detection, ownership reporting and the determinism
audit affordable at all. Kiln can report a script that overwrites a package-owned file, and it
hashes the changeset into the record so `kiln rebuild` can tell you which script is not a pure
function of its inputs.

The staging root is never what the script wrote to, so a script that fails leaves it untouched.

Use scripts sparingly. Anything you find yourself writing a script for twice is probably a key
the schema should have, or a module.

### 13.6 The sandbox

Two backends behind one trait: **bubblewrap** (the default for builds and for dracut) and
**systemd-nspawn**. It is a namespace sandbox, not a VM: a kernel privilege escalation escapes
it, and Kiln says so plainly rather than implying otherwise.

Every spec is explicit: the root the command sees as `/`, the bind mounts (no implicit host
access), the network (`Disabled` by default, and that default is the constraint the rest of the
model rests on), the user, a cleared-then-populated environment, shims, resource limits, and a
log path. The full log is always written and its path is printed on failure, because a failing
run comes back carrying only the last forty lines.

Tests assert on the exact argv each backend produces, not merely on the spec, because a spec
that says `Network::Disabled` and a backend that forgets `--unshare-net` is exactly the failure
a spec-only test misses.

### 13.7 Committing

The commit carries the record and the manifest as zstd-compressed JSON in its metadata, plus
`ostree.bootable` (libostree's own key, and what makes the deployment get a BLS entry). The
generation number is decided once and used for both copies of the record, the one the assembler
writes into the tree and the one that goes into metadata; computing it twice is how they came
to disagree.

---

## 14. Includes, composition, and the module library

### 14.1 `include`

`include` is the only composition operator. Two forms:

| Form | Resolves to |
| --- | --- |
| `"hardware.toml"` | Relative to the **including file's** directory |
| `"@kiln/desktop/gnome"` | `/usr/share/kiln/modules/desktop/gnome.toml` |

Rules:

- Top-level key, **before any table header**.
- Including the same file twice is a no-op.
- A cycle is a hard error that prints the cycle.
- Depth is capped at 32. A configuration that deep is almost always a cycle that dodged
  detection, or a module library that wants flattening.
- Remote includes (`git+…`, anything with `://`) are refused: an unpinned supply-chain input
  that breaks offline builds. Vendor the module into your tree instead.
- Relative includes must resolve inside the config root.

### 14.2 Composition in practice

A file's contribution is *its includes merged as siblings, then its own content laid over the
top*. So:

```toml
# system.toml
kiln = 1
include = ["@kiln/profiles/workstation", "@kiln/desktop/gnome"]

[boot]
timeout = 0            # wins over anything either include sets
```

If the two includes disagreed about `boot.timeout`, that would be an error naming both files
and lines, and setting it here is the fix.

### 14.3 The module library

Kiln ships 61 modules under `/usr/share/kiln/modules`:

```text
@kiln/profiles/    minimal · workstation · server
@kiln/kernel/      linux · linux-lts · linux-zen · linux-hardened · linux-rt
@kiln/boot/        grub2 · plymouth
@kiln/net/         networkmanager · systemd-networkd · nftables · sshd · iwd · tailscale
@kiln/gpu/         nvidia-open · nvidia-open-lts · nvidia-open-dkms · nvidia-cuda
                   amd · amd-rocm · intel
@kiln/desktop/     gnome-minimal · gnome · gnome-full
                   plasma-minimal · plasma · plasma-full
                   xfce-minimal · xfce
                   cosmic-minimal · cosmic
@kiln/wm/          hyprland · sway · niri · i3
@kiln/audio/       pipewire
@kiln/hardware/    firmware · bluetooth · printing · laptop · intel-ucode · amd-ucode
@kiln/virt/        libvirt · podman · docker · nvidia-docker · distrobox · lilipod
@kiln/dev/         base-devel · rust · go
@kiln/security/    wheel-sudo · apparmor
@kiln/system/      zram · swapfile
@kiln/terracotta/  kiln · installer · branding · branding-plymouth
```

A module is a small TOML file with no magic in it. This is the whole of
`@kiln/gpu/nvidia-open-dkms`, comments aside:

```toml
kiln = 1

[packages]
repo = ["nvidia-utils"]

[kernel]
dkms    = ["nvidia-open-dkms"]
cmdline = ["nvidia_drm.modeset=1"]

[kernel.modules]
blacklist = ["nouveau"]
```

### 14.4 The four rules

All four are enforced by `crates/kiln-config/tests/modules.rs` in CI:

1. **One module, one decision.** `@kiln/gpu/nvidia-open` installs the driver and stops. No
   CUDA, no Vulkan layers, no control panel; those are separate decisions.
2. **Only profiles compose**, one level deep. Everywhere else the graph is flat, so including
   one module cannot silently drag in nine files.
3. **A module fits on one screen.** Hard cap, 25 lines including comments. A module that needs
   more is making more than one decision.
4. **Every unit a module enables comes from a package that module installs.** Checked against
   the host's pacman file database, skipping with a message where there is none.

A fifth, unenforced but real: a module ships a file only when the thing does not work without
it. Opinions about how you would like it configured are not Kiln's to ship.

### 14.5 Profiles

A profile is the one place opinions live, and it is honest about it: a list of includes plus
the packages that are nobody's decision.

| Profile | Kernel | Includes | For |
| --- | --- | --- | --- |
| `minimal` | `linux` | grub2, firmware | The smallest image that boots and reaches a shell. No network stack, because a machine reachable over the network is a decision |
| `workstation` | `linux` | grub2, firmware, bluetooth, printing, NetworkManager, pipewire, wheel-sudo | A desktop machine. No graphics driver and no desktop; those depend on your card and your taste |
| `server` | `linux-lts` | grub2, firmware, systemd-networkd, nftables, sshd, wheel-sudo | A machine nobody sits in front of: reachable, firewalled, on the kernel that changes least |

All three install `base`, `dracut`, `ostree` and `systemd-sysvcompat`, and enable
`systemd-timesyncd.service`. `ostree` is not optional: its dracut module ships inside it, and
`kiln status` on the booted machine has nothing to read without it.

**Every profile already includes a kernel module**, so a profile plus `@kiln/kernel/linux-zen`
is a conflict on `kernel.package`, by design. To run a different kernel, copy the profile's
include list into your own configuration and swap that one line.

### 14.6 Making a module your own

If a module is not quite what you want, copy it into your tree and edit it:

```toml
include = ["modules/my-nvidia.toml"]
```

That is a supported workflow, not a workaround, and yours wins over anything it includes.

---

## 15. Errors and troubleshooting

### 15.1 How diagnostics read

Kiln's diagnostics name the file *and line* of everything, including inside shipped modules:

```text
kiln::merge

  × conflicting values for `boot.timeout`
   ╭─[desktop.toml:4:11]
 3 │ [boot]
 4 │ timeout = 5
   ·           ┬
   ·           ╰── set to 5 here
   ╰────
  help: `desktop.toml` and `gaming.toml` are both included by `system.toml`. Set
        `boot.timeout` in `system.toml` to resolve it — the includer always wins.
  × also in gaming.toml
   ╭─[gaming.toml:4:11]
 3 │ [boot]
 4 │ timeout = 0
   ·           ┬
   ·           ╰── and to 0 here
   ╰────
```

Every phase reports all of its problems before the next one starts.

### 15.2 Configuration errors (exit 1)

| Symptom | Cause and fix |
| --- | --- |
| `no configuration at /etc/kiln/system.toml` | Nothing there. Run `kiln init`, or pass `--config` |
| `` missing `kiln` schema version `` | Every Kiln file starts with `kiln = 1` |
| `` `kiln` must be the first key in the file `` | Move it to the top. Checked by position, because a reader scanning the top of the file is why the rule exists |
| `unsupported schema version 2` | This build understands `kiln = 1` |
| `` `include` is in the wrong place `` | You wrote `include` after a `[section]` header, so TOML made it a key *of that section*. Move it above every table header |
| `` unknown key `x` in `packages` `` | A typo. The message suggests the near miss, or lists what Kiln knows there |
| `` `boot.timeout` must be an integer, found string `` | Drop the quotes. Type errors are reported all at once |
| `` `0777` is not a file mode `` / `` `mode` must be a string `` | Three or four octal digits, as a string: `mode = "0755"` |
| `` `backup` is not a systemd unit name `` | Write `backup.service`, or whichever suffix you meant |
| `` this file has both `source` and `content` `` | Pick one; Kiln will not guess |
| `Kiln cannot ship a file to /boot/...` | See the routing table in [section 8.2](#82-where-a-target-may-land) |
| `include cycle` | The message prints the cycle |
| `` no such module `@kiln/desktop/gnme` `` | Did-you-mean, or a list of what the library has. If the library is empty, pass `--module-root ./modules` |
| `remote includes are not supported` | Vendor the module into your config tree |
| `include escapes the config root` | Everything the image is built from must live inside the config root. `--allow-external-sources` overrides, and warns |
| `` conflicting values for `x` `` | Two siblings disagree. Set the key in the includer |
| `systemd-boot cannot boot an OSTree system` | Remove the line; `grub2` is the only value |
| `mkinitcpio cannot build an initramfs that boots an OSTree system` | Remove the line; `dracut` is the only value |

### 15.3 Resolution failures (exit 2)

| Symptom | Cause and fix |
| --- | --- |
| `` no kernel in this image: nothing installs `linux` `` | Add the kernel to `packages.repo`, or include `@kiln/profiles/minimal` |
| `no init in this image` | Nothing provides `init`. Same fix |
| A package name that resolves nowhere | The message underlines the element itself, not the whole array, and suggests near misses from every package name the repositories hold |
| `` `pkgbuilds/x` has no .SRCINFO `` | `cd pkgbuilds/x && makepkg --printsrcinfo > .SRCINFO` |
| `` `pkgbuilds/x` has no PKGBUILD `` | There is nothing to build there |
| `` `x` is not the file its `sha256` describes `` | The message gives the actual digest. Update the line if the change was intended; if it was not, something replaced a package Kiln was about to install |
| `` could not fetch the checksum for `x` `` | A `sha256` URL that did not answer |
| `` `x` has an unsupported URL scheme `` | Only `http://` and `https://` |
| `` no package named `x` to build DKMS modules from `` | A `kernel.dkms` entry with no `source` names the *package* that ships the DKMS sources (`nvidia-open-dkms`, not `nvidia-open`). One from the AUR must be in `packages.aur` too |
| `` `x` cannot be built `` | Its `makedepends` do not resolve against the same repositories as the image |
| `failed to retrieve some files` on a fresh machine | The pacman keyring is being built; Kiln does this automatically the first time repositories require signatures |

### 15.4 Volatile inputs

```text
  2 inputs could not be checked without fetching:
    foo-git, bar-git     (VCS packages; run `kiln check --deep`)

  `kiln check --deep` fetches them and answers precisely.
```

A VCS package's version comes from running `pkgver()` against upstream, and a `SKIP` checksum
states nothing. Kiln will not guess, so those inputs are excluded from `plan_id` and reported
separately. `kiln check --deep` fetches and answers them; it does not build, it runs the same
`makepkg --verifysource` a build's first phase would.

A `--deep` that could not answer will not report "up to date".

### 15.5 Build failures (exit 3)

The staging root is removed on success and on failure. Keep it for inspection:

```console
$ sudo kiln build --keep-failed
```

A failed package does not take unrelated packages down with it; its dependents are reported as
skipped rather than blamed. Sandbox failures print the path to the full log, which is always
written.

Common causes:

- **A PKGBUILD that downloads in `build()`.** The network is off in phase 2. Move the
  download into `source=()`.
- **A DKMS tree with no `dkms.conf`.** Refused before a build root is assembled, pointing at
  `[[kernel.module]]`.
- **`systemd-analyze verify` complaints.** These are warnings, not failures.
- **A unit that nothing provides**, named in `enable`/`disable`/`mask`. That one *is* a
  failure, with a near-miss suggestion.

### 15.6 System and permission errors (exit 4)

| Symptom | Cause and fix |
| --- | --- |
| `building an image needs root` | libalpm would fail every `chown`, log a warning, and report success. Use `sudo` |
| `fstatat(ostree/deploy): No such file or directory` | The sysroot was never initialized. `kiln sysroot init --sysroot <path>`. If it was already built into, nothing is lost |
| `there is no generation 44` | `kiln list` shows what this machine has. A committed-but-undeployed generation is still one it has |
| `generation 44 carries no manifest` | Built by an older Kiln. It cannot be deployed safely, because kargs are declarative and its set is unknown |
| `no deployment for generation 38` | Named a generation that is not deployed |
| Every generation named by `kiln rm` was protected | Exits nonzero on purpose, so `kiln rm 1 && …` does not proceed |

### 15.7 The new image did not boot

If you reach a shell, `kiln status` says whether the counter demoted it. If you do not, the
machine demotes it for you after three attempts and boots the previous generation.

From there:

- `kiln deploy <gen>` tries it again;
- `kiln rebuild <gen>` rebuilds a past generation from its own record, if you want the old
  *image* back rather than the old deployment;
- the previous generations are in the GRUB menu, one BLS entry per deployment. Nothing about
  them is Kiln-specific.

If automatic rollback never armed, check for the warning at deploy time: an image with no
`grub` gets BLS entries and nothing that regenerates `grub.cfg`, so nothing counts. Include
`@kiln/boot/grub2`.

### 15.8 A change to a config file is not taking effect

Check `kiln status` for `/etc` drift. If the file is listed there, your hand-edited copy is
winning over the image and will keep winning until you restore the image's version:

```console
$ sudo cp /usr/etc/<path> /etc/<path>
```

See [section 11.5](#115-etc-drift).

### 15.9 Disk pressure

```console
$ kiln clean --dry-run
```

A build warns when free space is below roughly twice the image size. `kiln clean` frees both
deployments and the artifact cache. Everything under `/var/lib/kiln` is cache and history:
deleting it costs time, never correctness.

---

## 16. Development

### 16.1 Repository layout

```text
kiln/
├── Cargo.toml               workspace, members = crates/*
├── crates/                  see §17
├── modules/                 the shipped module library → /usr/share/kiln/modules
├── tests/
│   ├── corpus/              valid and invalid configurations, snapshot-tested
│   ├── repo-fixture/        a real tiny pacman repo built in-tree
│   └── vm/                  the boot acceptance fixture
├── packaging/PKGBUILD       the Arch package, terracotta-kiln
├── .github/workflows/       release.yml: tag → makepkg → GitHub release
├── docs/GUIDE.md            this file
└── CLAUDE.md                conventions and design boundaries
```

### 16.2 Building, testing, linting

```console
$ cargo build --release
$ cargo test                          # nothing privileged
$ sudo -E cargo test -- --ignored     # transactions, assembly, scripts, ostree: need root
$ cargo clippy --all-targets          # currently zero warnings; keep it that way
$ cargo fmt
$ cargo insta review                  # after a deliberate diagnostic change
$ ./tests/repo-fixture/build.sh       # the hermetic pacman repo (tests call it themselves)
```

Privileged tests are `#[ignore]`d by default with a reason string, and **skip with a message
rather than failing** when run without root. Anything new that needs root goes the same way.

### 16.3 Running locally

```console
$ cargo run --bin kiln -- --config <dir> --module-root ./modules check --offline
$ cargo run --bin kiln -- --config <dir> --module-root ./modules explain boot.timeout
```

`tests/corpus/valid/workstation` is a realistic configuration to point at.

### 16.4 The testing model

| Kind | Where | What it proves |
| --- | --- | --- |
| Snapshot tests over the corpus | `crates/kiln-config/tests/corpus.rs` | Valid configs produce the expected manifest; invalid ones produce the expected *rendered diagnostic*. When you change a diagnostic, read the snapshot diff as a user would. That is the review, not a formality |
| Property tests | `crates/kiln-config/tests/merge_algebra.rs` | Sibling order does not change `config_id` or validity; list order within a file does not change `config_id`; union is associative |
| Hash freeze | `crates/kiln-config/tests/hash_freeze.rs`, `crates/kiln-resolve/tests/hash_freeze.rs` | Committed expected `config_id`s and `plan_id`s. A refactor must not change them; see [the hash-freeze rule](#the-hash-freeze-rule) |
| Module library rules | `crates/kiln-config/tests/modules.rs` | The four rules in [section 14.4](#144-the-four-rules) |
| Solver and transaction | `crates/kiln-alpm/tests/` | Against `tests/repo-fixture`, a real tiny local repository, never the network |
| AUR | `crates/kiln-aur/tests/` | Recorded HTTP fixtures, never the network |
| Build scripts | `crates/kiln-image/tests/scripts.rs` | A **real overlayfs** with a fake sandbox: whiteouts, opaque directories and copy-up all come from the kernel, and the sandbox is a closure writing into the merged mount |
| Sandbox | `crates/kiln-sandbox/tests/` | The exact argv each backend produces, including that the build phase really has `Network::Disabled` |
| Boot acceptance | `crates/kiln-cli/tests/boot.rs`, fixture in `tests/vm/` | The only test that proves the project works, and the only one that uses the network |

#### The hash-freeze rule

These numbers are load-bearing. A hash-freeze failure has exactly three legitimate causes, and
each test's own failure message spells them out:

1. **A bug you just introduced.** The common case, and the reason the test exists.
2. **A deliberate encoding change.** Bump `HASH_EPOCH` in `kiln-manifest` and add a row to its
   epoch table saying why the change makes a genuinely different image. `kiln-resolve`'s
   freeze test also holds a `FROZEN_AT_EPOCH` that must move in the same commit.
3. **A shipped module changed**, for the fixtures that include one. That is a narrower licence
   than it looks: the module's own diff must be in the same commit, and the change has to be
   one that genuinely alters the image.

Never "fix" a failure by pasting the new values in.

#### Boot acceptance

```console
$ sudo -E cargo test -p kiln-cli --test boot -- --ignored --nocapture
```

Roughly twenty minutes and several hundred megabytes. It boots generation 1, boots generation
2, and rolls back to generation 1 in a real qemu VM, asserting every claim from inside the
running system through a probe the configuration itself ships. It skips with a message (never
fails) when root, `/dev/kvm`, qemu, `mkfs.ext4` or the network is missing.

Read `tests/vm/README.md` before touching it, particularly the part about the host and the
guest never holding the disk, or a cached view of it, at the same time.

### 16.5 Debugging

- `-v`/`--verbose` on almost every command: OSTree checksums, the full build record, every
  drifted file, per-input digests.
- `kiln explain` for anything about where a value came from.
- `kiln show` and `kiln show <gen>` for the merged manifest, on disk or from a commit.
- `sudo kiln build --keep-failed` keeps the staging root; walk into
  `/var/lib/kiln/build/<plan_id>/root`.
- Sandbox logs: the full log is always written and its path is printed on failure.
- `RUST_BACKTRACE=1` for panics, which should not happen and are bugs worth an issue.

### 16.6 Conventions that carry design weight

- **Order never matters.** Collections are `BTreeMap`/`BTreeSet` and canonically sorted before
  hashing. `files` is keyed by target path (collisions become impossible); `scripts` is keyed by
  name (ordering is content-determined, not file-order-determined).
- **Ambiguity is an error, not a coin flip.** Two included files setting the same scalar
  differently is a hard error naming both files and lines, never last-wins.
- **Spans are carried through the whole frontend**, so `kiln explain` can answer "set in
  `hardware.toml:14`, overriding `@kiln/hardware/nvidia:9`".
- **The config root is a security boundary.**
- **Network is on only during resolution and source fetching**, never during a build phase or
  a build script.
- **The build never touches the live root.** Everything happens in a staging root under
  `/var/lib/kiln`.
- **The user should never have to type `ostree`.** Users write `target = "/etc/motd"`; Kiln
  owns the `/usr/etc` translation, the `/var` drain, kernel placement and BLS entries.
- **The canonical encoding is hand-written on purpose** (`kiln-manifest/src/canon.rs`). The
  byte stream *is* the hash input, so stability must not depend on a dependency's formatting.

### 16.7 Documentation is part of the change

`README.md` and `docs/GUIDE.md` are the user-facing docs. A change to the CLI surface, the
schema, or a command's output is a change to the guide **in the same commit**. The guide quotes
real output in a dozen places, and a guide that quotes output the binary no longer produces is
worse than none.

### 16.8 Releasing

1. Bump `version` in the workspace `Cargo.toml`.
2. Bump `pkgver` in `packaging/PKGBUILD`.
3. Commit, then push the matching `vX.Y.Z` tag.

`.github/workflows/release.yml` builds the package in an `archlinux:base-devel` container with
`makepkg`, renames it to `terracotta-kiln-<arch>.pkg.tar.zst`, writes a `.sha256` beside it, and
attaches both to a GitHub release with generated notes. `@kiln/terracotta/kiln` points at
`releases/latest/download/`, so a release is immediately installable into an image.

---

## 17. Crate reference

The workspace is split along **testability** boundaries: the top crates are pure and
snapshot-testable, the bottom ones need root and run only in privileged containers.

### `kiln-diag`

`SourceFile`/`Origin`/`Spanned`/`Provenance`, the error taxonomy, `Phase`, `ExitCode`,
did-you-mean suggestions, and deterministic miette rendering.

Its own crate because error quality is a feature, and a feature with no home crate decays.
`render()` is nocolor and fixed-width so *rendered* diagnostics can be snapshotted. `SourceFile`
carries a `miette::NamedSource`, which is what makes a multi-file conflict able to name files.

**Belongs here:** anything about how a problem is described. **Does not:** anything that knows
what a Kiln key means.

### `kiln-manifest`

The canonical IR (`Manifest` and its component types), the hand-written canonical encoding
(`canon.rs`), `Hash`, `HASH_EPOCH`, `SCHEMA_VERSION`, and `config_id`.

No dependency on `kiln-config`: the IR must be readable by things that never parse TOML, such
as a commit's metadata.

**Belongs here:** the shape of an image and how it hashes. **Does not:** parsing, validation,
or anything with a span.

### `kiln-config`

The frontend. `discover` (entry point, config root, module resolution, the security boundary),
`node` (spanned generic tree, `toml_edit` parsing), `shorthand`, `structure`, `include` (the
graph), `merge` (the algebra and provenance), `schema` (the key list, list identities, scalar
types), `digest`, and `validate` (typed extraction plus semantic checks).

Merge operates on the generic tree, not on typed structs, so it stays property-testable and
every key keeps its provenance.

**Belongs here:** anything between bytes on disk and a `Manifest`.

### `kiln-alpm`

libalpm: `Session`, `Config`, the solver (`solve`), the transaction (`transact`, including
`.pkg.tar.zst` files loaded from disk), `RepoSpec`/`Trust`/`mirrors`, the keyring, mount
handling, and the `owns`/`installed_package` queries behind `kiln owns` and `kiln why`.

**Belongs here:** everything that talks to libalpm. **Does not:** anything that knows what a
Kiln manifest is.

### `kiln-resolve`

`BuildPlan`, `ResolvedInput` (the whole input taxonomy), `plan_id`, the repository set a
manifest implies, recipe reading, DKMS version resolution, build-key computation, volatile
input reporting, and `UidMap`.

**Belongs here:** cheap, networked, metadata-only work. **Does not:** anything that downloads a
tarball or unpacks one. The moment resolution needs to fetch a package to answer a question,
`kiln check` stops being cheap and starts being a build.

### `kiln-sandbox`

The `Sandbox` trait and `SandboxSpec`, with bubblewrap and systemd-nspawn backends, plus binary
shims and their logging.

**Belongs here:** isolation. **Does not:** knowledge of what is being isolated.

### `kiln-build`

Recipes, `Srcinfo` parsing, `Ingredients`/`build_key`, the build cache, the two-phase build, the
build root, synthesized module recipes (`module.rs`), and the synthesized DKMS recipe
(`dkms.rs`).

`kernel.dkms` sources are either a package installed into a build root that is never the image,
or a tree in the configuration copied into the build; `dkms build` runs there against the
resolved kernel and only the `.ko` files ship. The two differ in where `$source_tree` points and
nothing else.

### `kiln-aur`

The RPC, commit identity, the dependency closure (recursive, cycle-checked, depth-capped), and
the clone. The `Transport` trait is the seam that lets tests use recorded fixtures.

There is no AUR *builder* here: building is the ordinary PKGBUILD path in `kiln-build`.

### `kiln-image`

All eleven assembly steps plus normalization and `bootcount`. One module per step: `skeleton`,
`uid`, `hooks`, `scripts`, `overlay`, `units`, `kernel`, `drain`, `normalize`, `determinism`,
`verify`, `tree`, `assemble`.

Arch is not an OSTree distribution, and most of what could go wrong lives here. Read it before
writing image code.

### `kiln-record`

The build record: its format version, its serialization, `next_seed()`, and the conversion at
the `UidMap` boundary. The record is a *persisted* format read by a Kiln that may be older or
newer than the one that wrote it, so it has its own shapes rather than deriving `Serialize` on
in-memory types.

### `kiln-ostree`

libostree integration: `commit`, `deploy`, `generation` (commit metadata and numbering),
`entries` (BLS), `grubcfg`, `grubenv` (boot counting), the `Removal` policy `kiln rm`/`kiln
clean` are written against, and `drift` (the `/usr/etc` vs `/etc` walk).

Two libostree facts the crate inherits: there is no `rollback` verb (only `set-default`,
`undeploy` and `pin`, so `kiln rollback` is Kiln's own operation), and BLS boot order is the
*inverse* of entry filenames.

Read `kiln-image/src/bootcount.rs` and `kiln-ostree/src/grubenv.rs` before touching anything
near the bootloader.

### `kiln-cli`

The `kiln` binary. Hand-written argument parsing (`args.rs`), one module per command family
(`check`, `build`, `deep`, `deployments`, `disk`, `drift`, `explain`, `init`, `inspect`,
`realize`, `rebuild`, `show`, `completions`), and `pipeline.rs`, which is the one place
`check`, `build` and `apply` are written as the same pipeline stopped at three different points
so they cannot drift.

---

## 18. Extending Kiln

Before extending anything, check it against the scope boundaries in
[section 1.3](#13-the-philosophy). The most valuable contribution is often the one that does
not add a key.

### 18.1 Adding a configuration key

1. **`crates/kiln-manifest/src/manifest.rs`**: add the field to the right struct, give it a
   `Default` if it needs one, and add it to that type's `Canonical` impl. A new field in the
   canonical encoding changes every identity, so **bump `HASH_EPOCH`** and add a row to its
   table explaining why the change is a genuinely different image.
2. **`crates/kiln-config/src/schema.rs`**: add the dotted key to `KEYS`; add a `ListSpec` to
   `LISTS` if it is a list (with its identity key and shorthand); add its `scalar_type`; add
   `entry_keys` if it is an array of tables.
3. **`crates/kiln-config/src/validate.rs`**: extract and validate it, with a diagnostic that
   names the span and says what to write instead.
4. **`crates/kiln-cli/src/explain.rs`**: add it to `default_for` if it has a default. That
   table must match the schema's defaults exactly; a default in one and not the other is a bug
   in one of the two.
5. **`crates/kiln-cli/src/show.rs`**: show it, if a reader would want it.
6. **`crates/kiln-resolve`**: if it is an input, give it a `ResolvedInput` variant and a
   `kiln check` category. Every variant's encoding is tagged by name, so a plan containing none
   of a given kind encodes exactly as it would have before that kind existed.
7. **`crates/kiln-image`**: implement what it actually does.
8. Add a corpus case (valid and, if there is a way to get it wrong, invalid), update both
   `hash_freeze.rs` files and `kiln-resolve`'s `FROZEN_AT_EPOCH` in the same commit, and
   document it in [section 6.7](#67-key-reference).

### 18.2 Adding a CLI command

1. Add a `Command` variant in `crates/kiln-cli/src/args.rs`, parse it, and add it to the
   `known` list for did-you-mean.
2. Add it to the `HELP` string and to `completions.rs` for all three shells.
3. Dispatch it in `main.rs`. Decide deliberately whether it reads `/etc/kiln` (through
   `frontend()`) or a commit; commands that answer questions about a *generation* must read the
   commit, because the configuration has very possibly been edited since.
4. Return a meaningful `ExitCode`.
5. Add a test in `crates/kiln-cli/tests/cli.rs`, and document it in
   [section 5](#5-cli-reference).

### 18.3 Adding a module to the library

Create `modules/<namespace>/<name>.toml`. It must:

- start with `kiln = 1` and a comment saying what decision it makes and what it deliberately
  leaves out;
- stay at or under 25 lines;
- not `include` anything unless it is a profile;
- only enable units that come from packages it installs;
- ship a file only when the thing does not work without it.

`cargo test -p kiln-config --test modules` checks all of that. Then add it to
[section 14.3](#143-the-module-library) and to the README's list.

### 18.4 Adding validation

Semantic checks live in `crates/kiln-config/src/validate.rs` and push into `self.errs`
(fatal) or `self.notes` (accepted, but worth saying out loud). Notes are separate from errors
rather than a severity inside them, because `Errors::into_result` turns a non-empty set into a
failure and a note that failed the build would not be a note.

Scalar *type* errors belong in `structure.rs` instead, so a file with three of them reports all
three in one run.

Every new diagnostic wants a corpus case whose rendered output is snapshotted.

### 18.5 Adding an assembly step

Steps live one per module in `crates/kiln-image/src/`, and `assemble.rs` is the sequence. A new
step must:

- be pure where it can be (`drain::plan` is a value that can be snapshotted without root;
  `drain::apply` does the writing);
- collect every problem rather than returning the first;
- add its result to `assemble::Report`;
- be `#[ignore]`d with a reason if its test needs root, and skip with a message rather than
  failing.

Say in a comment *why* it sits where it does in the sequence. Several of the existing orderings
are load-bearing and none of them are obvious.

### 18.6 Adding a sandbox backend

Implement `Sandbox`. Refuse, loudly, anything in the spec the backend cannot enforce: silently
not applying a limit is worse than not having one, because the caller believes it is protected.
Assert on the exact argv in a test.

---

## 19. Examples

### 19.1 Minimal

```toml
# /etc/kiln/system.toml
kiln = 1

include = ["@kiln/profiles/minimal"]

[packages]
repo = []
```

This is exactly what `kiln init` writes. It boots and reaches a shell, with no network stack.

### 19.2 A workstation

```toml
kiln = 1

include = [
  "@kiln/profiles/workstation",
  "@kiln/desktop/gnome",
  "@kiln/gpu/amd",
]

[image]
name = "workstation"

[packages]
repo = ["neovim", "fish", "firefox", "git", "ripgrep"]

[kernel]
cmdline = ["quiet", "amd_iommu=on"]

[system]
hostname = "forge"
timezone = "Asia/Riyadh"
locale   = { lang = "en_US.UTF-8", generate = ["en_US.UTF-8 UTF-8"] }
```

### 19.3 A server

```toml
kiln = 1

include = ["@kiln/profiles/server", "@kiln/virt/podman"]

[image]
name = "edge-01"

[packages]
repo    = ["restic", "htop"]
exclude = ["nano"]

[systemd]
enable = ["fstrim.timer"]
mask   = ["systemd-networkd-wait-online.service"]

[[systemd.unit]]
name   = "backup.timer"
source = "units/backup.timer"
enable = true

[[systemd.unit]]
name   = "backup.service"
source = "units/backup.service"

[system]
hostname = "edge-01"
timezone = "UTC"
```

### 19.4 Splitting across files

```toml
# system.toml
kiln = 1

include = [
  "hardware.toml",
  "apps.toml",
  "@kiln/profiles/workstation",
  "@kiln/desktop/plasma",
]

[boot]
timeout = 0        # wins over whatever hardware.toml or a module says
```

```toml
# hardware.toml
kiln = 1

include = ["@kiln/gpu/nvidia-open", "@kiln/hardware/intel-ucode", "@kiln/hardware/laptop"]

[kernel]
cmdline = ["intel_iommu=on"]
```

```toml
# apps.toml
kiln = 1

[packages]
repo = ["neovim", "fish", "firefox", "obs-studio"]
aur  = ["zen-browser-bin"]
```

`packages.repo` unions across all of them. `boot.timeout` set in `system.toml` wins over every
included file, and if `hardware.toml` and `apps.toml` disagreed about a scalar, that would be a
hard error naming both.

### 19.5 Files, including a seeded one

```toml
kiln = 1

include = ["@kiln/profiles/minimal"]

[[file]]
source = "files/motd"
target = "/etc/motd"

[[file]]
source = "bin/deploy-check"
target = "/usr/bin/deploy-check"
mode   = "0755"

[[file]]
source = "files/sysctl/"          # trailing slash: recursive tree copy
target = "/usr/lib/sysctl.d/"

[[file]]
target  = "/usr/lib/tmpfiles.d/scratch.conf"
content = "d /var/scratch 0755 root root 30d\n"

[[file]]                          # seeded: restored from factory on a machine with no copy
target  = "/var/lib/myapp/seed.db"
source  = "files/seed.db"
```

### 19.6 A kernel with an out-of-tree module and a DKMS driver

```toml
kiln = 1

include = ["@kiln/profiles/workstation", "@kiln/kernel/linux-zen"]
# note: profiles already include a kernel. Copy the profile's include list and
# swap the kernel line instead of adding a second one; two is a conflict.

[kernel]
cmdline = ["quiet"]

[kernel.modules]
initramfs = ["i915"]                        # dracut --add-drivers
load      = ["v4l2loopback"]                # /etc/modules-load.d/kiln.conf
blacklist = ["nouveau"]                     # /etc/modprobe.d/kiln.conf
options   = { v4l2loopback = "devices=2 exclusive_caps=1" }   # /etc/modprobe.d/kiln.conf

[[kernel.module]]
name   = "my-module"
source = "kernel/my-module"       # a plain Makefile tree

[[kernel.dkms]]
name   = "my-driver"
source = "kernel/my-driver"       # a tree with a dkms.conf
```

For a DKMS package from the AUR, name it in both places:

```toml
[packages]
aur = ["v4l2loopback-dkms"]

[kernel]
dkms = ["v4l2loopback-dkms"]
```

### 19.7 Your own packages and repositories

```toml
kiln = 1

include = ["@kiln/profiles/minimal"]

[repos]
extra = [
  { name = "myrepo", server = "https://pkgs.example.com/$arch", key = "keys/myrepo.gpg" },
]

[packages]
repo  = ["myapp"]                                   # from myrepo
build = ["pkgbuilds/my-driver"]                     # needs PKGBUILD + .SRCINFO
file  = [
  { path = "packages/vendor-tool-2.1-1-x86_64.pkg.tar.zst", sha256 = "9f2c…" },
  { path   = "https://example.com/other.pkg.tar.zst",
    sha256 = "https://example.com/other.pkg.tar.zst.sha256" },
]
```

### 19.8 A build script

```toml
kiln = 1

include = ["@kiln/profiles/minimal"]

[[script]]
source = "scripts/20-locale.sh"
after  = "files"

[[script]]
name    = "brand"
after   = "packages"
content = """
#!/bin/sh
set -eu
printf 'Terracotta\\n' > /usr/lib/os-release-brand
"""
```

The first derives its name (`20-locale`) from the source file's stem. The second uses inline
content, so it must give a `name`.

### 19.9 A pinned, reproducible image

```toml
kiln = 1

include = ["@kiln/profiles/server"]

[repos]
snapshot = "2026-08-24"        # everything resolves from the Arch Archive

[packages]
repo = ["nginx"]
aur  = [{ name = "some-tool", commit = "a81fc2ef4b0d" }]
```

You do not have to do this to get reproducibility. Every build records the date it resolved on,
so `kiln rebuild <gen>` reconstructs a past image without anyone having pinned anything in
advance.

### 19.10 A full session

```console
$ sudo kiln init
wrote /etc/kiln/system.toml
next:  edit it, then `kiln check --offline`

$ $EDITOR /etc/kiln/system.toml
$ kiln check --offline          # is the configuration valid?
$ kiln check                    # what would a build change?
$ sudo kiln apply               # build, commit, stage
$ kiln list                     # see the new generation as `boots next`
$ sudo reboot

$ kiln status                   # confirm it booted and was marked good
$ kiln diff 43 44               # what actually changed
$ sudo kiln rollback            # if it was a mistake
```

---

## 20. FAQ

**Is this NixOS?**
No, and it deliberately is not trying to be. Kiln describes what is *inside an image*, not how
a whole system is configured, and its TOML has no language in it. There are no variables,
conditionals or functions, and there will not be.

**Can I apply changes without rebooting?**
No. One image, one reboot. A system that is sometimes the image and sometimes something else
cannot answer the question Kiln exists to answer.

**Can Kiln manage my users, dotfiles or GNOME settings?**
No. Those change without a reboot, so they are not image content. `/home` lives in `/var` and
is yours.

**Where did my `/var` go after a rollback?**
Nowhere. `/var` is shared across every generation and is never rolled back. Rolling back
restores the *image*, not your application state. That is the single biggest surprise in
systems of this shape.

**Why is my edited `/etc` file not changing?**
OSTree 3-way merges `/etc` at deploy, and it treats your hand-edit as a local modification that
wins over every future generation. `kiln status` reports it; the fix is
`cp /usr/etc/<path> /etc/<path>`. See [section 11.5](#115-etc-drift).

**Where is the lockfile?**
There isn't one. Every commit carries its own build record. OSTree is already a versioned
content-addressed store, and a second source of truth could only disagree with it.

**How do I reproduce a past image?**
`kiln rebuild <gen>`. It reads the generation's own commit, pins the recorded snapshot date to
drive the Arch Archive mirrors, and reports precisely what came back different.

**Why do I have to write `sha256` for a local package?**
Because an optional integrity guarantee is not a guarantee.

**Can I use systemd-boot?**
No. libostree keeps `/boot/loader` as a symlink pair for atomic entry swaps, vfat has no
symlinks, and UEFI reads only FAT, so `/boot` cannot be the ESP. libostree has no systemd-boot
backend at all. Writing `boot.loader = "systemd-boot"` gets that explanation.

**Can I use mkinitcpio?**
No. The sysroot pivot needs an initramfs hook, and upstream ostree ships one for dracut
(`50ostree`) and none for mkinitcpio.

**Why does `kiln build` say "nothing to do"?**
The plan already matches what is deployed. That is the plan/realize split paying off: Kiln
asked the question before paying for the answer. `--force` rebuilds anyway.

**Why does `kiln check` exit 10?**
Because 10 means "found changes". `kiln check && echo current` works in a timer or a prompt
without parsing anything.

**Can I install Kiln on a normal (non-OSTree) Arch system?**
Yes; the binary installs anywhere. You can run `kiln check`, `explain` and `show` there. To
`build` and `deploy` you need an OSTree sysroot, which is what `kiln sysroot init` creates.

**Does Kiln need the AUR?**
No. It supports it, with visible seams: commit-based identity, a marked dependency closure, and
volatile VCS inputs that it refuses to guess at.

**Can I build for a different architecture?**
Not yet. `image.arch` exists and is part of the identity, but multi-arch is not implemented.

**Where does Kiln store things?**
`/var/lib/kiln`, all of which is cache and history. Deleting it costs time, never correctness.

**How do I get a Kiln machine in the first place?**
Kiln has no installer. Use
[terracotta-installer](https://github.com/Terracotta-Linux/terracotta-installer) and
[terracotta-iso](https://github.com/Terracotta-Linux/terracotta-iso), or write your own against
the `--sysroot` seam.

---

## 21. Contributing

Contributions are welcome, and they do not have to be code.

| Way to help | Notes |
| --- | --- |
| 🐛 Bug fixes | A reproduction in `tests/corpus/` or a unit test is worth more than the fix |
| ✨ Features | Check them against the scope boundaries in [section 1.3](#13-the-philosophy) first |
| 📖 Documentation | Including this guide. If something here was hard to follow, that is a bug |
| 🧪 Tests | Especially around assembly and the Arch→OSTree contract |
| 🧩 Modules | Small, single-decision additions to `modules/` |
| 💡 Suggestions | Open an issue; design discussion is welcome |

### Before you open a pull request

```console
$ cargo fmt
$ cargo clippy --all-targets      # zero warnings
$ cargo test
$ sudo -E cargo test -- --ignored # if you touched anything privileged
```

Read [`CLAUDE.md`](../CLAUDE.md) before a change that touches more than one file. It covers the
architecture and the conventions the codebase depends on.

### Expectations

- **Diagnostics are a feature.** A new failure path deserves a message that names the span and
  says what to write instead. Add a corpus case whose *rendered* diagnostic is snapshotted.
- **Read snapshot diffs as a user would.** When you change a diagnostic, that reading is the
  review, not a formality.
- **Never paste new hash-freeze values in.** See
  [the hash-freeze rule](#the-hash-freeze-rule).
- **Privileged tests are `#[ignore]`d with a reason** and skip with a message rather than
  failing.
- **Docs ship with the change.** A change to the CLI surface, the schema, or a command's output
  is a change to this guide in the same commit.
- **Say why, not what, in comments.** Several orderings in this codebase are load-bearing and
  none of them are obvious; the comments explaining them are why they survive refactors.

---

## 22. Issues and bug reports

**Issues are always welcome**, including ones that turn out not to be bugs. Open them at
**[github.com/Terracotta-Linux/kiln/issues](https://github.com/Terracotta-Linux/kiln/issues)**.

Worth reporting:

- Crashes, panics, or a build that fails on a valid configuration.
- A command whose behaviour does not match this guide, in either direction.
- **Confusing or misleading diagnostics.** Error quality is a feature here, so a message that
  sent you the wrong way is a real bug.
- Documentation that is wrong, missing, or hard to follow.
- An image that builds and then does not boot, which is the most expensive failure in the
  pipeline and the one furthest from its cause.

### What makes a useful report

1. `kiln --version` output (it includes the schema version and hash epoch).
2. The exact command you ran and its full output.
3. The smallest configuration that reproduces it. `kiln check --offline` output is often
   enough; if not, `kiln show` and `kiln explain <key>` are.
4. For a build failure: whether `sudo kiln build --keep-failed` reproduces it, and the sandbox
   log path it printed.
5. For a boot failure: `kiln status` and `kiln list` from the generation you landed on.

Please do not paste private hostnames, keys or serial numbers. Nothing in Kiln's output needs
them.

---

## 23. AI assistance

AI tools were used while building parts of Kiln, including code, tests and documentation.
Everything here is reviewed, tested and maintained by a human, and the design decisions,
particularly the scope boundaries in [section 1.3](#13-the-philosophy), are deliberate ones.

This note exists so you know what went into the project. It is not a disclaimer about quality:
the boot acceptance test, the snapshot corpus and the hash-freeze tests are what quality claims
rest on here, not on who or what typed the first draft.

---

## 24. License

MIT. See [`LICENSE`](../LICENSE) in the repository root.

Copyright © 2026 Abdullah AL-Swedi.

Kiln links libalpm, libostree and glib, and drives pacman, dracut, GRUB, makepkg, dkms,
systemd and bubblewrap. Those are separate projects under their own licenses.
