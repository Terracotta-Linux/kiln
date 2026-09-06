# The Kiln guide

Kiln compiles a TOML description of a system into an OSTree commit and boots it. This is
the guide for using it. [`CLAUDE.md`](../CLAUDE.md) covers the architecture — read that one
if you are changing Kiln, this one if you are running it.

1. [The loop](#the-loop)
2. [Your first image](#your-first-image)
3. [The configuration language](#the-configuration-language)
4. [The module library](#the-module-library)
5. [Packages and where they come from](#packages-and-where-they-come-from)
6. [Files, units, and the escape hatch](#files-units-and-the-escape-hatch)
7. [Kernels and modules](#kernels-and-modules)
8. [Generations, rollback, and disk](#generations-rollback-and-disk)
9. [`/etc` drift](#etc-drift)
10. [Answering questions about your system](#answering-questions-about-your-system)
11. [When something goes wrong](#when-something-goes-wrong)
12. [Command reference](#command-reference)

---

## The loop

```
edit /etc/kiln    →    kiln check    →    kiln apply    →    reboot
                            ↑                                   │
                            └───────────  kiln rollback  ────────┘
```

Four things about that loop are worth internalizing before anything else.

**There is no live apply.** Every change is a new image and a reboot. Kiln has no fast path
that edits `/etc` in place, and will not grow one — a system that is sometimes the image and
sometimes something else cannot answer the question Kiln exists to answer.

**Building is not deploying.** `kiln build` produces a commit and stops. `kiln apply` builds
*and* stages the result for the next boot. If you are unsure which you want, you want
`apply`.

**Nothing is thrown away.** Every build is a numbered generation, kept until `kiln clean`
takes it. Rolling back is reordering a list, not restoring a backup.

**Kiln owns the image, not the machine.** `/var` is yours and persists across every
generation — your home directory, your databases, your container images, your logs. Rolling
back restores the *image* and not your application state, which is the number one surprise
in systems like this one.

---

## Your first image

You need an OSTree-capable machine — either one already running a Kiln image, or a mounted
target you are building into directly (see
[building into an unmounted target](#building-into-an-unmounted-target)). Kiln does not
install anything itself.

```console
$ sudo kiln init
wrote /etc/kiln/system.toml
next:  edit it, then `kiln check --offline`
```

That scaffold is four lines, on purpose:

```toml
# Kiln — what is inside this system's image.
# Everything here needs a new image and a reboot; nothing else belongs.

kiln = 1

include = ["@kiln/profiles/minimal"]

[packages]
repo = []
```

`@kiln/profiles/minimal` is the smallest set of packages that boots and reaches a shell — no
network stack, because a machine reachable over the network is a decision.
**Kiln has no implicit base**: delete that line and you get an empty image, not a surprising
one. `@kiln/profiles/workstation` and `@kiln/profiles/server` make the opposite decisions
about the same question. Add what you want:

```toml
kiln = 1

include = [
  "@kiln/profiles/workstation",
  "@kiln/desktop/gnome",
  "@kiln/gpu/amd",
]

[packages]
repo = ["neovim", "fish", "firefox"]

[system]
hostname = "forge"
timezone = "Asia/Riyadh"
```

Then check it without touching the network or building anything:

```console
$ kiln check --offline
```

`--offline` validates the configuration — parsing, includes, merging, every semantic rule —
without resolving package versions. Drop it and `kiln check` resolves everything and tells
you exactly what a build would produce that you do not already have. When it looks right:

```console
$ sudo kiln apply
$ sudo reboot
```

### Building into an unmounted target

Kiln has no `install` verb and no opinion about your disk — partitioning, formatting, an
initial account, and a bootloader install onto media you are not currently running from are
somebody else's job. What Kiln gives that job is a seam: `--sysroot` and `kiln sysroot init`
let `build` and `deploy` target a mounted root other than `/`.

```console
# the installer partitions, formats and mounts the target at /mnt
$ kiln sysroot init /mnt                              # Kiln's storage, like `git init`
$ cp -r myconfig /mnt/etc/kiln
$ kiln build  --sysroot /mnt --config /mnt/etc/kiln
$ kiln deploy --sysroot /mnt 1                        # build does not deploy
$ grub-install --efi-directory=/mnt/boot/efi …        # the installer's job, not Kiln's
```

That order matters and Kiln enforces it rather than trusting you to read it: building into a
target that was never `sysroot init`ed succeeds and then fails at deploy, so `build` warns
when its target is not initialized.

`grub-install` runs inside the deployment, so the binaries it needs have to be in the image
rather than on the installer's live medium. `@kiln/boot/grub2` installs both: `grub`, for the
`grub-mkconfig` every later deploy re-runs, and `efibootmgr`, which `grub-install` execs to
write the UEFI boot entry. `efibootmgr` is only an *optional* dependency of `grub`, so an
image that names neither builds and deploys perfectly and then cannot be made bootable.

One thing the installer must write into the configuration, because only it knows the answer:

```toml
[kernel]
cmdline = ["root=UUID=…", "rw"]
```

Kargs are fully declarative — Kiln has no hidden set it preserves — so a deploy without
`root=` produces a machine that boots exactly once.

---

## The configuration language

### Discovery

```
/etc/kiln/
├── system.toml         ← the entry point
├── hardware.toml
├── files/
├── units/
└── pkgbuilds/
```

The entry point is `/etc/kiln/system.toml`, or whatever `--config` points at (a file, or a
directory holding `system.toml`). The directory containing it is the **config root**.

Two rules about the config root are worth knowing up front:

- **It is a security boundary.** Every `source` and `path` in the whole configuration must
  resolve, after following symlinks, inside it. `source = "../../home/you/.ssh/id_ed25519"`
  is a hard error, not a leak. Escaping needs `--allow-external-sources` and warns about
  each path that did.
- **Nothing is glob-loaded.** There are no drop-in directories. Every file that participates
  is reachable through an explicit `include` chain from `system.toml`, so the chain *is* the
  documentation of what your system is made of.

`/etc/kiln` is not required to be the source of truth. `kiln build --config ~/src/my-image`
works identically, which is what makes a git-tracked configuration and a build server
possible.

### `include`, and the three merge rules

`include` is the only composition operator. There are no variables, no interpolation, no
conditionals, no functions, and no inheritance keywords. TOML here is data.

| Form | Means |
|---|---|
| `"hardware.toml"` | relative to the **including file's** directory |
| `"@kiln/desktop/gnome"` | a shipped module, `/usr/share/kiln/modules/desktop/gnome.toml` |

`include` must be a top-level key **before any table header**. A bare key written after
`[packages]` silently becomes `packages.include` in TOML; Kiln catches that one by span and
tells you to move it.

Merging is three rules:

1. **Lists union.** Duplicates collapse, order is discarded. `packages.repo` from six files
   is one deduplicated set. Reordering lines in a file can never change what you get.
2. **The includer wins.** If your `system.toml` includes `@kiln/boot/grub2` and both set
   `boot.timeout`, yours is used.
3. **Siblings conflicting is an error.** If you include two files that set `boot.timeout` to
   *different* values, that is a hard error naming both files and lines — never a silent
   last-one-wins. Set it yourself in the includer to resolve it. Identical values are fine.

Including the same file twice is a no-op; a cycle is an error that prints the cycle.

There is **no unset operator**. `packages.exclude`, `systemd.disable` and `systemd.mask`
cover the real cases. If you need to not have something, do not include the file that adds
it.

### The whole schema

Nine top-level tables and four array-of-table forms. This is all of it:

```toml
kiln = 1                                  # schema version. Required, first key.

include = ["hardware.toml", "@kiln/profiles/workstation"]

[image]
name = "workstation"                      # → ostree ref kiln/workstation/x86_64
arch = "x86_64"

[repos]
snapshot = "2026-08-24"                   # or omit: track live mirrors, like Arch
extra    = [{ name = "myrepo", server = "https://pkgs.example.com/$arch",
              key = "keys/myrepo.gpg" }]

[packages]
repo    = ["base", "git", "neovim", "firefox"]
aur     = ["zen-browser-bin", { name = "foo-git", commit = "a81fc2e" }]
build   = ["pkgbuilds/my-driver"]
file    = [{ path = "packages/myapp-1.0-1-x86_64.pkg.tar.zst", sha256 = "9f2c…" }]
exclude = ["nano"]                        # must not appear, even as a dependency

[kernel]
package = "linux"
headers = true
cmdline = ["quiet", "amd_iommu=on"]

[kernel.modules]
load      = ["v4l2loopback"]
blacklist = ["nouveau"]
options   = { v4l2loopback = "devices=2 exclusive_caps=1" }

[[kernel.module]]                         # built from source, out of tree
name   = "my-module"
source = "kernel/my-module"

[boot]
loader  = "grub2"
timeout = 5

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

[system]
hostname = "forge"
timezone = "Asia/Riyadh"
locale   = { lang = "en_US.UTF-8", generate = ["en_US.UTF-8 UTF-8"] }
keymap   = "us"
```

Every table is optional. `kiln explain <key>` will tell you what Kiln does when you leave
one out, and why.

---

## The module library

Kiln ships around fifty modules in `/usr/share/kiln/modules`, included by name:

```
@kiln/profiles/    minimal · workstation · server
@kiln/kernel/      linux · linux-lts · linux-zen · linux-hardened · linux-rt
@kiln/boot/        grub2 · plymouth
@kiln/net/         networkmanager · systemd-networkd · nftables · sshd · iwd · tailscale
@kiln/gpu/         nvidia-open · nvidia-open-lts · nvidia-cuda · amd · amd-rocm · intel
@kiln/desktop/     gnome-minimal · gnome · gnome-full
                   plasma-minimal · plasma · plasma-full
                   xfce-minimal · xfce
                   cosmic-minimal · cosmic
@kiln/wm/          hyprland · sway · niri · i3
@kiln/audio/       pipewire
@kiln/hardware/    firmware · bluetooth · printing · laptop · intel-ucode · amd-ucode
@kiln/virt/        libvirt · podman · docker
@kiln/dev/         base-devel · rust · go
@kiln/security/    wheel-sudo · apparmor
@kiln/terracotta/  kiln · installer · branding · branding-plymouth
```

A module is a small TOML file with no magic in it. This is the whole of
`@kiln/gpu/nvidia-open`:

```toml
kiln = 1
[packages]
repo = ["nvidia-open", "nvidia-utils"]
[kernel]
cmdline = ["nvidia_drm.modeset=1"]
[kernel.modules]
blacklist = ["nouveau"]
```

Four rules keep the library honest, and they tell you what to expect from it:

1. **One module, one decision.** `@kiln/gpu/nvidia-open` installs the driver and stops. No
   CUDA, no Vulkan layers, no control panel — those are separate decisions.
2. **Only profiles include other modules,** one level deep. Everywhere else the graph is
   flat, so including one module cannot silently drag in nine files.
3. **A module fits on one screen.** Hard cap, 25 lines.
4. **A module ships a file only when the thing does not work without it.** Opinions about
   how you would like it configured are not Kiln's to ship.

If a module is not quite what you want, copy it into your own tree and edit it — that is a
supported workflow, not a workaround. `include = ["modules/my-nvidia.toml"]` and yours wins
over anything it includes.

---

## Packages and where they come from

Five kinds of input, all of which end up as a `.pkg.tar.zst` going through pacman:

| Key | What it is |
|---|---|
| `packages.repo` | official Arch repositories, and any `repos.extra` you add |
| `packages.aur` | AUR packages, built in a sandbox |
| `packages.build` | your own PKGBUILDs, from a directory in your config tree |
| `packages.file` | a `.pkg.tar.zst`, local or by URL, with a required `sha256` |
| `[[kernel.module]]` | an out-of-tree module, built against the image's kernel |

A few things that surprise people:

**A bare string is shorthand.** `aur = ["zen-browser-bin"]` and
`aur = [{ name = "zen-browser-bin" }]` are the same. The long form exists so you can pin:
`{ name = "foo-git", commit = "a81fc2e" }`.

**A local package's checksum is required, not optional.** An optional integrity guarantee is
not a guarantee.

**`packages.file`'s `path` can be a URL instead.** `{ path = "https://example.com/myapp.pkg.tar.zst",
sha256 = "…" }` works the same as a path relative to the config root, except the download
happens during `kiln build`, not `kiln check` — resolution carries the URL and its `sha256`
through untouched, the same way it carries an AUR package's pinned commit. Only `http://` and
`https://` are accepted.

**`sha256` can be a URL too**, pointing at a `.sha256` file rather than naming the digest
directly: `sha256 = "https://example.com/myapp.pkg.tar.zst.sha256"`. Unlike the package itself,
`kiln check` *does* fetch this — it is a few bytes, the same kind of network call resolution
already makes to ask the AUR for a package's current commit — and resolves it to the concrete
digest before it reaches the plan. The file is parsed the way `sha256sum` writes one: 64 hex
characters, optionally followed by whitespace and a filename.

**The network is off during every build.** It is on during resolution and while fetching
sources, and off from the moment a build phase starts. That constraint is exactly what makes
a build's output a function of things Kiln has hashed — and it means a PKGBUILD that
downloads something in `build()` will fail, correctly.

**`repos.snapshot` is opt-in.** By default Kiln tracks live mirrors, like Arch. Pin a date
only when you want a frozen package set; `kiln rebuild <gen>` reproduces a past image
without one.

**An AUR package's repo dependencies are not in the plan.** If `zen-browser-bin` needs
`qt6-base`, that comes from the official repositories at build time and is not something
`kiln check` reports moving. Asking the AUR for `qt6-base` would fail with a message about a
package nobody wrote down, so the dependency closure deliberately stops at the AUR boundary.

---

## Files, units, and the escape hatch

### `[[file]]`

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

`source` or `content`, never both. Modes are strings — TOML has no octal literal, and `0755`
would be decimal 755.

**A `[[file]]` targeting `/var` is accepted with a note**, and it is seeded rather than
shipped: `/var` does not exist in the commit at all, so the file goes to
`/usr/share/factory` and a `tmpfiles.d` line materializes it at first boot. That is how you
ship a default database or a seed file, and it is a one-time thing rather than something the
image re-asserts.

**Kiln warns on a mode more restrictive than `0644`.** `/usr` is world-readable and a commit
outlives the generation it was built for, so a mode signalling secrecy is a mode the image
cannot honor. Secrets belong in `/var` at runtime, put there by something else.

### `[[systemd.unit]]` and unit state

```toml
[systemd]
enable  = ["sshd.socket"]
disable = ["systemd-resolved.service"]
mask    = ["NetworkManager-wait-online.service"]

[[systemd.unit]]
name   = "backup.timer"
source = "units/backup.timer"
enable = true
```

Unit state is image content. Naming a unit nothing in the image provides is a hard error at
assembly, in all three directions — so a typo in `enable` fails the build rather than
producing an image where the service silently is not running.

The corollary is that `systemctl enable` on the running machine is *drift*: it writes a
symlink into `/etc` that outlives every future generation's preset. See
[`/etc` drift](#etc-drift).

### `[[script]]`

```toml
[[script]]
source = "scripts/20-locale.sh"
after  = "packages"            # or "files", the default
```

The escape hatch, for the things the schema does not have a key for. A script runs chrooted
in the image being assembled, with **no network, ever**, and its effect is captured as an
overlayfs changeset — so Kiln can see exactly what it changed, report a script that
overwrites a package-owned file, and keep the output a pure function of hashed inputs.

Use it sparingly. Anything you find yourself writing a script for twice is probably a key
the schema should have, or a module.

---

## Kernels and modules

Three different things, three different keys:

```toml
[kernel]
package = "linux"                         # which kernel package
headers = false                           # build-time only; see below

[kernel.modules]                          # in-tree modules, just configured
load      = ["v4l2loopback"]
blacklist = ["nouveau"]
options   = { v4l2loopback = "devices=2 exclusive_caps=1" }

[[kernel.module]]                         # out-of-tree, built from source
name   = "my-module"
source = "kernel/my-module"
```

`headers = false` is the default and is usually right. Headers are a *build-time* dependency
that Kiln installs inside the sandbox when a module needs them; nothing on an immutable
system rebuilds modules at runtime, so shipping ~150 MB of headers in the image is waste.

Out-of-tree modules are built against the exact kernel in the image, and rebuilt when it
moves — `kiln check` reports that as `(kernel changed)` before you build.

The initramfs is dracut, with the upstream `ostree` module. The bootloader is GRUB2. Neither
takes another value; `boot.loader = "systemd-boot"` gets a diagnostic explaining that
libostree keeps `/boot/loader` as a symlink pair, vfat has no symlinks, and UEFI reads only
FAT — so `/boot` cannot be the ESP and systemd-boot cannot read the ext4 `/boot` libostree
needs.

---

## Generations, rollback, and disk

Every build is a numbered generation. The number is assigned at commit time and means the
same thing forever — unlike an OSTree deployment index, which renumbers as deployments come
and go.

```console
$ kiln list
 GEN  STATUS           COMMIT         GENERATED           IMAGE
  43  ● booted         9d41af02c3b1   2026-09-01 14:22:07 workstation
  42  rollback target  7f2a04e19bc8   2026-08-28 09:41:15 workstation
   1  baseline, pinned 31c9be750ad4   2026-07-02 11:03:44 workstation
```

Right after `kiln apply`, before you reboot, the generation it staged is the one marked
`boots next` — the running system stays `● booted` until you actually boot the new one:

```console
$ kiln list
 GEN  STATUS                    COMMIT         GENERATED           IMAGE
  44  boots next                c07be1d4f9a2   2026-09-03 14:50:11 workstation
  43  ● booted, rollback target 9d41af02c3b1   2026-09-01 14:22:07 workstation
   1  baseline, pinned          31c9be750ad4   2026-07-02 11:03:44 workstation
```

```console
$ kiln status
generation  43
image       workstation
built       2026-09-01 14:22:07
state       booted
rollback    generation 42
```

### Rolling back

```console
$ kiln rollback              # boot the previous generation
$ kiln deploy 41             # or a specific one
```

`kiln rollback` reorders the deployment list; the old image is still on disk, byte for byte.
It restores the **image** and not `/var`, which is shared across every generation.

### Automatic rollback

A generation staged by `kiln apply` boots on probation. GRUB counts the attempts in its own
`grubenv`, a unit in the image clears the counter once the system is up, and a generation
that fails to come up three times is demoted — the machine boots the previous one by itself,
with no rescue USB. `kiln status` says so when it happens:

```
boot        generation 44 failed to boot 3 times and was demoted;
            you are running generation 43. `kiln deploy 44` tries it again.
```

**Generation 1 is the baseline** and is protected: it is a generation known to have booted
on this exact hardware, which is what automatic rollback needs a floor of. `kiln clean` will
not take it without `--remove-baseline`.

### Disk

```console
$ kiln clean --dry-run       # see the decision
$ kiln clean                 # keep 3 + the baseline + anything pinned
$ kiln clean --keep 5
$ kiln pin 42                # keep this one regardless
```

`kiln clean` trims the artifact cache as well as the deployments. The cache budget is
`min(20 GiB, 10% of the filesystem)`, and a trim stops the moment it is under budget rather
than clearing everything — the next build should not re-download packages it had a minute
ago.

A build needs roughly twice the image size free, and Kiln checks before starting rather than
failing halfway through a transaction. Failed staging roots are removed; `--keep-failed`
keeps one for inspection.

---

## `/etc` drift

This is the one way a Kiln system can lie about itself, so it is worth understanding.

A file Kiln ships to `/etc` lives in the commit as `/usr/etc`. At every deploy, OSTree
3-way-merges the new commit's `/usr/etc` with your live `/etc`. That merge is what lets your
machine keep its own `fstab` and its own users across generations — and it is also what
makes a hand-edit permanent:

> If you edit a file in `/etc` that the image ships, the merge treats your version as a local
> modification and keeps it. **Forever.** Every future generation's version of that file
> loses, including one you built specifically to change it.

Rebuilding does not fix it. Rolling back does not fix it. `kiln diff` shows the change you
asked for, correctly, and the machine still has the old contents.

So `kiln status` reports it:

```
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

- **A `[[file]]` you wrote down is the sharp case** and is always named. You edited the
  config, Kiln built the file into the image, and the merge threw it away.
- **A locally created file shadows nothing** and is only counted — the image never had an
  opinion about that path. `kiln status --verbose` names them.
- **`systemctl enable` shows up here**, because unit state is image content and the symlink
  it writes outlives every future preset. Put it in `[systemd] enable` instead.

Files that a correct machine changes by itself — `machine-id`, the account files, `fstab`,
SSH host keys, `resolv.conf`, `/etc/kiln` — are never reported. That list is short and fixed
on purpose: each entry is something Kiln permanently gives up the ability to warn about.

There is no `kiln reset`. Deleting a file you chose to edit is destructive with no undo, and
the fix is a `cp` away.

---

## Answering questions about your system

### "Why is this value what it is?"

```console
$ kiln explain boot.timeout
boot.timeout
  value       0
  set in      system.toml:30
  overriding  @kiln/boot/grub2:18

  The includer wins over what it includes. Two files
  at the same depth disagreeing would have been an error, not this.
```

It takes more than an exact key. A **group** lists everything under it, set or not:

```console
$ kiln explain boot
boot
  a group of keys, not a value of its own

  boot.loader     "grub2"
                  set in @kiln/boot/grub2:17
  boot.timeout    0
                  set in system.toml:30
  boot.initramfs  "dracut"
                  Kiln's default — the only supported value
```

A **list** shows who asked for each element:

```console
$ kiln explain packages.repo
packages.repo
  kind        a list — 9 files unions into it
  22 elements, and who asked for each:
    fish            system.toml:18
    gdm             @kiln/desktop/gnome:5
    gnome-console   @kiln/desktop/gnome:6
    nvidia-open     @kiln/gpu/nvidia-open:11
    …
```

And a single **element** answers "which file put this here":

```console
$ kiln explain packages.repo/gnome-shell
packages.repo/gnome-shell
  asked for   @kiln/desktop/gnome:5
  in          packages.repo
```

`kiln explain include` is the odd one out, because there is no value to print — the graph
consumes the key. It answers the question the key actually stands for:

```console
$ kiln explain include
include
  kind        the include graph, not a value
  10 files, entry point first:
    system.toml
    hardware.toml
    @kiln/gpu/nvidia-open
    @kiln/profiles/minimal
    …
```

That list is complete by construction: nothing is glob-loaded, so every file that
participates is reachable through an explicit `include`.

### "Why is this package in my image?"

`kiln explain packages.repo/<name>` answers it from the configuration. `kiln why` answers it
from the built image, which is the other half — whether it is there because you asked or
because something depends on it:

```console
$ kiln why mesa
mesa 26.1.5-1
  from the extra repository
  required by gnome-shell, xdg-desktop-portal-gnome
```

A package that is neither named in your configuration nor required by anything says so
plainly — that is the answer to "why is this still here" after an `exclude` or a removed
dependency.

### "What owns this file?"

```console
$ kiln owns /etc/pacman.conf
pacman
  /etc/pacman.conf is usr/etc/pacman.conf in the image: Kiln moves /etc to
  /usr/etc and the live /etc is merged onto it at deploy.
  pacman 7.1.0-1
```

### "What changed?"

```console
$ kiln check                 # what a build would change, without building
$ kiln diff 42 43            # between two generations
$ kiln diff                  # booted vs pending
$ kiln show 42               # what generation 42 was asked to contain
```

None of `diff`, `why`, `owns` or `show <gen>` reads `/etc/kiln`. They read the generation's
own commit — which is the point, because the usual reason to ask is a generation whose
configuration has since been edited.

---

## When something goes wrong

### `kiln check` says two inputs could not be checked

```
  2 inputs could not be checked without fetching:
    foo-git, bar-git     (VCS packages; run `kiln check --deep`)
```

A VCS package's version comes from running `pkgver()` against upstream, and a `SKIP`
checksum states nothing. Kiln will not guess — an untrustworthy `check` is worse than no
`check` — so those inputs are excluded from the build identity and reported separately.
`kiln check --deep` fetches and answers them. It does not build; it runs the same
`makepkg --verifysource` a build's first phase would.

### The build failed

The staging root is removed on success and on failure. `--keep-failed` keeps it for one
build so you can look inside:

```console
$ sudo kiln build --keep-failed
```

A build failure exits `3`. A failed package does not take unrelated packages down with it —
its dependents are reported as skipped rather than blamed.

### The new image did not boot

If you get as far as a shell, `kiln status` says whether the counter demoted it. If you do
not, the machine will demote it for you after three attempts and boot the previous
generation. From there, `kiln deploy <gen>` tries it again, and `kiln rebuild <gen>` rebuilds
a past generation from its own record if you want the old image back rather than the old
deployment.

Should you need to pick manually, the previous generations are in the GRUB menu — that is
libostree's BLS entries, one per deployment, and nothing about them is Kiln-specific.

### A change to a config file is not taking effect

Check `kiln status` for `/etc` drift. If the file is listed there, your hand-edited copy is
winning over the image, and it will keep winning until you restore the image's version with
`cp /usr/etc/<path> /etc/<path>`.

### An error names a file you did not write

Kiln's diagnostics name the file *and line* of everything, including inside shipped modules:

```
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

Every phase reports all of its problems before the next one starts, so three type errors in a
file come out in one run rather than three.

### Exit codes

| Code | Means |
|---|---|
| `0` | fine, including "nothing to do" |
| `1` | configuration error |
| `2` | resolution failure |
| `3` | build failure |
| `4` | system or permission error |
| `10` | `kiln check` found changes — so `kiln check && echo current` works in scripts |

---

## Command reference

### Building

| Command | |
|---|---|
| `kiln check [--deep] [--offline]` | what would change, without building. `--offline` validates the configuration only; `--deep` resolves VCS packages by fetching |
| `kiln build [--force] [--offline]` | build an image. Refuses a no-op unless `--force` |
| `kiln apply [--force]` | build, then stage for the next boot |
| `kiln rebuild <gen>` | rebuild a past generation from its record |
| `--keep-failed` | on `build`/`apply`: keep a failed staging root |

### Inspection

| Command | |
|---|---|
| `kiln diff [<gen>] [<gen>]` | what changed between two generations. Default: booted vs pending |
| `kiln why <package>` | what pulled a package into the image |
| `kiln owns <path>` | which package owns a file in the image |
| `kiln explain <key>` | which file set a value. Also takes a group (`boot`) or an element (`packages.repo/neovim`) |
| `kiln show [<gen>]` | the merged manifest, or a past generation's |

### Deployments

Always by generation, never by OSTree index.

| Command | |
|---|---|
| `kiln list` | every generation on this machine |
| `kiln status` | what is booted, what boots next, `/etc` drift |
| `kiln rollback` | boot the previous generation |
| `kiln deploy <gen>` | boot a specific generation |
| `kiln pin <gen>` / `kiln unpin <gen>` | keep a generation through `kiln clean` |
| `kiln rm <gen>...` | undeploy generations |
| `kiln clean [--keep N] [--dry-run]` | keep N, the baseline, and anything pinned |
| `--remove-baseline` | let `rm` and `clean` take generation 1 |

### Storage

| Command | |
|---|---|
| `kiln init` | scaffold `/etc/kiln` |
| `kiln sysroot init <path>` | create an OSTree sysroot to build into |

### Global flags

| Flag | |
|---|---|
| `--config <path>` | entry point, or a directory containing `system.toml` |
| `--sysroot <path>` | operate on another root — the installer seam |
| `--allow-external-sources` | permit sources outside the config root, with a warning |
| `--module-root <path>` | override `/usr/share/kiln/modules` |
| `-v`, `--verbose` | more detail, including OSTree checksums |

`KILN_CONFIG_DIR` and `KILN_MODULE_DIR` do the same job as `--config` and `--module-root`.
