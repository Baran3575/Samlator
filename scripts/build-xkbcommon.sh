#!/usr/bin/env bash
# Build libxkbcommon as a static library for aarch64-linux-android.
#
# Samlator has no X11 and no system Wayland library: smithay runs on the pure
# Rust Wayland backend. libxkbcommon is the only C dependency in the whole
# stack, because keymap compilation (Electron sends raw evdev codes) needs it.
set -euo pipefail

PREFIX="${PREFIX:-$PWD/deps/arm64}"
VERSION="${XKBCOMMON_VERSION:-1.7.0}"
CROSS_FILE="${MESON_CROSS_FILE:-}"
WORK="${WORK:-$PWD/.cache/deps}"

mkdir -p "$WORK" "$PREFIX"
cd "$WORK"

if [ ! -f "libxkbcommon-$VERSION.tar.xz" ]; then
  curl -fsSL -o "libxkbcommon-$VERSION.tar.xz" \
    "https://xkbcommon.org/download/libxkbcommon-$VERSION.tar.xz"
fi
rm -rf "libxkbcommon-$VERSION"
tar xf "libxkbcommon-$VERSION.tar.xz"
cd "libxkbcommon-$VERSION"

SETUP_ARGS=(
  --prefix="$PREFIX"
  --buildtype=release
  --default-library=static
  -Denable-x11=false
  -Denable-wayland=false
  -Denable-docs=false
  -Dbuild-tools=false
)
if [ -n "$CROSS_FILE" ]; then
  SETUP_ARGS+=(--cross-file "$CROSS_FILE")
fi

meson setup build "${SETUP_ARGS[@]}"
ninja -C build
ninja -C build install

echo "libxkbcommon installed into $PREFIX"
find "$PREFIX" -name 'libxkbcommon*' -maxdepth 3 -print
