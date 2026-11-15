#!/usr/bin/env bash
# Assemble the Samlator APK.
#
#   scripts/build-apk.sh [debug|release]
#
# Expects: rustup target aarch64-linux-android, cargo-ndk, ANDROID_NDK_HOME,
# a local Android SDK, and libxkbcommon built by scripts/build-xkbcommon.sh.
set -euo pipefail

PROFILE="${1:-release}"
# SAMLATOR_SAFE=1 builds the diagnostic front end (surface + egui only).
FEATURES=""
if [ "${SAMLATOR_SAFE:-0}" = "1" ]; then
    FEATURES="--features safe-boot"
fi
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ABI_DIR="$ROOT/android/app/src/main/jniLibs/arm64-v8a"
ASSET_XKB="$ROOT/android/app/src/main/assets/xkb"
DEPS="$ROOT/deps/android"

if [ ! -f "$DEPS/lib/libxkbcommon.so" ]; then
    echo "libxkbcommon for Android is missing; run scripts/build-xkbcommon.sh android" >&2
    exit 1
fi

mkdir -p "$ABI_DIR"

# 1. Rust: the cdylib that NativeActivity loads.
(
    cd "$ROOT/rust"
    CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-L native=$DEPS/lib" \
        cargo ndk -t arm64-v8a -P 29 build --$PROFILE -p samlator-app $FEATURES
)

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/rust/target}"
cp "$TARGET_DIR/aarch64-linux-android/$PROFILE/libsamlator_app.so" "$ABI_DIR/"
cp "$DEPS/lib/libxkbcommon.so" "$ABI_DIR/"

# 2. XKB rules, so the compositor can build a keymap on the device.
#    libxkbcommon reads $XKB_CONFIG_ROOT; the app points it here at startup.
if [ ! -d "$ASSET_XKB" ]; then
    if [ -d /usr/share/X11/xkb ]; then
        mkdir -p "$ASSET_XKB"
        cp -r /usr/share/X11/xkb/. "$ASSET_XKB/"
        # Trim what a phone keyboard never needs.
        rm -rf "$ASSET_XKB/symbols/deepin" "$ASSET_XKB/symbols/pl" "$ASSET_XKB/symbols/ru" 2>/dev/null || true
    else
        echo "warning: /usr/share/X11/xkb not found; the on-device keymap will be unavailable" >&2
    fi
fi

# 3. Gradle
KIND="$( [ "$PROFILE" = "release" ] && echo assembleRelease || echo assembleDebug )"
( cd "$ROOT/android"
  if [ -x ./gradlew ]; then
      ./gradlew --no-daemon "$KIND"
  else
      gradle --no-daemon "$KIND"
  fi
)

find "$ROOT/android/app/build/outputs/apk" -name '*.apk' -print
