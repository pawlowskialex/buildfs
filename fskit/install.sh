#!/usr/bin/env bash
# Builds the FSKit module and installs it as ~/Applications/BuildFS.app.
# The build product is removed so the installed copy is the only one
# registered: /tmp and build directories don't survive reboots or cleans.
# Unmount every buildfs FSKit volume first.
#
# One-time, manual: enable "buildfs" in System Settings > General >
# Login Items & Extensions > File System Extensions.
set -euo pipefail
cd "$(dirname "$0")"
DD="${DD:-$PWD/../target/fskit-dd}"
APP="$HOME/Applications/BuildFS.app"
BUILT="$DD/Build/Products/Release/BuildFS.app"
ID=com.pawlowskialex.buildfs.fsmodule

xcodebuild -project BuildFS.xcodeproj -scheme BuildFS -configuration Release \
  -allowProvisioningUpdates -derivedDataPath "$DD" build -quiet > "$DD.log" 2>&1 ||
  { tail -40 "$DD.log"; exit 1; }
mkdir -p "$HOME/Applications"
rm -rf "$APP"
cp -R "$BUILT" "$APP"

# Forget every other registered copy, then register the installed one.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister
pluginkit -m -v -i "$ID" 2>/dev/null | sed -nE 's/^.*[[:space:]](\/.*\.appex)$/\1/p' | while read -r p; do
  case "$p" in
    "$APP"/*) ;;
    *) pluginkit -r "$p" 2>/dev/null || true; "$LSREGISTER" -u "${p%/Contents/Extensions/*}" 2>/dev/null || true ;;
  esac
done
"$LSREGISTER" -u "$BUILT" 2>/dev/null || true
rm -rf "$BUILT" /tmp/fskit-dd/Build/Products/Release/BuildFS.app
"$APP/Contents/MacOS/BuildFS" > /dev/null
pluginkit -a "$APP/Contents/Extensions/BuildFSExtension.appex"
pluginkit -e use -i "$ID" 2>/dev/null || true
pkill -f BuildFSExtension.appex 2>/dev/null || true
pluginkit -m -v -i "$ID"
