//! DKMS packages, as recipes.
//!
//! A DKMS package — `nvidia-open-dkms` and its kind — ships no compiled module.
//! It ships a source tree under `/usr/src/<name>-<version>` and a `dkms.conf`,
//! and expects `dkms` to compile it *on the machine, at install time, against
//! the running kernel*. An immutable image has no such moment: there is no
//! install time on the target, no headers in the image, and nothing writable
//! under `/usr/lib/modules` to write the result into.
//!
//! So Kiln moves that moment. The sources are put where `dkms` can reach them —
//! a DKMS **package** installed into a build root that is never the image, or a
//! **directory in the configuration** copied into the build the way
//! `[[kernel.module]]`'s source is — `dkms build` runs there against the exact
//! kernel the plan resolved, and the `.ko` files it produces are packaged and
//! installed like anything else. What ships is the compiled driver, and the
//! ~1 GB of sources that produced it stays behind.
//!
//! The two differ only in where `$source_tree` points and in how the sources get
//! there. Everything after that — `dkms add`, `dkms build`, the copy out of the
//! DKMS tree — is one code path, because it is one operation.
//!
//! As with [`crate::module`], there is no DKMS builder here — only a *recipe
//! writer*. Everything after this file is the same code a `packages.build`
//! entry goes through: the two phases, the build key, the cache, the failure
//! report. A DKMS package is not a third kind of build.
//!
//! Three facts about `dkms` shape the recipe, all of them checked against
//! dkms 3.4.3:
//!
//! - `dkms add` and `dkms build` need write access to the DKMS tree, not root
//!   (`check_rw_dkms_tree`), so `--dkmstree` under `$srcdir` lets both run as
//!   the unprivileged build user. `dkms install` *does* demand root, which is
//!   why `package()` copies the modules itself rather than calling it.
//! - On Arch, `dkms` rewrites every `DEST_MODULE_LOCATION` to `/updates/dkms`
//!   regardless of what a `dkms.conf` asks for (`override_dest_module_location`
//!   keys off `/etc/os-release`). Copying to `updates/dkms` is therefore not an
//!   approximation of what `dkms install` would do — it is the same
//!   destination, reached without needing root inside `fakeroot`.
//! - Module signing would generate a machine key on first use and sign with it.
//!   An image has to be a function of its inputs, so signing is turned off
//!   explicitly rather than left to `dkms`'s in-a-chroot guess.

use std::fmt;
use std::path::{Path, PathBuf};

/// The name of the package Kiln builds out of a DKMS package.
///
/// Not the DKMS package's own name: what comes out is a different artifact —
/// compiled modules, no sources, no `dkms.conf` — and a package claiming to be
/// `nvidia-open-dkms` while containing none of it would make `pacman -Q` on a
/// booted image lie. Not the stripped name either (`nvidia-open-dkms` →
/// `nvidia-open`), because that one is a real package in the repositories and
/// would collide with itself.
///
/// Shared between resolution, which puts it in the plan, and realization, which
/// writes it into the recipe — the two must agree or the artifact would be
/// filed under a name nothing looks up.
pub fn package_name(package: &str) -> String {
    format!("{package}-modules")
}

/// Where a DKMS driver's sources come from.
///
/// The distinction is real work, not a label: a package has a version and lands
/// in the build root through `makedepends`, a tree has a content hash and is
/// copied in. Everything downstream is the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sources<'a> {
    /// A DKMS package — installed into the build root, never into the image.
    Package { name: &'a str, evr: &'a str },
    /// A directory in the configuration tree that ships a `dkms.conf`.
    Tree { name: &'a str, dir: &'a Path },
}

impl Sources<'_> {
    /// The name the entry was written under. For a package it is the package;
    /// for a tree it is a label the configuration chose.
    pub fn name(&self) -> &str {
        match self {
            Sources::Package { name, .. } | Sources::Tree { name, .. } => name,
        }
    }
}

/// Write a synthesized PKGBUILD for a DKMS driver into `into`.
///
/// For [`Sources::Tree`] the tree is **copied**, not linked or built in place,
/// for the same reason [`crate::module::materialize`] copies one: the
/// configuration root is somewhere Kiln reads and never writes, and `makepkg`
/// writes to the directory it is run from.
pub fn materialize(
    sources: &Sources<'_>,
    into: &Path,
    arch: &str,
    kernel_package: &str,
    kernel_evr: &str,
) -> Result<PathBuf, Error> {
    let _ = std::fs::remove_dir_all(into);
    match sources {
        Sources::Package { .. } => {
            std::fs::create_dir_all(into).map_err(|source| Error::Io {
                doing: "creating the DKMS build directory",
                path: into.to_path_buf(),
                source,
            })?;
        }
        Sources::Tree { dir, .. } => {
            if let Some(parent) = into.parent() {
                std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                    doing: "creating the DKMS build directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            crate::module::copy_tree(dir, into).map_err(Error::Module)?;
            // A `dkms.conf` is the whole difference between this and a
            // `[[kernel.module]]` tree, and finding out it is missing twenty
            // minutes into a build root would be finding out too late.
            if !into.join("dkms.conf").is_file() {
                return Err(Error::NotDkms {
                    dir: dir.to_path_buf(),
                });
            }
        }
    }

    write(
        &into.join("PKGBUILD"),
        &pkgbuild(sources, arch, kernel_package, kernel_evr),
    )?;
    // Written rather than generated, for the same reason `module` writes one:
    // `Recipe::read` would otherwise spend a sandbox running
    // `makepkg --printsrcinfo` to re-read a file Kiln wrote thirty lines ago.
    write(
        &into.join(".SRCINFO"),
        &srcinfo(sources, arch, kernel_package, kernel_evr),
    )?;
    Ok(into.to_path_buf())
}

/// The names a DKMS build root has to hold, beyond `base-devel`.
///
/// `dkms` itself, the kernel's headers, and — when the sources come from one —
/// the package carrying them. Named in one place because resolution folds their
/// resolved versions into the build key and realization installs them; a build
/// root that held a different set from the one the key describes is the
/// silently-wrong artifact the whole key exists to prevent.
pub fn makedepends(sources: &Sources<'_>, kernel_package: &str) -> Vec<String> {
    let mut out = vec![DKMS.to_string(), format!("{kernel_package}-headers")];
    if let Sources::Package { name, .. } = sources {
        out.push((*name).to_string());
    }
    out
}

/// The package providing `/usr/bin/dkms`.
pub const DKMS: &str = "dkms";

/// Where `dkms` puts a built module on Arch, and therefore where this ships it.
/// `updates/` is ahead of the kernel's own `kernel/` in modprobe's search
/// order, which is what lets a DKMS driver shadow an in-tree one of the same
/// name.
pub const DEST: &str = "updates/dkms";

fn pkgbuild(sources: &Sources<'_>, arch: &str, kernel_package: &str, kernel_evr: &str) -> String {
    let name = package_name(sources.name());
    let deps = makedepends(sources, kernel_package)
        .iter()
        .map(|d| format!("'{d}'"))
        .collect::<Vec<_>>()
        .join(" ");
    let (version, pkgdesc, sourcetree, stage) = match sources {
        Sources::Package { name, evr } => (
            crate::module::version_of(evr),
            format!("DKMS modules from {name} {evr}, built against {kernel_package} {kernel_evr}"),
            // The package unpacked its sources here, and `dkms`'s own default.
            "/usr/src".to_string(),
            String::new(),
        ),
        Sources::Tree { name, .. } => (
            // A tree in the configuration has no version of its own — the
            // driver's `dkms.conf` names one, but that is only known once the
            // build is running. The kernel EVR is the honest answer to "which
            // build of this is it", and is what `[[kernel.module]]` uses too.
            crate::module::version_of(kernel_evr),
            format!("DKMS modules from {name}, built against {kernel_package} {kernel_evr}"),
            // Not /usr/src: the build user cannot write there, and `dkms add`
            // needs the sources at `$source_tree/$module-$version` — a name
            // only the driver's own dkms.conf knows.
            "$srcdir/src".to_string(),
            STAGE.to_string(),
        ),
    };
    format!(
        r#"# Synthesized by Kiln. Do not edit: this file is written
# fresh into a scratch directory on every build, from the DKMS sources the plan
# resolved and the kernel it resolved alongside them.
pkgname={name}
pkgver={version}
pkgrel=1
pkgdesc="{pkgdesc}"
arch=('{arch}')
license=('unknown')
makedepends=({deps})
# The module is the artifact. Stripping it removes the symbols modprobe and
# every crash dump want, and Arch's own module packages set this too.
options=('!strip')

# A DKMS tree of the build's own, so that `dkms add` and `dkms build` — which
# need a writable tree but not root — can run as the unprivileged build user.
# A function rather than a variable: `makepkg` sources this file once before
# `$srcdir` exists, and a top-level assignment would capture the empty string.
_dkmstree() {{
  echo "$srcdir/dkms"
}}

# Where `dkms` looks for `$module-$version`.
_sourcetree() {{
  echo "{sourcetree}"
}}

# The one kernel in the build root. This puts the resolved kernel EVR in the
# build key, so a root assembled for this key holds exactly one `-headers`
# package — and reading the version out of the root rather than substituting it
# in means the recipe cannot disagree with what it is compiling against.
_kernelrelease() {{
  local build
  for build in /usr/lib/modules/*/build; do
    if [[ -d $build ]]; then
      basename "$(dirname "$build")"
      return 0
    fi
  done
  echo "no kernel headers in the build root: /usr/lib/modules/*/build is empty" >&2
  return 1
}}

# The one `dkms.conf` in the source tree. Found by looking rather than by name:
# a DKMS source directory is `<module>-<version>`, which is neither the package
# name nor the package version and is not derivable from either.
_dkms_conf() {{
  local found=() conf
  for conf in "$(_sourcetree)"/*/dkms.conf; do
    [[ -f $conf ]] && found+=("$conf")
  done
  if (( ${{#found[@]}} == 0 )); then
    echo "==> ERROR: no dkms.conf under $(_sourcetree), so there is nothing DKMS can build" >&2
    return 1
  fi
  if (( ${{#found[@]}} > 1 )); then
    echo "==> ERROR: more than one DKMS source tree under $(_sourcetree):" >&2
    printf '  %s\n' "${{found[@]}}" >&2
    echo "Kiln builds one DKMS driver per build root and cannot tell which of these is it." >&2
    return 1
  fi
  echo "${{found[0]}}"
}}

# PACKAGE_NAME and PACKAGE_VERSION out of a dkms.conf. Sourced in a subshell,
# which is what `dkms` itself does with the same file moments later.
_dkms_field() {{
  ( source "$2" >/dev/null; printf '%s\n' "${{!1}}" )
}}
{stage}
build() {{
  local kver conf mod ver tree
  tree=$(_dkmstree)
  kver=$(_kernelrelease)
  conf=$(_dkms_conf)
  mod=$(_dkms_field PACKAGE_NAME "$conf")
  ver=$(_dkms_field PACKAGE_VERSION "$conf")
  if [[ -z $mod || -z $ver ]]; then
    echo "==> ERROR: $conf declares no PACKAGE_NAME/PACKAGE_VERSION" >&2
    return 1
  fi
  echo "==> building DKMS module $mod/$ver against kernel $kver"

  # Signing generates a machine-owned key on first use and signs with it. An
  # image is a function of its inputs, and a secret invented mid-build is not
  # one of them. `dkms` reads this from its framework config, which means an
  # exported value it does not override.
  export try_sign_modules=false

  mkdir -p "$tree"
  dkms add --dkmstree "$tree" --sourcetree "$(_sourcetree)" -m "$mod" -v "$ver"
  dkms build --dkmstree "$tree" --sourcetree "$(_sourcetree)" -m "$mod" -v "$ver" -k "$kver"
}}

package() {{
  local kver conf mod ver built dest tree
  tree=$(_dkmstree)
  kver=$(_kernelrelease)
  conf=$(_dkms_conf)
  mod=$(_dkms_field PACKAGE_NAME "$conf")
  ver=$(_dkms_field PACKAGE_VERSION "$conf")

  # `dkms install` is the other half of this and would demand root, which
  # `makepkg` does not have. It would also copy to exactly here: on Arch
  # `dkms` rewrites every DEST_MODULE_LOCATION to /updates/dkms.
  dest="$pkgdir/usr/lib/modules/$kver/{dest}"
  install -dm755 "$dest"
  for built in "$tree/$mod/$ver/$kver"/*/module/*.ko*; do
    [[ -f $built ]] || continue
    install -m644 -t "$dest" "$built"
  done
  # An empty install is a build that compiled nothing, which `make` — and
  # `dkms`, which wraps it — is happy to call success. Better to say so here
  # than to ship an image missing a driver.
  if [[ -z $(ls -A "$dest") ]]; then
    echo "==> ERROR: {label} built no kernel modules for $kver" >&2
    return 1
  fi
}}
"#,
        dest = DEST,
        label = sources.name(),
    )
}

/// `prepare()` for a source tree: put it where `dkms add` expects to find it.
///
/// Only the tree case has one. `dkms` insists the sources live at
/// `$source_tree/$module-$version`, and both halves of that name come out of
/// the driver's own `dkms.conf` — so the move cannot happen until the file has
/// been read, and it happens here rather than in `build()` so that a
/// `--keep-failed` root shows the same layout a working build had.
const STAGE: &str = r#"
prepare() {
  local mod ver staged
  staged="$srcdir/staged"
  rm -rf "$staged" "$(_sourcetree)"
  cp -a "$startdir/." "$staged"
  # The recipe Kiln wrote is not part of the driver's source.
  rm -f "$staged/PKGBUILD" "$staged/.SRCINFO"

  mod=$(_dkms_field PACKAGE_NAME "$staged/dkms.conf")
  ver=$(_dkms_field PACKAGE_VERSION "$staged/dkms.conf")
  if [[ -z $mod || -z $ver ]]; then
    echo "==> ERROR: dkms.conf declares no PACKAGE_NAME/PACKAGE_VERSION" >&2
    return 1
  fi
  mkdir -p "$(_sourcetree)"
  mv "$staged" "$(_sourcetree)/$mod-$ver"
}
"#;

/// The same facts in `.SRCINFO` form. `makedepends` is the field that matters:
/// it is what puts `dkms`, the kernel headers and — for a package — the sources
/// themselves in the build root, and their resolved EVRs are what fold into the
/// build key.
fn srcinfo(sources: &Sources<'_>, arch: &str, kernel_package: &str, kernel_evr: &str) -> String {
    let name = package_name(sources.name());
    // The same answer `pkgbuild` reaches, and it has to be: a `.SRCINFO` that
    // disagreed with the recipe beside it would be a lie Kiln told itself.
    let version = crate::module::version_of(match sources {
        Sources::Package { evr, .. } => evr,
        Sources::Tree { .. } => kernel_evr,
    });
    let mut out =
        format!("pkgbase = {name}\n\tpkgver = {version}\n\tpkgrel = 1\n\tarch = {arch}\n");
    for dep in makedepends(sources, kernel_package) {
        out.push_str(&format!("\tmakedepends = {dep}\n"));
    }
    out.push_str(&format!("\npkgname = {name}\n"));
    out
}

fn write(path: &Path, text: &str) -> Result<(), Error> {
    std::fs::write(path, text).map_err(|source| Error::Io {
        doing: "writing the synthesized recipe",
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug)]
pub enum Error {
    /// A source tree with no `dkms.conf` is a `[[kernel.module]]`, not a DKMS
    /// driver — and the two are built by different tools.
    NotDkms {
        dir: PathBuf,
    },
    Module(crate::module::Error),
    Io {
        doing: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotDkms { dir } => write!(
                f,
                "{} has no dkms.conf, so `dkms` has nothing to build there\n\n\
                 A source tree without one is an ordinary out-of-tree module: name it \
                 with `[[kernel.module]]` instead, which builds it with `make`.",
                dir.display()
            ),
            Error::Module(e) => write!(f, "{e}"),
            Error::Io {
                doing,
                path,
                source,
            } => write!(f, "{doing} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}
