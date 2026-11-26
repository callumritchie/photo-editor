#!/usr/bin/env bash
# Build the Rust core and the SwiftUI shell, then assemble app/build/PhotoEditor.app (ad-hoc signed).
set -euo pipefail
cd "$(dirname "$0")/.."
export MACOSX_DEPLOYMENT_TARGET=14.0
cargo build --release -p pe-ffi
# SwiftPM does not track the Rust static library, so force a relink to pick up core changes.
rm -f app/.build/release/PhotoEditor
swift build -c release --package-path app
APP=app/build/PhotoEditor.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "app/.build/release/PhotoEditor" "$APP/Contents/MacOS/PhotoEditor"
# Lens database (CC BY-SA 3.0 data, with its licence and notice).
mkdir -p "$APP/Contents/Resources"
cp -R data/lensfun "$APP/Contents/Resources/lensfun"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Photo Editor</string>
  <key>CFBundleIdentifier</key><string>io.github.photo-editor.dev</string>
  <key>CFBundleExecutable</key><string>PhotoEditor</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.0.1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
codesign --force --sign - "$APP" >/dev/null
echo "built $APP"
