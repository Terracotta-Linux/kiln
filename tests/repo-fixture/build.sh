#!/usr/bin/env bash
# Builds the fixture pacman repository. Solver and transaction
# tests use a real tiny local repo built in-tree, never the network.
#
# Everything here is offline: no `source=()` entry fetches anything, and the
# repo is a genuine pacman repo — real .pkg.tar.zst archives with real .PKGINFO
# and .MTREE, and a real repo-add database — so libalpm exercises its actual
# code paths rather than a mock.
#
#   ./build.sh          build into ./repo (idempotent; skips if up to date)
#   ./build.sh --force  rebuild from scratch
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
out=$here/repo
work=$here/.work
arch=$(uname -m)

[[ ${1-} == --force ]] && rm -rf "$out" "$work"

# The stamp is the hash of this script: the fixture is a pure function of it, so
# a changed script rebuilds and an unchanged one costs nothing.
stamp=$(b3sum "$0" 2>/dev/null | cut -d' ' -f1 || sha256sum "$0" | cut -d' ' -f1)
if [[ -f $out/.stamp && $(cat "$out/.stamp") == "$stamp" ]]; then
    echo "repo-fixture: up to date"
    exit 0
fi

rm -rf "$out" "$work"
mkdir -p "$out" "$work"

# Deterministic packaging. No signing, no compression variance, no stripping
# (there are no binaries), and a pinned epoch so two runs agree.
cat > "$work/makepkg.conf" <<EOF
CARCH="$arch"
CHOST="$arch-pc-linux-gnu"
PKGEXT='.pkg.tar.zst'
SRCEXT='.src.tar.gz'
COMPRESSZST=(zstd -c -T0 --ultra -20 -)
BUILDENV=(!distcc !color !ccache !check !sign)
OPTIONS=(!strip !docs !libtool !staticlibs !emptydirs !zipman !purge !debug)
INTEGRITY_CHECK=(sha256)
EOF
export SOURCE_DATE_EPOCH=0

# build <dir-with-PKGBUILD>
build() {
    local dir=$1
    ( cd "$dir" && makepkg --config "$work/makepkg.conf" --nodeps --noconfirm --clean \
        >"$work/$(basename "$dir").log" 2>&1 ) \
        || { echo "makepkg failed for $dir:"; tail -30 "$work/$(basename "$dir").log"; exit 1; }
    mv "$dir"/*.pkg.tar.zst "$out/"
}

pkg() { # pkg <name>  — starts a package directory, echoes its path
    local d=$work/$1
    mkdir -p "$d"
    echo "$d"
}

# --- fixture-filesystem -----------------------------------------------------
# Mirrors what Arch's `filesystem` owns, because that ownership is the reason
# the transaction has to be split and the reason the
# top-level usr-merge symlinks cannot be laid down first (N1).
d=$(pkg fixture-filesystem)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-filesystem
pkgver=1.0
pkgrel=1
pkgdesc="Base directory layout and account files"
arch=('any')
license=('MIT')
# The assembler installs `filesystem` alone, first, by name (step 2). Every
# fixture package is prefixed so it cannot be confused with a real one, so this
# is how the fixture answers to the name the assembler asks for.
provides=('filesystem')
package() {
  install -dm755 "$pkgdir"/{usr/bin,usr/lib,usr/share,etc,var/lib,var/log,var/cache}
  install -dm755 "$pkgdir"/{home,opt,srv,root}
  install -dm755 "$pkgdir/srv/http"
  printf 'root:x:0:0:root:/root:/bin/sh\n' > "$pkgdir/etc/passwd"
  printf 'root:x:0:\n' > "$pkgdir/etc/group"
  printf 'root:!*::\n' > "$pkgdir/etc/shadow"
  chmod 600 "$pkgdir/etc/shadow"
  printf '# fixture\n' > "$pkgdir/etc/fstab"
  # libostree titles the boot entry from ID or PRETTY_NAME and refuses to
  # deploy a tree that has neither, so an image without this commits and then
  # fails. Arch's `filesystem` package ships it, and so does this one.
  cat > "$pkgdir/usr/lib/os-release" <<'OSREL'
NAME="Kiln fixture"
ID=kiln-fixture
PRETTY_NAME="Kiln fixture"
OSREL
}
EOF
build "$d"

# --- fixture-libfoo 1.2 and 1.3 --------------------------------------------
# Two versions so change detection has something real to detect. 1.3
# lands in repo-next, which a test registers instead of `fixture`.
for v in 1.2 1.3; do
  d=$(pkg "fixture-libfoo-$v")
  cat > "$d/PKGBUILD" <<EOF
pkgname=fixture-libfoo
pkgver=$v
pkgrel=1
pkgdesc="A library"
arch=('any')
license=('MIT')
package() {
  install -dm755 "\$pkgdir/usr/lib"
  printf 'libfoo $v\n' > "\$pkgdir/usr/lib/libfoo.so"
  install -dm755 "\$pkgdir/usr/include"
  printf '#define FOO $v\n' > "\$pkgdir/usr/include/foo.h"
}
EOF
  build "$d"
done
mkdir -p "$out/next"
mv "$out/fixture-libfoo-1.3-1-any.pkg.tar.zst" "$out/next/"

# --- fixture-app ------------------------------------------------------------
# Depends on libfoo, and ships a scriptlet, because scriptlet output capture and
# scriptlet failure handling are both things this fixture exists to make testable.
d=$(pkg fixture-app)
cat > "$d/fixture-app.install" <<'EOF'
post_install() {
  echo "fixture-app: post_install ran"
  systemctl daemon-reload || true
}
post_upgrade() { post_install; }
EOF
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-app
pkgver=2.0
pkgrel=1
pkgdesc="An application"
arch=('any')
license=('MIT')
depends=('fixture-libfoo')
install=fixture-app.install
package() {
  install -dm755 "$pkgdir/usr/bin"
  printf '#!/bin/sh\necho app\n' > "$pkgdir/usr/bin/fixture-app"
  chmod 755 "$pkgdir/usr/bin/fixture-app"
  install -dm755 "$pkgdir/etc"
  printf 'setting = 1\n' > "$pkgdir/etc/fixture-app.conf"
}
EOF
build "$d"

# --- fixture-alt ------------------------------------------------------------
# provides/conflicts, so the solver's real resolution is what gets tested
# rather than a name lookup.
d=$(pkg fixture-alt)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-alt
pkgver=1.0
pkgrel=1
pkgdesc="An alternative application"
arch=('any')
license=('MIT')
depends=('fixture-libfoo')
provides=('fixture-app=2.0' 'fixture-editor')
conflicts=('fixture-app')
package() {
  install -dm755 "$pkgdir/usr/bin"
  printf '#!/bin/sh\necho alt\n' > "$pkgdir/usr/bin/fixture-app"
  chmod 755 "$pkgdir/usr/bin/fixture-app"
}
EOF
build "$d"

# --- fixture-sysuser --------------------------------------------------------
# A package that creates a service account, which is what UID pinning
# exists to keep stable across generations.
d=$(pkg fixture-sysuser)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-sysuser
pkgver=1.0
pkgrel=1
pkgdesc="A daemon with a service account"
arch=('any')
license=('MIT')
package() {
  install -dm755 "$pkgdir/usr/lib/sysusers.d"
  printf 'u fixture-daemon - "Fixture daemon" /var/lib/fixture\ng fixture-group -\n' \
    > "$pkgdir/usr/lib/sysusers.d/fixture.conf"
  install -dm755 "$pkgdir/usr/lib/systemd/system"
  printf '[Unit]\nDescription=Fixture\n[Service]\nExecStart=/usr/bin/true\n[Install]\nWantedBy=multi-user.target\n' \
    > "$pkgdir/usr/lib/systemd/system/fixture.service"
}
EOF
build "$d"

# --- fixture-varpayload -----------------------------------------------------
# All three cases of the /var drain in one package:
# a directory, a regular file, and a relative symlink.
d=$(pkg fixture-varpayload)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-varpayload
pkgver=1.0
pkgrel=1
pkgdesc="A package with a real /var payload"
arch=('any')
license=('MIT')
package() {
  install -dm755 "$pkgdir/var/lib/fixture"
  printf 'seed\n' > "$pkgdir/var/lib/fixture/seed.db"
  install -dm700 "$pkgdir/var/lib/fixture/private"
  install -dm755 "$pkgdir/var/log/fixture"
  install -dm755 "$pkgdir/var"
  ln -s ../run/fixture "$pkgdir/var/fixture-run"
  install -dm755 "$pkgdir/srv/fixture"
  printf 'served\n' > "$pkgdir/srv/fixture/index.html"
}
EOF
build "$d"

# --- fixture-tool -----------------------------------------------------------
# A statically linked binary that appends its arguments to a file.
#
# It exists because an alpm hook and an `.INSTALL` scriptlet both run *chrooted
# into the image*, and a fixture root contains no shell and no libc. A hook
# whose Exec is a static binary the fixture itself ships can therefore actually
# run — which is what makes hook shadowing testable rather than
# merely described. Scriptlets always go through /bin/sh and so remain untested
# on their success path here; that is covered by the real-image test, where the
# image has bash.
command -v gcc >/dev/null || { echo "repo-fixture needs gcc for fixture-tool"; exit 1; }
d=$(pkg fixture-tool)
cat > "$d/tool.c" <<'EOF'
/* Appends argv[2..] to the file named by argv[1]. Static, so it runs in a
   chroot containing nothing else at all. */
#include <fcntl.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    int fd = open(argv[1], O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (fd < 0) return 1;
    for (int i = 2; i < argc; i++) {
        write(fd, argv[i], __builtin_strlen(argv[i]));
        write(fd, " ", 1);
    }
    write(fd, "\n", 1);
    return 0;
}
EOF
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-tool
pkgver=1.0
pkgrel=1
pkgdesc="A static helper that runs inside an otherwise-empty chroot"
arch=('x86_64')
license=('MIT')
build() { gcc -static -Os -o fixture-tool "$startdir/tool.c"; }
package() {
  install -Dm755 fixture-tool "$pkgdir/usr/bin/fixture-tool"
}
EOF
build "$d"

# --- fixture-hook -----------------------------------------------------------
# A package-shipped alpm hook. It always runs and can only be
# shadowed by filename, which kiln-image depends on.
d=$(pkg fixture-hook)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-hook
pkgver=1.0
pkgrel=1
pkgdesc="A package that ships an alpm hook"
arch=('any')
license=('MIT')
depends=('fixture-tool')
package() {
  install -dm755 "$pkgdir/usr/share/libalpm/hooks"
  # Triggered by the library rather than by a binary, so the hook fires without
  # dragging in fixture-app's deliberately-broken scriptlet.
  cat > "$pkgdir/usr/share/libalpm/hooks/99-fixture.hook" <<'HOOK'
[Trigger]
Operation = Install
Operation = Upgrade
Type = Path
Target = usr/lib/libfoo.so

[Action]
Description = Fixture hook writing runtime state into the image
When = PostTransaction
Exec = /usr/bin/fixture-tool /fixture-hook-ran the-package-hook
HOOK
}
EOF
build "$d"

# --- fixture-linux ----------------------------------------------------------
# A kernel, shaped like a modern Arch one: vmlinuz and pkgbase already live in
# /usr/lib/modules/$kver, so the kernel-placement step is a no-op.
d=$(pkg fixture-linux)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-linux
pkgver=6.19
pkgrel=1
pkgdesc="A kernel"
arch=('any')
license=('MIT')
package() {
  local kver=6.19.0-fixture
  install -dm755 "$pkgdir/usr/lib/modules/$kver"
  printf 'not really a kernel
' > "$pkgdir/usr/lib/modules/$kver/vmlinuz"
  printf 'fixture-linux
' > "$pkgdir/usr/lib/modules/$kver/pkgbase"
  printf '# modules
' > "$pkgdir/usr/lib/modules/$kver/modules.order"
}
EOF
build "$d"

# --- fixture-linux-headers --------------------------------------------------
# Headers are a *build-time* dependency installed inside the sandbox, not
# image content. An out-of-tree module's build root needs them, so the fixture
# has to have them for that path to be testable at all.
d=$(pkg fixture-linux-headers)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-linux-headers
pkgver=6.19
pkgrel=1
pkgdesc="Headers for building modules against fixture-linux"
arch=('any')
license=('MIT')
package() {
  local kver=6.19.0-fixture
  install -dm755 "$pkgdir/usr/lib/modules/$kver/build"
  printf 'VERSION = 6
' > "$pkgdir/usr/lib/modules/$kver/build/Makefile"
}
EOF
build "$d"

# --- fixture-init -----------------------------------------------------------
# Something has to be PID 1. It provides the virtual name `init`, which is what
# the bootability check actually asks for.
d=$(pkg fixture-init)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-init
pkgver=1.0
pkgrel=1
pkgdesc="An init"
arch=('any')
license=('MIT')
provides=('init')
package() {
  install -dm755 "$pkgdir/usr/lib/systemd/system"
  printf '#!/bin/sh
exec /bin/sh
' > "$pkgdir/usr/lib/systemd/systemd"
  chmod 755 "$pkgdir/usr/lib/systemd/systemd"
  printf '[Unit]
Description=Multi-User System
'     > "$pkgdir/usr/lib/systemd/system/multi-user.target"
}
EOF
build "$d"

# --- fixture-clash-a / fixture-clash-b --------------------------------------
# A genuine conflict: two packages that refuse to coexist and neither of which
# provides the other. The distinction matters — a package that *provides* what
# it conflicts with is a replacement, and libalpm silently drops the replaced
# one from the target list instead of erroring (see fixture-alt).
for side in a b; do
  other=$([[ $side == a ]] && echo b || echo a)
  d=$(pkg "fixture-clash-$side")
  cat > "$d/PKGBUILD" <<EOF
pkgname=fixture-clash-$side
pkgver=1.0
pkgrel=1
pkgdesc="Conflicts with fixture-clash-$other"
arch=('any')
license=('MIT')
conflicts=('fixture-clash-$other')
package() {
  install -dm755 "\$pkgdir/usr/share/fixture"
  printf '$side\n' > "\$pkgdir/usr/share/fixture/clash-$side"
}
EOF
  build "$d"
done

# --- fixture-broken ---------------------------------------------------------
# Depends on something no repository provides, so the unsatisfiable path has a
# test rather than a comment. Nothing else depends on it.
d=$(pkg fixture-broken)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-broken
pkgver=1.0
pkgrel=1
pkgdesc="Depends on something that does not exist"
arch=('any')
license=('MIT')
depends=('fixture-nonexistent')
package() {
  install -dm755 "$pkgdir/usr/share/doc/fixture-broken"
}
EOF
build "$d"

# --- base-devel -------------------------------------------------------------
# Named exactly as Arch names it, because kiln-resolve puts `base-devel` in
# every build root and a fixture that called it something else would be
# testing a different code path. Note it is a real *package* in current Arch —
# it was a package group until 2022, which is a trap worth not falling into.
d=$(pkg base-devel)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=base-devel
pkgver=1
pkgrel=2
pkgdesc="Tools for building packages"
arch=('any')
license=('MIT')
depends=('fixture-tool')
package() { :; }
EOF
build "$d"

# --- fixture-base -----------------------------------------------------------
# The meta package, so a config can say one name and get a graph.
d=$(pkg fixture-base)
cat > "$d/PKGBUILD" <<'EOF'
pkgname=fixture-base
pkgver=1.0
pkgrel=1
pkgdesc="Minimal fixture base"
arch=('any')
license=('MIT')
depends=('fixture-filesystem' 'fixture-libfoo')
package() { :; }
EOF
build "$d"

# --- databases --------------------------------------------------------------
repo-add --quiet "$out/fixture.db.tar.gz" "$out"/*.pkg.tar.zst
cp "$out"/fixture-libfoo-1.2-*.pkg.tar.zst "$out/next/" 2>/dev/null || true
# repo-next is `fixture` with libfoo upgraded: the same packages, one newer.
for p in "$out"/*.pkg.tar.zst; do
    case $(basename "$p") in fixture-libfoo-*) continue ;; esac
    cp "$p" "$out/next/"
done
rm -f "$out/next"/fixture-libfoo-1.2-*.pkg.tar.zst
repo-add --quiet "$out/next/fixture.db.tar.gz" "$out"/next/*.pkg.tar.zst

echo "$stamp" > "$out/.stamp"
rm -rf "$work"

echo "repo-fixture: built $(ls "$out"/*.pkg.tar.zst | wc -l) packages in $out"
