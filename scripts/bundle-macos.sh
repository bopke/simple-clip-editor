#!/usr/bin/env bash
# Build "Clip Editor.app".
#
# The bundle structure is built from the start rather than retrofitted, and the
# result is ad-hoc signed: free, no Apple account, and it converts the
# unfixable "app is damaged" failure on Apple Silicon into the normal
# quarantine dialog the user can actually get past.
set -euo pipefail

cd "$(dirname "$0")/.."
NAME="Clip Editor"
APP="target/${NAME}.app"
BIN="target/release/clip-editor"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Apple Silicon only. Intel Macs are not a release target: the only static
# ffmpeg builds available for them are a different ffmpeg major than the arm64
# ones, and shipping two ffmpeg versions to one platform is exactly the silent
# behaviour difference the pinned download exists to prevent.
cargo build --release -p clip-editor
cp "$BIN" "$APP/Contents/MacOS/clip-editor"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Clip Editor</string>
  <key>CFBundleDisplayName</key>       <string>Clip Editor</string>
  <key>CFBundleIdentifier</key>        <string>net.clipeditor.app</string>
  <key>CFBundleVersion</key>           <string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleExecutable</key>        <string>clip-editor</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <key>LSArchitecturePriority</key>
  <array><string>arm64</string></array>
  <key>NSHighResolutionCapable</key>   <true/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Video</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.movie</string>
        <string>public.mpeg-4</string>
        <string>org.matroska.mkv</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# Ad-hoc signature. Not notarisation — users still get the quarantine dialog,
# which is documented in the README.
codesign --force --deep --sign - "$APP"
codesign --verify --verbose=1 "$APP"

# Distribute zipped: a bare .app loses its permissions bits over most
# transports. The GPL text ships with it, which is a licence obligation rather
# than a nicety.
cp LICENSE docs/ffmpeg-license.md target/ 2>/dev/null || true
(cd target && rm -f "${NAME}.zip" \
  && zip -qry "${NAME}.zip" "${NAME}.app" LICENSE ffmpeg-license.md)

echo "built $APP"
echo "      target/${NAME}.zip"
