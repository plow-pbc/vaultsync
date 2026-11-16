#!/bin/sh
# Cross-builds the engine's static library for the Mac app (arm64 and
# x86_64) on Linux, in Docker with zig, into macos/lib/ for macos/build.sh.
# On a Mac with Rust installed, build.sh builds it itself.
set -eu
cd "$(dirname "$0")/.."
mkdir -p macos/lib
docker volume create vaultsync-macos-engine >/dev/null
docker run --rm -v "$PWD":/src:ro -v "$PWD/macos/lib":/out -v vaultsync-macos-engine:/cache \
    rust:1-bookworm sh -eu -c '
export CARGO_HOME=/cache/cargo CARGO_TARGET_DIR=/cache/target PATH=/cache/cargo/bin:$PATH
if [ ! -x /cache/cargo/bin/cargo-zigbuild ]; then
    mkdir -p /cache/cargo/bin && cp /usr/local/cargo/bin/* /cache/cargo/bin/
    cargo install --locked cargo-zigbuild
fi
if [ ! -d /cache/zig ]; then
    apt-get -qq update && apt-get -qq install -y python3-pip >/dev/null
    pip install --break-system-packages -q --target /cache/zig ziglang
fi
export PATH=/cache/zig/ziglang:$PATH
rustup target add aarch64-apple-darwin x86_64-apple-darwin
# iroh and iroh-relay also build cdylibs, which nothing here uses but cargo
# still links: give those links empty framework stubs and leave symbols to
# load time. The static library never references them; the app links the
# real macOS SDK.
for lib in objc resolv iconv c++ System; do
    mkdir -p /cache/stub/lib
    printf -- "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos, x86_64-macos ]\ninstall-name: /usr/lib/lib$lib.dylib\n...\n" > /cache/stub/lib/lib$lib.tbd
done
for fw in Security SystemConfiguration CoreFoundation Foundation IOKit CoreServices AppKit Network; do
    mkdir -p /cache/stub/$fw.framework
    printf -- "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos, x86_64-macos ]\ninstall-name: /System/Library/Frameworks/$fw.framework/Versions/A/$fw\n...\n" > /cache/stub/$fw.framework/$fw.tbd
done
export MACOSX_DEPLOYMENT_TARGET=14.0
export RUSTFLAGS="-C link-arg=-L/cache/stub/lib -C link-arg=-F/cache/stub -C link-arg=-undefined -C link-arg=dynamic_lookup"
rm -rf /tmp/w && mkdir /tmp/w && cd /tmp/w && cp -r /src/Cargo.toml /src/Cargo.lock /src/crates .
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo zigbuild --release --locked -p vaultsync-ffi --target "$target"
    cp "$CARGO_TARGET_DIR/$target/release/libvaultsync_ffi.a" "/out/libvaultsync_ffi-$target.a"
done'
# The container ran as root; hand the libraries back.
docker run --rm -v "$PWD/macos/lib":/out alpine chown -R "$(id -u):$(id -g)" /out
ls -l macos/lib
