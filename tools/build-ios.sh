#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Build the MediaFlow Proxy Light static library as an iOS .xcframework.
#
# Ships two slices:
#   - aarch64-apple-ios       (physical iPhone / iPad)
#   - aarch64-apple-ios-sim   (iOS simulator running on Apple Silicon Mac)
#
# Intel Mac iOS simulator (x86_64-apple-ios) is intentionally not included:
# Apple stopped selling Intel Macs in 2023, the iOS 17+ simulator on Intel
# is unsupported by Xcode, and the extra slice adds ~107 MB to the archive.
# Set INCLUDE_X86_64_SIM=1 to opt in if you really need it.
#
# Requirements:
#   - Xcode installed with command-line tools
#   - Rust iOS targets:
#       rustup target add aarch64-apple-ios aarch64-apple-ios-sim
#
# Usage:
#   ./tools/build-ios.sh
#
# Output:
#   MediaflowProxy.xcframework   (in the project root)
# ---------------------------------------------------------------------------

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# iOS feature set — all proxy features relevant on-device are enabled.
#
# Included:
#   ffi         — required: the C bridge used by the Swift wrapper app
#   hls,mpd,drm — core streaming + DASH/DRM
#   xtream      — IPTV provider API
#   extractors  — video-host extractors (Cloudflare bypass works once
#                 IPHONEOS_DEPLOYMENT_TARGET is ≥ 13.0, see below)
#   telegram    — MTProto streaming from Telegram media
#   acestream   — P2P BitTorrent live streams
#   web-ui,base64-url — on-device UI + utilities
#   tls-rustls  — pure-Rust TLS (avoids openssl-sys cross-compile)
#
# Excluded:
#   transcode — needs ffmpeg subprocess, not possible on iOS sandbox
#   redis     — external cache, not useful on-device (local cache used instead)
FEATURES="ffi,hls,mpd,drm,xtream,extractors,telegram,acestream,web-ui,base64-url,tls-rustls"
XCFRAMEWORK_OUT="$PROJECT_DIR/MediaflowProxy.xcframework"
HEADERS_DIR="$PROJECT_DIR/include"

# Minimum iOS version — needs to be high enough for modern dependencies'
# system intrinsic requirements (boring-sys' chkstk_darwin, zstd-sys).
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-13.0}"

if [[ ! -f "$HEADERS_DIR/mediaflow_ffi.h" ]]; then
    echo "ERROR: $HEADERS_DIR/mediaflow_ffi.h not found." >&2
    echo "       Run 'cargo build --features ffi' first to generate the C header." >&2
    exit 1
fi

# Targets — `x86_64-apple-ios` opt-in only (see header comment).
TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim)
if [[ "${INCLUDE_X86_64_SIM:-}" == "1" ]]; then
    TARGETS+=(x86_64-apple-ios)
fi

# Build each iOS target — `[profile.release]` produces optimised static
# archives but Cargo's `strip = true` doesn't apply to them, so we strip
# manually below.
for TARGET in "${TARGETS[@]}"; do
    echo "==> Building for $TARGET"
    cargo build \
        --release \
        --target "$TARGET" \
        --features "$FEATURES" \
        --manifest-path "$PROJECT_DIR/Cargo.toml"
done

# Strip debug info + local symbols from each .a.  Keeps public symbols
# required for Swift to link against (SSL_*, mediaflow_* FFI exports).
# Typically cuts each archive size by ~25% before zip compression.
echo "==> Stripping debug info from static archives"
for TARGET in "${TARGETS[@]}"; do
    LIB="$PROJECT_DIR/target/$TARGET/release/libmediaflow_proxy_light.a"
    BEFORE=$(du -h "$LIB" | cut -f1)
    # -S removes debug symbols, -x removes local (non-global) symbols.
    # Global symbols are preserved — Swift's linker needs them.
    strip -S -x "$LIB"
    AFTER=$(du -h "$LIB" | cut -f1)
    echo "    $TARGET:  $BEFORE  →  $AFTER"
done

# Produce the simulator library.  If we built only arm64, use it directly;
# if we also built x86_64 (INCLUDE_X86_64_SIM=1), lipo them into a universal.
SIM_LIB="$PROJECT_DIR/target/aarch64-apple-ios-sim/release/libmediaflow_proxy_light.a"
if [[ "${INCLUDE_X86_64_SIM:-}" == "1" ]]; then
    SIM_LIB="$PROJECT_DIR/target/libmediaflow_sim_universal.a"
    echo "==> Creating universal simulator library (arm64 + x86_64)"
    lipo -create \
        "$PROJECT_DIR/target/aarch64-apple-ios-sim/release/libmediaflow_proxy_light.a" \
        "$PROJECT_DIR/target/x86_64-apple-ios/release/libmediaflow_proxy_light.a" \
        -output "$SIM_LIB"
fi

# Remove old xcframework if it exists
rm -rf "$XCFRAMEWORK_OUT"

echo "==> Creating xcframework"
xcodebuild -create-xcframework \
    -library "$PROJECT_DIR/target/aarch64-apple-ios/release/libmediaflow_proxy_light.a" \
    -headers "$HEADERS_DIR" \
    -library "$SIM_LIB" \
    -headers "$HEADERS_DIR" \
    -output "$XCFRAMEWORK_OUT"

echo ""
echo "==> xcframework size:"
du -sh "$XCFRAMEWORK_OUT"

echo ""
echo "==> Done: $XCFRAMEWORK_OUT"
echo "    Add this framework to your Xcode project under:"
echo "    ios/MediaflowProxy/Frameworks/MediaflowProxy.xcframework"
echo ""
