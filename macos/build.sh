#!/bin/sh
# Builds "BrainSync by Plow.app" (arm64 + x86_64), signs it with the team's
# Developer ID, notarizes and staples it, and packs it into
# build/BrainSync-by-Plow.dmg.
#
#   build.sh                      sign and notarize (keychain profile plow-notary)
#   NOTARY_PROFILE=other build.sh
#   APPLE_ID=… APPLE_PASSWORD=… APPLE_TEAM_ID=… build.sh   notarize with an Apple ID (CI)
#   SIGN=- build.sh               ad-hoc signed, not notarized (local testing)
#
# The engine (crates/ffi) is linked in as a static library; build-engine.sh
# makes it.
set -eu
cd "$(dirname "$0")"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' ../Cargo.toml)
SIGN=${SIGN:-Developer ID Application: The Plow Collective, Inc (3559PD337Z)}
NOTARY_PROFILE=${NOTARY_PROFILE:-plow-notary}
APP="build/BrainSync by Plow.app"
DMG=build/BrainSync-by-Plow.dmg

./build-engine.sh

# One build per architecture, joined: a multi-arch `swift build` needs Xcode.
set --
for arch in arm64 x86_64; do
    swift build -c release --triple "$arch-apple-macosx14.0"
    set -- "$@" "$(swift build -c release --triple "$arch-apple-macosx14.0" --show-bin-path)/VaultSyncBar"
done
rm -rf build && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The Plow app icon and menu-bar mark, rendered from the plow repo's brand/.
cp Resources/AppIcon.icns Resources/MenuIcon.png Resources/MenuIcon@2x.png "$APP/Contents/Resources/"
lipo -create -output "$APP/Contents/MacOS/VaultSync" "$@"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>co.plow.vaultsync</string>
    <key>CFBundleName</key><string>BrainSync</string>
    <key>CFBundleDisplayName</key><string>BrainSync by Plow</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
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

# The app beside an Applications link: open it, drag one onto the other.
dmg() {
    rm -rf build/dmg "$DMG" && mkdir build/dmg && cp -R "$APP" build/dmg/ && ln -s /Applications build/dmg/Applications
    hdiutil create -quiet -volname "BrainSync by Plow" -srcfolder build/dmg -format UDZO "$DMG"
    rm -rf build/dmg
}
if [ "$SIGN" = - ]; then
    codesign --force --sign - --entitlements VaultSync.entitlements "$APP"
    dmg
    echo "$DMG (ad-hoc signed, not notarized)"
    exit 0
fi
codesign --force --options runtime --timestamp --entitlements VaultSync.entitlements --sign "$SIGN" "$APP"
dmg
codesign --force --timestamp --sign "$SIGN" "$DMG"
if [ -n "${APPLE_ID:-}" ]; then
    xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" \
        --password "${APPLE_PASSWORD:?}" --team-id "${APPLE_TEAM_ID:?}" --wait
else
    xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
fi
xcrun stapler staple "$DMG"
spctl --assess --type open --context context:primary-signature --verbose "$DMG"
echo "$DMG"
