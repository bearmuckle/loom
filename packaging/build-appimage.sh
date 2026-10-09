#!/usr/bin/env bash
#
# Packages the native `loom-ui` client as a Linux AppImage.
#
# Usage:
#   ARCH=x86_64 TARGET=x86_64-unknown-linux-gnu \
#     LOOM_BUILD_VERSION=v0.8.1 packaging/build-appimage.sh [dist-dir]
#
# `ARCH` is the AppImage architecture (`x86_64` or `aarch64`) and must match the
# `TARGET` the binary was built for. The binary is expected at
# `target/${TARGET}/release/loom-ui` when `TARGET` is set, and at
# `target/release/loom-ui` otherwise. The resulting
# `loom-ui-<version>-<arch>.AppImage` is written to the dist directory
# (`<workspace>/dist` by default).
#
# `linuxdeploy` and `appimagetool` are used from PATH when present and are
# otherwise downloaded into `${LOOM_APPIMAGE_TOOLS_DIR}` (default
# `<workspace>/target/appimage-tools`). Both are AppImages themselves and the
# runners that build releases have no FUSE, so everything runs through AppImage's
# extract-and-run path: `APPIMAGE_EXTRACT_AND_RUN=1` is exported rather than
# `--appimage-extract-and-run` being passed on a command line, so that every
# process involved -- including the helpers linuxdeploy starts itself -- extracts
# instead of trying to mount an image.

set -euo pipefail

ARCH="${ARCH:?set ARCH to the AppImage architecture (x86_64 or aarch64)}"
TARGET="${TARGET:-}"
# The libraries every AppImage takes from the host system instead of bundling.
GLIBC_LIBRARIES="libc.so.6 libm.so.6 libgcc_s.so.1 libpthread.so.0 libdl.so.2 librt.so.1 libstdc++.so.6 ld-linux-x86-64.so.2 ld-linux-aarch64.so.1"
WORKSPACE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="${1:-"$WORKSPACE/dist"}"
VERSION="${LOOM_BUILD_VERSION:-dev}"
SUBDIR="${TARGET:+$TARGET/}"
RELEASE_DIR="$WORKSPACE/target/${SUBDIR}release"
BINARY="$RELEASE_DIR/loom-ui"
TOOLS_DIR="${LOOM_APPIMAGE_TOOLS_DIR:-"$WORKSPACE/target/appimage-tools"}"
BUILD_DIR="$WORKSPACE/target/${SUBDIR}appimage"
APPDIR="$BUILD_DIR/AppDir"
ICON="$WORKSPACE/crates/loom-ui/pwa/icon-512.png"

if [ ! -x "$BINARY" ]; then
  echo "missing executable $BINARY; build it first with" \
    "'cargo build --release --locked --package loom-ui --target ${TARGET:-<host>}'" >&2
  exit 1
fi

# -- Tooling ------------------------------------------------------------------

tool_path() {
  local name="$1" url="$2" path="$TOOLS_DIR/$1"
  if [ ! -x "$path" ]; then
    echo "downloading $name into $TOOLS_DIR" >&2
    curl -fsSL -o "$path" "$url"
    chmod +x "$path"
  fi
  printf '%s' "$path"
}

if command -v linuxdeploy >/dev/null 2>&1; then
  LINUXDEPLOY="$(command -v linuxdeploy)"
else
  mkdir -p "$TOOLS_DIR"
  LINUXDEPLOY="$(tool_path linuxdeploy \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage")"
fi

if command -v appimagetool >/dev/null 2>&1; then
  APPIMAGETOOL="$(command -v appimagetool)"
else
  mkdir -p "$TOOLS_DIR"
  APPIMAGETOOL="$(tool_path appimagetool \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage")"
fi

export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH
export PATH="$TOOLS_DIR:$PATH"
# appimagetool records the version in the image's desktop file
# (`X-AppImage-Version`) and appimagetool/AppImage tooling reads it from here.
export VERSION="${VERSION#v}"

# -- AppDir -------------------------------------------------------------------

# linuxdeploy requires the desktop file and the icon to be present in the
# AppDir, and appimagetool additionally reads them from the AppDir root. The
# file names also have to line up with the desktop file's `Exec`/`Icon` values,
# which is why the repo's `packaging/loom.desktop` is installed as
# `loom-ui.desktop` here and by the `.deb`.
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/512x512/apps" \
  "$APPDIR/usr/share/licenses/loom-ui" \
  "$DIST"
install -m755 "$BINARY" "$APPDIR/usr/bin/loom-ui"
install -m644 "$WORKSPACE/packaging/loom.desktop" "$APPDIR/usr/share/applications/loom-ui.desktop"
install -m644 "$WORKSPACE/packaging/loom.desktop" "$APPDIR/loom-ui.desktop"
install -m644 "$ICON" "$APPDIR/usr/share/icons/hicolor/512x512/apps/loom-ui.png"
install -m644 "$ICON" "$APPDIR/loom-ui.png"
ln -sf loom-ui.png "$APPDIR/.DirIcon"
install -m644 "$WORKSPACE/LICENSE-AGPL" "$APPDIR/usr/share/licenses/loom-ui/LICENSE-AGPL"
install -m644 "$WORKSPACE/LICENSES.md" "$APPDIR/usr/share/licenses/loom-ui/LICENSES.md"

# -- AppImage -----------------------------------------------------------------

# linuxdeploy deploys the executable into the AppDir, bundles the shared libraries
# it resolves, and appimagetool then assembles the image from that AppDir.
#
# linuxdeploy deliberately does not bundle the libraries on the AppImage
# excludelist. The glibc family there has to stay out of the image, but the other
# entries are real dependencies of the client -- `libxcb.so.1` for X11 and
# `libz.so.1` for the vendored libgit2 -- and an image without them fails to
# start with "error while loading shared libraries: libxcb.so.1" on a system that
# does not happen to have them. `--library` deploys each of those explicitly,
# together with its own dependencies.
libraries=()
while read -r soname path; do
  case " $GLIBC_LIBRARIES " in
    *" $soname "*) continue ;;
  esac
  # Already bundled by linuxdeploy.
  [ -e "$APPDIR/usr/lib/$soname" ] && continue
  libraries+=(--library "$path")
done < <(ldd "$APPDIR/usr/bin/loom-ui" | tr -s ' ' | awk '$2 == "=>" && $3 ~ /^\// { print $1, $3 }')

"$LINUXDEPLOY" \
  --appdir "$APPDIR" \
  --executable "$APPDIR/usr/bin/loom-ui" \
  --desktop-file "$APPDIR/usr/share/applications/loom-ui.desktop" \
  --icon-file "$APPDIR/usr/share/icons/hicolor/512x512/apps/loom-ui.png" \
  "${libraries[@]}"

# Naming the destination explicitly keeps the release file name stable instead of
# deriving it from the desktop file's `Name`.
OUTPUT="$DIST/loom-ui-$VERSION-$ARCH.AppImage"
"$APPIMAGETOOL" "$APPDIR" "$OUTPUT"
chmod +x "$OUTPUT"
echo "wrote $OUTPUT"
