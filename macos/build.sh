#!/bin/sh
# Builds "WikiSync by Plow.app" (arm64 + x86_64), signs it with the team's
# Developer ID, notarizes and staples it, and packs it into
# build/WikiSync-by-Plow.dmg.
#
#   build.sh                      sign and notarize (keychain profile plow-notary)
#   NOTARY_PROFILE=other build.sh
#   APPLE_ID=… APPLE_PASSWORD=… APPLE_TEAM_ID=… build.sh   notarize with an Apple ID (CI)
#   SIGN=- build.sh               ad-hoc signed, not notarized (local testing)
#   SANDBOX=1 build.sh            with the App Sandbox (the App Store build); without
#                                 it the app shares the command-line tool's vaults
#
# The engine (crates/ffi) is linked in as a static library; build-engine.sh
# makes it.
set -eu
cd "$(dirname "$0")"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' ../Cargo.toml)
SIGN=${SIGN:-Developer ID Application: The Plow Collective, Inc (3559PD337Z)}
NOTARY_PROFILE=${NOTARY_PROFILE:-plow-notary}
APP="build/WikiSync by Plow.app"
ENTITLEMENTS=
if [ "${SANDBOX:-0}" = 1 ]; then ENTITLEMENTS=VaultSync.entitlements; fi
DMG=build/WikiSync-by-Plow.dmg

./build-engine.sh

# One build per architecture, joined: a multi-arch `swift build` needs Xcode.
set --
for arch in arm64 x86_64; do
    swift build -c release --triple "$arch-apple-macosx14.0"
    set -- "$@" "$(swift build -c release --triple "$arch-apple-macosx14.0" --show-bin-path)/VaultSyncBar"
done
rm -rf build && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The Plow app icon and menu-bar mark, rendered from the plow repo's brand/;
# menu-icon.swift adds the sync dot to the mark.
cp Resources/AppIcon.icns Resources/MenuIcon.png Resources/MenuIcon@2x.png "$APP/Contents/Resources/"
lipo -create -output "$APP/Contents/MacOS/VaultSync" "$@"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>co.plow.vaultsync</string>
    <key>CFBundleName</key><string>WikiSync</string>
    <key>CFBundleDisplayName</key><string>WikiSync by Plow</string>
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
    hdiutil create -quiet -volname "WikiSync by Plow" -srcfolder build/dmg -format UDZO "$DMG"
    rm -rf build/dmg
}
if [ "$SIGN" = - ]; then
    codesign --force --sign - ${ENTITLEMENTS:+--entitlements "$ENTITLEMENTS"} "$APP"
    dmg
    echo "$DMG (ad-hoc signed, not notarized)"
    exit 0
fi
codesign --force --options runtime --timestamp ${ENTITLEMENTS:+--entitlements "$ENTITLEMENTS"} --sign "$SIGN" "$APP"
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
