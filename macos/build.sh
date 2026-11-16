#!/bin/sh
# Builds VaultSync.app (arm64 + x86_64), signs it with the team's Developer ID,
# notarizes and staples it, and zips it into build/VaultSync.zip.
#
#   build.sh                      sign and notarize (keychain profile plow-notary)
#   NOTARY_PROFILE=other build.sh
#   APPLE_ID=… APPLE_PASSWORD=… APPLE_TEAM_ID=… build.sh   notarize with an Apple ID (CI)
#   SIGN=- build.sh               ad-hoc signed, not notarized (local testing)
#
# The engine (crates/ffi) is linked in as a static library: built here with
# cargo when Rust is installed, else from lib/libvaultsync_ffi-<target>.a for
# both targets (scripts/macos-engine.sh cross-builds them on Linux).
set -eu
cd "$(dirname "$0")"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' ../Cargo.toml)
SIGN=${SIGN:-Developer ID Application: The Plow Collective, Inc (3559PD337Z)}
NOTARY_PROFILE=${NOTARY_PROFILE:-plow-notary}
APP=build/VaultSync.app

mkdir -p lib
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    if command -v cargo >/dev/null; then
        MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release --locked -p vaultsync-ffi --target "$target" --manifest-path ../Cargo.toml
        cp "../target/$target/release/libvaultsync_ffi.a" "lib/libvaultsync_ffi-$target.a"
    fi
    [ -f "lib/libvaultsync_ffi-$target.a" ] || { echo "build.sh: no engine library for $target" >&2; exit 1; }
done
lipo -create -output lib/libvaultsync_ffi.a lib/libvaultsync_ffi-aarch64-apple-darwin.a lib/libvaultsync_ffi-x86_64-apple-darwin.a

# One build per architecture, joined: a multi-arch `swift build` needs Xcode.
set --
for arch in arm64 x86_64; do
    swift build -c release --triple "$arch-apple-macosx14.0"
    set -- "$@" "$(swift build -c release --triple "$arch-apple-macosx14.0" --show-bin-path)/VaultSyncBar"
done
rm -rf build && mkdir -p "$APP/Contents/MacOS"
lipo -create -output "$APP/Contents/MacOS/VaultSync" "$@"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>co.plow.vaultsync</string>
    <key>CFBundleName</key><string>VaultSync</string>
    <key>CFBundleExecutable</key><string>VaultSync</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>LSUIElement</key><true/>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
</dict>
</plist>
PLIST

if [ "$SIGN" = - ]; then
    codesign --force --sign - --entitlements VaultSync.entitlements "$APP"
    ditto -c -k --keepParent "$APP" build/VaultSync.zip
    echo "build/VaultSync.zip (ad-hoc signed, not notarized)"
    exit 0
fi
codesign --force --options runtime --timestamp --entitlements VaultSync.entitlements --sign "$SIGN" "$APP"
ditto -c -k --keepParent "$APP" build/VaultSync.zip
if [ -n "${APPLE_ID:-}" ]; then
    xcrun notarytool submit build/VaultSync.zip --apple-id "$APPLE_ID" \
        --password "${APPLE_PASSWORD:?}" --team-id "${APPLE_TEAM_ID:?}" --wait
else
    xcrun notarytool submit build/VaultSync.zip --keychain-profile "$NOTARY_PROFILE" --wait
fi
xcrun stapler staple "$APP"
rm build/VaultSync.zip && ditto -c -k --keepParent "$APP" build/VaultSync.zip
spctl --assess --type execute --verbose "$APP"
echo "build/VaultSync.zip"
