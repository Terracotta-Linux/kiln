# tests/vm — boot acceptance

Boot acceptance in qemu is the only test that proves the project works. Everything else in
the suite checks a tree, a plan, or a diagnostic. This is the one that puts a real
Arch-derived OSTree image on a real disk, boots it, and asks the running system whether the
Arch→OSTree contract actually holds.

```
kiln sysroot init  →  kiln apply  →  boot  →  assert generation 1
                      kiln apply  →  boot  →  assert generation 2
                      kiln rollback → boot  →  assert generation 1 again
```

The harness is `crates/kiln-cli/tests/boot.rs`. This directory holds the fixture it builds.

```
config/
├── system.toml                     the image
├── files/probe                     the in-image acceptance probe
├── scripts/10-marker.sh            a build script with a checkable effect
└── units/kiln-boot-probe.{service,timer}
```

## Running it

```
sudo -E cargo test -p kiln-cli --test boot -- --ignored --nocapture
```

Roughly twenty minutes, and several hundred megabytes downloaded. It skips with a message —
never fails — when root, `/dev/kvm`, qemu, `mkfs.ext4` or the network is missing.

**This is the one test in the workspace that uses the network**, and it has to: the hermetic
`tests/repo-fixture` holds four tiny packages with no kernel and no userland, and there is
no way to boot that. The whole point is a real image.

## The fixture is a real configuration

Nothing here is assembled by the harness. The probe reaches the image as a `[[file]]`, its
units as `[[systemd.unit]]`s, the `/var` seed as a `[[file]]` targeting `/var`, and the
marker as a `[[script]]`. That is deliberate: if any of those mechanisms is broken, a
booted system says so, rather than a snapshot test saying something about a directory.

`root=/dev/vda` is in `kernel.cmdline` and that is not a quirk of the test. Kargs are fully
declarative — Kiln passes the complete set on every deploy, so a karg that is not in
the manifest is one the next `kiln apply` removes. Whoever installs a system owns its disk
layout and writes that line.

## What the probe asserts

Every line it prints is `KILN| key=value`, read off the serial console after the VM powers
off. It is a **timer**, not a service wanted by `multi-user.target`: it asks systemd whether
boot succeeded, so it must not be part of the boot transaction it is judging.

| | |
|---|---|
| filesystem shape | `/usr` read-only, `/etc` writable and populated |
| the `/var` drain | every `d`/`C`/`L` line in Kiln's tmpfiles fragments produced something; `/var/lib/pacman` absent; `/var/home` and `/root` present |
| `/var`-targeted files | the `[[file]]` targeting `/var` was restored from `/usr/share/factory` |
| build scripts | the build script's changeset is readable from the running system |
| pacman on a read-only `/usr` | `pacman -Q` works from a read-only `/usr` |
| UID seed replay | service-account gids are the same in generation 2 as in generation 1 |
| the `/ostree` symlink | libostree can read its own sysroot from inside the deployment |
| automatic rollback counting | the grub.d boot-counting fragment is present, the generated `grub.cfg` contains the counting logic — which is the proof libostree ran `grub-mkconfig` *inside* the deployment and so read the fragment at all — and `boot_counter` is gone because this boot was blessed |
| boot success | `multi-user.target` active and no failed units — never `is-system-running --wait`, which waits for a queue this probe is itself in |

## What it does not prove

The VM boots the kernel and initramfs **directly**, extracted from the BLS entry libostree
wrote, rather than through GRUB from the ESP. That covers everything Kiln is responsible
for — that the entry exists, that it is the right one, that its `options` line carries the
kargs the manifest asked for, and that the initramfs pivots into the deployment. It does
not cover firmware→GRUB→kernel, which is GRUB's job and needs OVMF and a partitioned disk.

That boundary matters most for automatic rollback. Everything Kiln owns of it is checked
here: the fragment reaches the image, libostree reads it from inside the deployment, the
counting logic lands in `grub.cfg`, and a good boot clears the counter. What is *not* checked
is GRUB executing that logic — spending an attempt, and selecting entry 1 when they run out
— because nothing here runs GRUB. That is the same firmware→GRUB→kernel gap, and closing it
needs OVMF, a partitioned disk with an ESP, and a deliberately unbootable generation.

## The trap that is not Kiln's

**The host is the only writer.** qemu runs with `-snapshot`, so every guest write goes to a
temporary overlay that is discarded when the VM powers off, and the image is read-only from
the guest's side.

That is not an optimization; it is the whole reason this test is reproducible. The host
writes `disk.img` through a loop device, which carries its own page cache over that file,
while qemu reads and writes the same bytes through its own descriptor. Two caches over one
extent map are individually right and jointly wrong, and what that produces names neither:

```
error preparing the build directory: Bad message (os error 74)
EXT4-fs error (device vda): ext4_lookup: deleted inode referenced
/usr/bin/bash: error while loading shared libraries: invalid ELF header
```

Every one of those reads as a corrupted image, a broken `/var` drain, or a mangled commit.
It is none of them — the file on disk is byte-perfect throughout, and `sha256sum` says so
while the guest cannot execute it.

It costs the test nothing. Nothing here depends on guest-side persistence: `kiln apply` and
`kiln rollback` run on the host, and everything the probe checks is restored from the image
on every boot by design. If anything it is stricter, because each boot is then a first boot
onto a bare `/var` — the path that actually has to work.

Two narrower guards remain, because a single writer is worth checking rather than assuming:
the harness *settles* at every handover (flush everything dirty, drop the page cache, so the
next reader has to read the disk), and runs `e2fsck -fn` between phases so that if the image
ever does stop being consistent, the failure names the moment instead of the symptom two
minutes later.

Written down here because it cost an afternoon of reading it as a `/var` drain bug.
