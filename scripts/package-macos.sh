#!/usr/bin/env bash
# Builds "Peachey Studio.app" (and a .dmg) in dist/.
#
#   scripts/package-macos.sh            # native architecture
#   SIGN_ID="Developer ID Application: …" scripts/package-macos.sh
#
# Without SIGN_ID the bundle is ad-hoc signed, which runs locally but is not
# notarized for distribution.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
APP="dist/Peachey Studio.app"
cargo build --release -p peel-app -p peel-host-clap --bins

rm -rf "$APP" dist/iconset
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" dist/iconset
cp target/release/peachey-studio "$APP/Contents/MacOS/peachey-studio"
# Plugin scanner: a crashing plugin takes down this helper, not the app.
cp target/release/peel-clap-scan "$APP/Contents/MacOS/peel-clap-scan"
# Python and packages ship in the app; nothing is installed at first launch.
uv run --no-project --python 3.12.13 scripts/bundle-python.py "$APP/Contents/Resources/python"
cp LICENSE README.md assets/LICENSES.md "$APP/Contents/Resources/"
cp docs/SCRIPTING.md "$APP/Contents/Resources/"
mkdir -p "$APP/Contents/Resources/sound-licenses"
cp assets/drums/big-rusty/LICENSE.txt assets/drums/big-rusty/README.md assets/drums/big-rusty/manifest.json "$APP/Contents/Resources/sound-licenses/"
mkdir -p "$APP/Contents/Resources/sound-licenses/strings"
cp assets/strings/recorded/LICENSE-guitar.txt assets/strings/recorded/LICENSE-bass.txt assets/strings/recorded/README.md assets/strings/recorded/manifest.json "$APP/Contents/Resources/sound-licenses/strings/"
mkdir -p "$APP/Contents/Resources/sound-licenses/palm-muted"
cp assets/strings/palm-muted/{LICENSE.txt,NOTICE.txt,README.md,manifest.json} "$APP/Contents/Resources/sound-licenses/palm-muted/"

# Icon set from the generated PNGs.
cp assets/icons/peachey-16.png dist/iconset/icon_16x16.png
cp assets/icons/peachey-32.png dist/iconset/icon_16x16@2x.png
cp assets/icons/peachey-32.png dist/iconset/icon_32x32.png
cp assets/icons/peachey-64.png dist/iconset/icon_32x32@2x.png
cp assets/icons/peachey-128.png dist/iconset/icon_128x128.png
cp assets/icons/peachey-256.png dist/iconset/icon_128x128@2x.png
cp assets/icons/peachey-256.png dist/iconset/icon_256x256.png
cp assets/icons/peachey-512.png dist/iconset/icon_256x256@2x.png
cp assets/icons/peachey-512.png dist/iconset/icon_512x512.png
cp assets/icons/peachey-1024.png dist/iconset/icon_512x512@2x.png
mv dist/iconset dist/Peachey.iconset
iconutil -c icns dist/Peachey.iconset -o "$APP/Contents/Resources/Peachey.icns"
rm -rf dist/Peachey.iconset

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Peachey Studio</string>
  <key>CFBundleDisplayName</key><string>Peachey Studio</string>
  <key>CFBundleIdentifier</key><string>io.github.hacchake.peacheystudio</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>peachey-studio</string>
  <key>CFBundleIconFile</key><string>Peachey</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Peachey Studio records audio from the input you choose.</string>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Peachey Studio Project</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>CFBundleTypeIconFile</key><string>Peachey</string>
      <key>LSHandlerRank</key><string>Owner</string>
      <key>CFBundleTypeExtensions</key><array><string>peelproj</string></array>
    </dict>
    <dict>
      <key>CFBundleTypeName</key><string>Standard MIDI File</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>CFBundleTypeExtensions</key><array><string>mid</string><string>midi</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

uv run --no-project --python 3.12.13 scripts/sign-macos.py "$APP"
# Check the signed, relocated interpreter before making the disk image.
"$APP/Contents/Resources/python/bin/python3" -I -B -c 'from pytheory import TonedScale; assert [t.midi for t in TonedScale(tonic="C4")["major"].triad().tones] == [60,64,67]'
codesign --verify --deep --strict --verbose=1 "$APP"

DMG="dist/Peachey-Studio-${VERSION}-macos-$(uname -m).dmg"
rm -f "$DMG"
hdiutil create -quiet -volname "Peachey Studio" -srcfolder "$APP" -ov -format UDZO "$DMG"
echo "Built $APP and $DMG"
