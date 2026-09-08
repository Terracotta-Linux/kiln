<div align="center">

# 🔥 Kiln

**A declarative Linux system image builder for Arch.**

TOML in `/etc/kiln` → an OSTree commit → a bootable deployment.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/Terracotta-Linux/kiln?color=orange)](https://github.com/Terracotta-Linux/kiln/releases)
[![Release build](https://img.shields.io/github/actions/workflow/status/Terracotta-Linux/kiln/release.yml?label=release%20build)](https://github.com/Terracotta-Linux/kiln/actions/workflows/release.yml)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-B7410E?logo=rust&logoColor=white)](Cargo.toml)
[![Guide](https://img.shields.io/badge/docs-GUIDE-informational)](docs/GUIDE.md)
[![Last commit](https://img.shields.io/github/last-commit/Terracotta-Linux/kiln)](https://github.com/Terracotta-Linux/kiln/commits/main)

</div>

---

You describe what is inside your system. Kiln builds it, commits it to OSTree, and stages it
for the next boot. If the new image is wrong, you reboot into the old one; it is still there,
byte for byte.

```toml
# /etc/kiln/system.toml
kiln = 1
include = ["@kiln/profiles/workstation", "@kiln/desktop/gnome", "@kiln/gpu/amd"]

[packages]
repo = ["neovim", "fish", "firefox"]

[systemd]
enable = ["fstrim.timer"]

[[file]]
source = "files/motd"
target = "/etc/motd"
```

```console
$ kiln check        # what would change, without building anything
$ sudo kiln apply   # build it, stage it for the next boot
$ sudo reboot
$ sudo kiln rollback # if it was a mistake
```

📖 **New here?** [`docs/GUIDE.md`](docs/GUIDE.md) is the complete guide: concepts, the whole
schema, every command, the architecture, and troubleshooting.

---

## 🤔 What Kiln is

Kiln is **a distribution's build tool, not an image-shipping pipeline**. The loop is: write
config on your system, build on your system, deploy on your system, use your system. No
remotes, no registry, no push, no pull, no fleet.

It answers exactly one question: *what is inside the image?* The test for whether something
belongs in Kiln is simple. **If it changes, do you need a new image and a reboot?** If not,
it is out of scope, deliberately and permanently:

| Not in Kiln | Why |
| --- | --- |
| Login accounts, dotfiles, desktop settings | Image content only; your `/var` is yours |
| Live apply | One image, one reboot. There is no `/etc`-only fast path |
| Installation (`kiln install`, an ISO, partitioning) | `--sysroot` and `kiln sysroot init` are the seam an installer builds against |
| An implicit base | An empty config produces an empty image. `@kiln/profiles/minimal` is the one-line answer |
| Variables, conditionals, functions, inheritance | TOML here is data. `include` is the only composition operator |

## ✨ Why it might interest you

- ⏪ **Atomic and reversible.** Every build is a numbered generation. `kiln rollback` boots
  the previous one, and a machine that fails to boot three times demotes the new generation
  and boots the old one by itself.
- 📦 **It stays Arch.** Real pacman packages from real Arch repositories, the AUR, your own
  PKGBUILDs, out-of-tree and DKMS kernel modules, local or remote `.pkg.tar.zst` files.
  `pacman -Q`, `kiln why` and `kiln owns` all work inside the booted image.
- 🔍 **`kiln check` covers every input**, not only official packages. Your files, your
  PKGBUILDs, your AUR pins and your settings report in one place, and the fix is always one
  command.
- 💬 **It explains itself.** `kiln explain boot.timeout` says which file set a value and what
  it overrode. `kiln why firefox` says what pulled a package in. `kiln diff 41 42` says what
  changed between two generations, read from the commits rather than from a lockfile.
- 🚫 **There is no lockfile.** Every commit carries its own build record. OSTree is already a
  versioned content-addressed store; a second source of truth could only disagree with it.

## 🏗️ How it works

Six stages, in order:

```text
frontend       TOML → parse → include graph → merge → validate → Manifest    ⇒ config_id
resolution     pacman syncdb · AUR RPC · local hashing → BuildPlan           ⇒ plan_id
               (plan_id already deployed? stop, unless --force)
realization    fetch packages · build PKGBUILDs in sandboxes → artifact store
assembly       pacman transaction into a staging root · files · unit state
normalization  usr-merge · /etc→/usr/etc · /var drain · depmod · initramfs
commit&deploy  ostree commit (carrying the build record) · deploy
```

The **plan/realize split** is the load-bearing idea. Resolution is cheap, networked and
metadata-only; realization is expensive and sandboxed. That split is what makes `kiln check`
possible without building, and what lets `kiln build` refuse a no-op.

Three identities travel with an image: `config_id` (blake3 of the canonical manifest, local
file digests included), `plan_id` (`config_id` plus every resolved external input; this is
the build identity), and the OSTree commit checksum.

## 🚀 Installation

Kiln runs on an OSTree-capable Arch-derived system. To put a Kiln-built system such as
Terracotta Linux on real hardware:

- **[terracotta-installer](https://github.com/Terracotta-Linux/terracotta-installer)**
  partitions the disk and builds the image onto it.
- **[terracotta-iso](https://github.com/Terracotta-Linux/terracotta-iso)** is the live ISO
  that boots the installer. Prebuilt images are on its
  [releases page](https://github.com/Terracotta-Linux/terracotta-iso/releases).

To install Kiln itself on an existing Arch system, build the package from `packaging/`:

```console
$ git clone https://github.com/Terracotta-Linux/kiln.git
$ cd kiln/packaging
$ makepkg -si          # builds terracotta-kiln, installs kiln + the module library
```

Released packages (`terracotta-kiln-x86_64.pkg.tar.zst`, with a `.sha256` beside them) are
attached to every [GitHub release](https://github.com/Terracotta-Linux/kiln/releases), and
`@kiln/terracotta/kiln` installs them straight into an image.

Runtime dependencies: `pacman`, `bubblewrap`, `ostree`, `dracut`, `grub`, `git`.
Full details are in the [installation section of the guide](docs/GUIDE.md#4-installation).

## 🧭 Everyday commands

| Command | What it does |
| --- | --- |
| `kiln check` | What a build would change, without building. Exits `10` when something would |
| `kiln apply` | Build, commit, and stage for the next boot |
| `kiln build` | Build and commit, without deploying |
| `kiln list` / `kiln status` | Every generation; what is booted, what boots next, `/etc` drift |
| `kiln rollback` / `kiln deploy <gen>` | Boot the previous generation, or a specific one |
| `kiln diff [<gen>] [<gen>]` | What changed between two generations |
| `kiln explain <key>` | Which file set a value, and what it overrode |
| `kiln why <pkg>` / `kiln owns <path>` | What pulled a package in; which package owns a file |
| `kiln show [<gen>]` | The merged manifest, on disk or from a past commit |
| `kiln rebuild <gen>` | Reconstruct a past generation from its own build record |
| `kiln clean` / `kiln pin <gen>` | Reclaim disk; keep a generation regardless |
| `kiln init` / `kiln sysroot init <path>` | Scaffold a config; create a sysroot to build into |

Full flags, output and error cases are in the
[CLI reference](docs/GUIDE.md#5-cli-reference).

## ⚙️ Configuration in one screen

```toml
kiln = 1                                   # schema version; required, first key

include = ["hardware.toml", "@kiln/profiles/workstation"]

[image]
name = "workstation"                       # → ostree ref kiln/workstation/x86_64

[packages]
repo    = ["base", "git", "neovim"]
aur     = ["zen-browser-bin"]
exclude = ["nano"]

[kernel]
package = "linux"
cmdline = ["quiet", "amd_iommu=on"]

[systemd]
enable = ["sshd.socket", "fstrim.timer"]

[[file]]
source = "files/motd"
target = "/etc/motd"                       # Kiln owns the /usr/etc translation
```

Merging is three rules: **lists union**, **the includer wins**, and **two siblings setting the
same scalar differently is a hard error**, never a silent last-one-wins. Every key keeps the
file and line it came from, which is what `kiln explain` prints.

The [configuration chapter](docs/GUIDE.md#6-the-configuration-system) documents every key,
its type, its default and its behaviour.

## 🧩 The module library

Kiln ships 58 small modules under `/usr/share/kiln/modules`, included by name:

```text
@kiln/profiles/   minimal · workstation · server
@kiln/kernel/     linux · linux-lts · linux-zen · linux-hardened · linux-rt
@kiln/boot/       grub2 · plymouth
@kiln/net/        networkmanager · systemd-networkd · nftables · sshd · iwd · tailscale
@kiln/gpu/        nvidia-open · nvidia-open-lts · nvidia-open-dkms · nvidia-cuda · amd · amd-rocm · intel
@kiln/desktop/    gnome · plasma · xfce · cosmic  (each with -minimal, plus gnome-full/plasma-full)
@kiln/wm/         hyprland · sway · niri · i3
@kiln/audio/      pipewire
@kiln/hardware/   firmware · bluetooth · printing · laptop · intel-ucode · amd-ucode
@kiln/virt/       libvirt · podman · docker · distrobox · lilipod
@kiln/dev/        base-devel · rust · go
@kiln/security/   wheel-sudo · apparmor
@kiln/terracotta/ kiln · installer · branding · branding-plymouth
```

Four rules keep the library honest, all enforced in CI: one decision per module, a 25-line
cap, only profiles compose, and every unit a module enables comes from a package that module
installs. If a module is not quite what you want, copy it into your own tree and edit it.
That is a supported workflow, not a workaround.

## 📚 Documentation

| Document | For |
| --- | --- |
| [`docs/GUIDE.md`](docs/GUIDE.md) | Everything: concepts, CLI, full schema, architecture, development, FAQ |
| [`CLAUDE.md`](CLAUDE.md) | Conventions and design boundaries, for contributors |
| [`tests/vm/README.md`](tests/vm/README.md) | The boot acceptance test |
| [`tests/repo-fixture/README.md`](tests/repo-fixture/README.md) | The hermetic pacman repository the tests use |

## 🛠️ Development

```console
$ cargo build --release
$ cargo test                        # nothing privileged
$ sudo -E cargo test -- --ignored   # transactions, assembly, ostree: need root
$ cargo clippy --all-targets        # currently zero warnings; keep it that way
$ cargo fmt
```

Until the module library is installed to `/usr/share/kiln/modules`, point at the tree:

```console
$ cargo run --bin kiln -- --config ./myconfig --module-root ./modules check --offline
```

The boot acceptance test is the only one that proves the project works and the only one that
uses the network. It boots generation 1, boots generation 2, and rolls back, asserting every
claim from inside a qemu VM. It takes about twenty minutes:

```console
$ sudo -E cargo test -p kiln-cli --test boot -- --ignored --nocapture
```

More in the [development chapter](docs/GUIDE.md#16-development).

## 📈 Status

The frontend, the builder, the whole package/build/module input taxonomy and the command
surface are implemented and tested, including automatic rollback on boot failure and `/etc`
drift detection.

Declared in the schema and hashed into the image identity, but not yet written out by the
assembler: `[system]` (hostname, timezone, keymap, locale) and `kernel.modules`'
`load`/`blacklist`/`options`. The
[guide shows the `[[file]]` form to use meanwhile](docs/GUIDE.md#67-key-reference).

Not built yet: reproducibility auditing beyond `kiln rebuild`, and multi-arch.

## 🤝 Contributing

Contributions are welcome, and they do not have to be code:

- 🐛 Bug fixes and reproductions
- ✨ Features that fit the scope boundaries above
- 📖 Documentation improvements (including this README and the guide)
- 🧪 Tests, especially around assembly and the Arch→OSTree contract
- 💡 Suggestions and design discussion in an issue

Before a change that touches more than one file, read [`CLAUDE.md`](CLAUDE.md); it covers the
architecture and the conventions the codebase relies on. Run `cargo test`, `cargo clippy
--all-targets` and `cargo fmt` before opening a pull request, and `sudo -E cargo test --
--ignored` if you touched anything privileged. A change to the CLI surface, the schema or a
command's output is a change to [`docs/GUIDE.md`](docs/GUIDE.md) in the same commit; the
guide quotes real output in a dozen places.

## 🐛 Issues and bug reports

**Issues are always welcome**, and so are reports that turn out not to be bugs. Open one at
**[github.com/Terracotta-Linux/kiln/issues](https://github.com/Terracotta-Linux/kiln/issues)**
for:

- crashes, wrong behaviour, or a build that fails on a valid configuration
- confusing or misleading diagnostics (error quality is a feature here)
- documentation that is wrong, missing, or hard to follow
- a feature that exists but does not do what it says

A useful report includes `kiln --version`, the command you ran, the output, and the smallest
configuration that reproduces it. `kiln check --offline` output is often enough.

## 🤖 A note on AI assistance

AI tools were used while building parts of Kiln, including code, tests and documentation.
Everything here is reviewed, tested and maintained by a human, and the design decisions are
deliberate ones. This note is here so you know what went into the project, not as a
disclaimer about its quality.

## 📄 License

MIT. See [LICENSE](LICENSE).
