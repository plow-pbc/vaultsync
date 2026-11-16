#!/bin/sh
# Makes lib/libvaultsync_ffi.a, the engine the app links, for arm64 and
# x86_64: built here with cargo when Rust is installed, else from
# lib/libvaultsync_ffi-<target>.a for both targets (scripts/macos-engine.sh
# cross-builds those on Linux).
set -eu
cd "$(dirname "$0")"
mkdir -p lib
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    if command -v cargo >/dev/null; then
        MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release --locked -p vaultsync-ffi --target "$target" --manifest-path ../Cargo.toml
        cp "../target/$target/release/libvaultsync_ffi.a" "lib/libvaultsync_ffi-$target.a"
    fi
    [ -f "lib/libvaultsync_ffi-$target.a" ] || { echo "build-engine.sh: no engine library for $target" >&2; exit 1; }
done
lipo -create -output lib/libvaultsync_ffi.a lib/libvaultsync_ffi-aarch64-apple-darwin.a lib/libvaultsync_ffi-x86_64-apple-darwin.a
