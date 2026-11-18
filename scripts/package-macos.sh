#!/bin/sh
# Wraps the wireless-battery binary into WirelessBattery.app and zips it.
#   usage: scripts/package-macos.sh <version> <path-to-binary> [output-dir]
#
# The bundle gives the app a stable identity (needed for the Input Monitoring permission and for
# "Start at login") and LSUIElement hides the Dock icon. It is signed with a local self-signed certificate (or ad-hoc), so Gatekeeper
# asks the user to confirm the first launch (see README).
set -eu

version=${1:?usage: package-macos.sh <version> <binary> [output-dir]}
bin=${2:?usage: package-macos.sh <version> <binary> [output-dir]}
out=${3:-dist}
root=$(cd "$(dirname "$0")/.." && pwd)

app="$out/WirelessBattery.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/WirelessBattery"
chmod 755 "$app/Contents/MacOS/WirelessBattery"
cp "$root/LICENSE" "$app/Contents/Resources/LICENSE.txt"
# Rust crate licenses, made by scripts/third-party-licenses.py (CI generates it before packaging).
if [ -f "$root/installer/THIRD_PARTY_LICENSES.txt" ]; then
  cp "$root/installer/THIRD_PARTY_LICENSES.txt" "$app/Contents/Resources/THIRD_PARTY_LICENSES.txt"
else
  echo "warning: installer/THIRD_PARTY_LICENSES.txt not found; Rust crate licenses will be missing" >&2
fi

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Wireless Battery</string>
  <key>CFBundleDisplayName</key><string>Wireless Battery</string>
  <key>CFBundleIdentifier</key><string>app.wirelessbattery.monitor</string>
  <key>CFBundleExecutable</key><string>WirelessBattery</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# A stable identity keeps the macOS Accessibility grant across rebuilds. Create it once with a
# self-signed code-signing certificate named like this in the login keychain; else fall back to ad-hoc.
identity="${SIGN_IDENTITY:-Wireless Battery Local Signing}"
if security find-certificate -c "$identity" >/dev/null 2>&1; then
  codesign --force --sign "$identity" "$app"
else
  echo "warning: no '$identity' certificate; ad-hoc signing (Accessibility must be re-granted after every build)" >&2
  codesign --force --sign - "$app"
fi
codesign --verify --strict "$app"

zip="WirelessBattery-$version-macos.zip"
rm -f "$out/$zip"
(cd "$out" && ditto -c -k --norsrc --noextattr --noqtn --keepParent WirelessBattery.app "$zip")
echo "Created $out/$zip"
