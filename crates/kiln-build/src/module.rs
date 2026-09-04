//! Out-of-tree kernel modules, as recipes.
//!
//! > `[[kernel.module]]` entries are compiled against the exact kernel in the
//! > image and packaged. Kiln synthesizes a PKGBUILD-equivalent recipe and runs
//! > it through the normal sandbox, so modules get the same caching,
//! > isolation and failure reporting as everything else.
//!
//! So there is no module builder here, only a *recipe writer*. Everything after
//! this file — the two phases, the build key, the cache, the failure report —
//! is the same code a `packages.build` entry goes through, which is the whole
//! point: a module is not a second kind of build.
//!
//! The synthesized recipe finds the kernel version by looking for the headers
//! in the build root rather than being told it. The build root holds exactly
//! one `linux-headers`, installed from the same snapshot the image resolved
//! from, so `/usr/lib/modules/*/build` names the kernel the module is being
//! built for — and a recipe that reads it cannot disagree with the root it is
//! running in, which a substituted version string could.

use std::fmt;
use std::path::{Path, PathBuf};

/// Write a module's source tree and a synthesized PKGBUILD into `into`.
///
/// The tree is **copied**, not linked or built in place: this keeps the
/// configuration root somewhere Kiln reads and never writes, and `makepkg`
/// writes to the directory it is run from.
pub fn materialize(
    name: &str,
    source: &Path,
    into: &Path,
    arch: &str,
    kernel_package: &str,
    kernel_evr: &str,
) -> Result<PathBuf, Error> {
    let _ = std::fs::remove_dir_all(into);
    if let Some(parent) = into.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            doing: "creating the module build directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    copy_tree(source, into)?;

    let pkgbuild = pkgbuild(name, arch, kernel_package, kernel_evr);
    write(&into.join("PKGBUILD"), &pkgbuild)?;
    // Written rather than generated: `Recipe::read` falls back to
    // `makepkg --printsrcinfo` in a sandbox when a recipe ships none, and
    // spending a sandbox to re-read a file Kiln wrote thirty lines ago would be
    // asking bash what Kiln already knows.
    write(
        &into.join(".SRCINFO"),
        &srcinfo(name, arch, kernel_package, kernel_evr),
    )?;
    Ok(into.to_path_buf())
}

/// `pkgver` may not contain `-` or `:`, and a kernel EVR contains both.
pub fn version_of(kernel_evr: &str) -> String {
    kernel_evr.replace([':', '-'], "_")
}

fn pkgbuild(name: &str, arch: &str, kernel_package: &str, kernel_evr: &str) -> String {
    let version = version_of(kernel_evr);
    format!(
        r#"# Synthesized by Kiln. Do not edit: this file is written
# fresh into a scratch directory on every build, from the module's source tree
# and the kernel the plan resolved.
pkgname={name}
pkgver={version}
pkgrel=1
pkgdesc="Out-of-tree kernel module {name}, built against {kernel_package} {kernel_evr}"
arch=('{arch}')
license=('unknown')
makedepends=('{kernel_package}-headers')
# The module is the artifact. Stripping it removes the symbols modprobe and
# every crash dump want, and Arch's own module packages set this too.
options=('!strip')

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

prepare() {{
  rm -rf "$srcdir/{name}"
  cp -a "$startdir/." "$srcdir/{name}"
  # The recipe Kiln wrote is not part of the module's source.
  rm -f "$srcdir/{name}/PKGBUILD" "$srcdir/{name}/.SRCINFO"
}}

build() {{
  local kver
  kver=$(_kernelrelease)
  make -C "/usr/lib/modules/$kver/build" M="$srcdir/{name}" modules
}}

package() {{
  local kver
  kver=$(_kernelrelease)
  # Inside the kernel's own module directory rather than a sibling of it:
  # Kiln finds the kernel version by looking for `/usr/lib/modules/*/pkgbase`
  # and refuses to guess when it finds two directories, so a module that adds a
  # top-level one would break the step that places the kernel.
  local dest="$pkgdir/usr/lib/modules/$kver/extramodules"
  install -dm755 "$dest"
  find "$srcdir/{name}" -name '*.ko' -exec install -m644 -t "$dest" {{}} +
  # An empty install is a build that compiled nothing, which `make` is happy to
  # call success. Better to say so here than to ship an image missing a driver.
  if [[ -z $(ls -A "$dest") ]]; then
    echo "==> ERROR: {name} built no .ko files" >&2
    return 1
  fi
}}
"#
    )
}

/// The same facts in `.SRCINFO` form. `makedepends` is the field that matters:
/// it is what puts `linux-headers` in the build root, and its resolved EVR is
/// what folds into the build key.
fn srcinfo(name: &str, arch: &str, kernel_package: &str, kernel_evr: &str) -> String {
    format!(
        "pkgbase = {name}\n\
         \tpkgver = {}\n\
         \tpkgrel = 1\n\
         \tarch = {arch}\n\
         \tmakedepends = {kernel_package}-headers\n\
         \npkgname = {name}\n",
        version_of(kernel_evr)
    )
}

fn write(path: &Path, text: &str) -> Result<(), Error> {
    std::fs::write(path, text).map_err(|source| Error::Io {
        doing: "writing the synthesized recipe",
        path: path.to_path_buf(),
        source,
    })
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    let out = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .output()
        .map_err(|source| Error::Io {
            doing: "copying the module source from",
            path: from.to_path_buf(),
            source,
        })?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Copy {
        from: from.to_path_buf(),
        why: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

#[derive(Debug)]
pub enum Error {
    Copy {
        from: PathBuf,
        why: String,
    },
    Io {
        doing: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Copy { from, why } => {
                write!(f, "copying the module source at {}: {why}", from.display())
            }
            Error::Io {
                doing,
                path,
                source,
            } => write!(f, "{doing} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}
