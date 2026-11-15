#!/usr/bin/env bash
# Build libxkbcommon from source.
#
#   scripts/build-xkbcommon.sh host      # Linux host, used by `cargo test`
#   scripts/build-xkbcommon.sh android   # aarch64-linux-android, shipped in the APK
#
# The Rust bindings (`xkbcommon` crate) use a bare `#[link(name = "xkbcommon")]`,
# so both runs produce a shared library; on Android it travels inside the APK
# next to `libsamlator_app.so` and is resolved from the app's native library dir.
set -euo pipefail

TARGET="${1:-host}"
XKBCOMMON_VERSION="${XKBCOMMON_VERSION:-1.7.0}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$ROOT/deps/work"
PREFIX="$ROOT/deps/$TARGET"
SRC="$WORK/libxkbcommon-$XKBCOMMON_VERSION"
TARBALL="$WORK/libxkbcommon-$XKBCOMMON_VERSION.tar.xz"

if [ -f "$PREFIX/lib/libxkbcommon.so" ] || [ -f "$PREFIX/lib64/libxkbcommon.so" ]; then
    echo "==> libxkbcommon already built in $PREFIX"
    exit 0
fi

command -v meson >/dev/null || { echo "meson is required" >&2; exit 1; }
command -v ninja >/dev/null || { echo "ninja is required" >&2; exit 1; }

mkdir -p "$WORK"
if [ ! -d "$SRC" ]; then
    if [ ! -f "$TARBALL" ]; then
        echo "==> downloading libxkbcommon $XKBCOMMON_VERSION"
        curl -fsSL -o "$TARBALL" \
            "https://xkbcommon.org/download/libxkbcommon-$XKBCOMMON_VERSION.tar.xz"
    fi
    tar -xJf "$TARBALL" -C "$WORK"
fi

MESON_ARGS=(
    --buildtype=release
    --default-library=shared
    --prefix="$PREFIX"
    -Denable-x11=false
    -Denable-wayland=false
    -Denable-docs=false
    -Denable-tools=false
    -Denable-xkbregistry=false
    -Denable-bash-completion=false
)

case "$TARGET" in
    host)
        BUILD="$WORK/build-host"
        ;;
    android)
        NDK="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
        if [ -z "$NDK" ] || [ ! -d "$NDK" ]; then
            echo "ANDROID_NDK_HOME is not set or does not point at the NDK" >&2
            exit 1
        fi
        API="${ANDROID_API:-29}"
        BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
        CROSS="$WORK/android-aarch64.ini"
        cat > "$CROSS" <<EOF
[binaries]
c = '$BIN/aarch64-linux-android$API-clang'
ar = '$BIN/llvm-ar'
strip = '$BIN/llvm-strip'
c_ld = '$BIN/ld.lld'
pkg-config = 'false'

[host_machine]
system = 'android'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
EOF
        BUILD="$WORK/build-android"
        MESON_ARGS+=(--cross-file "$CROSS")
        ;;
    *)
        echo "unknown target: $TARGET (expected host or android)" >&2
        exit 1
        ;;
esac

rm -rf "$BUILD"
meson setup "$BUILD" "$SRC" "${MESON_ARGS[@]}"
meson compile -C "$BUILD"
meson install -C "$BUILD"

echo "==> libxkbcommon installed into $PREFIX"
