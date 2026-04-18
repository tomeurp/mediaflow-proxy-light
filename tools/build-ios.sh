#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Build the MediaFlow Proxy Light static library as an iOS .xcframework.
#
# Requirements:
#   - Xcode installed with command-line tools
#   - Rust iOS targets:
#       rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
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

echo "==> Building for aarch64-apple-ios (device)"
cargo build \
    --release \
    --target aarch64-apple-ios \
    --features "$FEATURES" \
    --manifest-path "$PROJECT_DIR/Cargo.toml"

echo "==> Building for aarch64-apple-ios-sim (Apple Silicon simulator)"
cargo build \
    --release \
    --target aarch64-apple-ios-sim \
    --features "$FEATURES" \
    --manifest-path "$PROJECT_DIR/Cargo.toml"

echo "==> Building for x86_64-apple-ios (Intel Mac simulator)"
cargo build \
    --release \
    --target x86_64-apple-ios \
    --features "$FEATURES" \
    --manifest-path "$PROJECT_DIR/Cargo.toml"

# Merge simulator slices into a fat library
SIM_UNIVERSAL="$PROJECT_DIR/target/libmediaflow_sim_universal.a"
echo "==> Creating universal simulator library"
lipo -create \
    "$PROJECT_DIR/target/aarch64-apple-ios-sim/release/libmediaflow_proxy_light.a" \
    "$PROJECT_DIR/target/x86_64-apple-ios/release/libmediaflow_proxy_light.a" \
    -output "$SIM_UNIVERSAL"

# Remove old xcframework if it exists
rm -rf "$XCFRAMEWORK_OUT"

echo "==> Creating xcframework"
xcodebuild -create-xcframework \
    -library "$PROJECT_DIR/target/aarch64-apple-ios/release/libmediaflow_proxy_light.a" \
    -headers "$HEADERS_DIR" \
    -library "$SIM_UNIVERSAL" \
    -headers "$HEADERS_DIR" \
    -output "$XCFRAMEWORK_OUT"

echo ""
echo "==> Done: $XCFRAMEWORK_OUT"
echo "    Add this framework to your Xcode project under:"
echo "    ios/MediaflowProxy/Frameworks/MediaflowProxy.xcframework"
echo ""
