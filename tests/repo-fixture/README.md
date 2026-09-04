# The fixture repository

A real, tiny pacman repository, built in-tree by `build.sh`. Solver and transaction tests
use this, **never the network**.

It is a genuine repository — real `.pkg.tar.zst` archives with real `.PKGINFO`
and `.MTREE`, and a real `repo-add` database — so libalpm exercises its actual
code paths. A mock would test the mock.

```
./build.sh          build into ./repo (idempotent; a stamp skips a no-op run)
./build.sh --force  rebuild from scratch
```

`repo/` is generated and git-ignored; the script that produces it is the source
of truth. Tests call `build.sh` themselves, so `cargo test` needs no setup.

## What each package is for

Every package exists to make one specific behaviour testable. Nothing here is
filler.

| Package | Exists because |
|---|---|
| `fixture-filesystem` | owns `/etc/passwd`, `/etc/group`, and `/home`, `/opt`, `/srv`, `/root` as real directories — the ownership that forces the transaction to be split and the top-level usr-merge symlinks to be deferred |
| `base-devel` | named exactly as Arch names it, because `kiln-resolve` puts `base-devel` in every build root. It is a real *package* in current Arch — it was a package group until 2022 |
| `fixture-base` | a meta package, so a config can name one thing and get a graph |
| `fixture-libfoo` | a plain dependency, built at **1.2** in `repo/` and **1.3** in `repo/next/` so change detection has a real version bump to detect |
| `fixture-app` | depends on `fixture-libfoo` and ships an `.INSTALL` scriptlet that calls `systemctl daemon-reload` — scriptlet output capture and shimming |
| `fixture-alt` | `provides` **and** `conflicts` with `fixture-app`: libalpm treats that as a *replacement* and resolves silently. Also provides the virtual name `fixture-editor`, so provides-resolution is tested apart from name lookup |
| `fixture-clash-a` / `-b` | a genuine conflict — they refuse to coexist and neither provides the other, which is the case that actually errors |
| `fixture-broken` | depends on something no repository provides, so the unsatisfiable path is tested rather than described |
| `fixture-linux` | a kernel with `vmlinuz` and `pkgbase` already under `/usr/lib/modules/$kver`, the way a modern Arch kernel ships them |
| `fixture-linux-headers` | headers are a *build-time* dependency installed in the sandbox rather than image content; an out-of-tree module's build root needs them |
| `fixture-init` | `provides=('init')` — what the bootability check actually asks for |
| `fixture-sysuser` | ships `sysusers.d` and a systemd unit — UID pinning and unit enablement |
| `fixture-varpayload` | ships a `/var` directory, a `/var` file, a *relative symlink* under `/var`, and `/srv` content: all three cases of the drain plus the relocation of a top-level directory into it |
| `fixture-hook` | ships an alpm hook in `/usr/share/libalpm/hooks`, which always runs and can only be shadowed by filename |
| `fixture-tool` | a **statically linked** binary that appends its arguments to a file. Both hooks and `.INSTALL` scriptlets run chrooted into the image, and a fixture root has no shell and no libc — so a hook whose `Exec` is this can actually run, which is what makes hook shadowing testable rather than merely described. It is also why the fixture build needs `gcc`. |

## `repo/next/`

The same package set with `fixture-libfoo` upgraded to 1.3. Registering it
instead of `repo/` is "the mirrors moved" — which is what `kiln check` exists to
notice.

## What is deliberately *not* testable here

Scriptlets always run through `/bin/sh`, and the fixture root has no shell, so
an `.INSTALL` **succeeding** cannot be tested with this repository. That is not
a gap so much as a redirection: the failure path is the risky one, and it *is*
tested — `fixture-app`'s scriptlet cannot run, and Kiln must turn that into an
aborted build rather than the silent success libalpm reports. The success path
belongs to the real-image test, where the image contains bash.

## Determinism

`build.sh` pins `SOURCE_DATE_EPOCH=0` and its own `makepkg.conf`, and stamps the
output with a hash of itself. Editing the script rebuilds; running it twice does
not.
