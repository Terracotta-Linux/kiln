# kiln

A declarative Linux system image builder for Arch.

TOML in `/etc/kiln/` → an OSTree commit → a bootable deployment. You describe what is
inside your system; Kiln builds it, commits it, and stages it for the next boot. If the new
image is wrong, you reboot into the old one — it is still there, byte for byte.

```toml
# /etc/kiln/system.toml
kiln = 1
include = ["@kiln/profiles/workstation", "@kiln/desktop/gnome", "@kiln/gpu/amd"]

[packages]
repo = ["neovim", "fish", "firefox"]
```

```console
$ kiln check          # what would change, without building anything
$ kiln apply          # build it, stage it for next boot
$ reboot
$ kiln rollback       # if it was a mistake
```

**New here?** [`docs/GUIDE.md`](docs/GUIDE.md) is the user guide — the loop, the language,
the module library, every command, and what to do when something goes wrong.

## What it is

Kiln is **a distribution's build tool, not an image-shipping pipeline.** The loop is: write
config on your system → build on your system → deploy on your system → use your system.
There are no remotes, no registry, no push, no pull, and no fleet.

It answers exactly one question: *what is inside the image?* The test for whether something
belongs in Kiln is **if it changes, do you need a new image and a reboot?** If not, it is
out of scope — deliberately, permanently, and this is not an oversight:

- **No login accounts, dotfiles, or desktop settings.** Image content only.
- **No live-apply.** One image, one reboot. There is no `/etc`-only fast path.
- **No installation.** No `kiln install`, no ISO, no partitioning. `--sysroot` and
  `kiln sysroot init` let you build into a target that is not the running root; anything
  beyond that is a separate program's job, not Kiln's.
- **No implicit base.** An empty config produces an empty image. `@kiln/profiles/minimal`
  is the one-line answer.
- **TOML is data, not a language.** No variables, interpolation, conditionals, or
  inheritance. `include` is the only composition operator. Resisting "NixOS but with TOML"
  is a design goal, not a limitation to be fixed.

## Why it might interest you

- **Atomic and reversible.** Every build is a generation. `kiln rollback` boots the previous
  one, and a machine that fails to boot three times rolls itself back without you.
- **It stays Arch.** Real pacman packages from real Arch repositories, the AUR, your own
  PKGBUILDs, out-of-tree kernel modules, local `.pkg.tar.zst` files. `pacman -Q`, `kiln why`
  and `kiln owns` all work inside the booted image.
- **`kiln check` covers every input**, not only official packages — your files, your
  PKGBUILDs, your AUR pins and your configuration report in one place, and the fix is always
  one command.
- **It explains itself.** `kiln explain kernel.cmdline` says which file set a value and what
  it overrode. `kiln why firefox` says what pulled a package in. `kiln diff 41 42` says what
  changed between two generations, read from the commits rather than from a lockfile.
- **There is no lockfile.** Every commit carries its own build record. OSTree is already a
  versioned content-addressed store; a second source of truth could only disagree with it.

## Status

The frontend, the builder, the full package/build/module input taxonomy, and the command
surface described above are implemented and tested, including automatic rollback on boot
failure and `/etc` drift detection. The acceptance test boots generation 1, boots
generation 2, and rolls back to generation 1 in a real qemu VM, asserting every claim from
inside the running system.

Not built yet: reproducibility auditing and multi-arch.

## Building it

```console
$ cargo build --release
$ cargo test                        # nothing privileged
$ sudo -E cargo test -- --ignored   # transactions, assembly, ostree: need root
```

Until the module library is installed to `/usr/share/kiln/modules`, point at the tree:

```console
$ cargo run --bin kiln -- --config ./myconfig --module-root ./modules check --offline
```

The boot acceptance test — the only one that proves the project works, and the only one that
uses the network — takes about twenty minutes:

```console
$ sudo -E cargo test -p kiln-cli --test boot -- --ignored --nocapture
```

## Documentation

- [`docs/GUIDE.md`](docs/GUIDE.md) — the user guide: the loop, the language, the module
  library, every command, and what to do when something goes wrong.
- [`CLAUDE.md`](CLAUDE.md) — architecture, crate layout, and conventions, for anyone
  changing Kiln rather than running it.

## Contributing

Issues and pull requests are welcome. `CLAUDE.md` covers the architecture and the
conventions the codebase relies on — worth a read before a change that touches more than
one file. `cargo test` runs everything that doesn't need root; `sudo -E cargo test --
--ignored` runs the rest, including the boot acceptance test in a qemu VM.

## License

See [LICENSE](LICENSE).
