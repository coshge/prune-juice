#!/usr/bin/env bash
#
# Assemble PruneJuice.app.
#
# Runs identically here and in CI, so the release is one reproducible command
# rather than a sequence someone remembers.
#
#   ./scripts/bundle.sh                 # ad-hoc signed, runs locally
#   SIGN_ID="Developer ID Application: …" ./scripts/bundle.sh
#   SIGN_ID=… NOTARY_PROFILE=pj ./scripts/bundle.sh --notarize
set -euo pipefail

# rustup installs outside the default PATH for a non-login shell.
export PATH="$HOME/.cargo/bin:$PATH"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
OUT="$HERE/dist"
APP="$OUT/PruneJuice.app"
NOTARIZE=0
[ "${1:-}" = "--notarize" ] && NOTARIZE=1

say() { printf '  %s\n' "$*"; }

rm -rf "$OUT"
mkdir -p "$APP/Contents/MacOS"

# --- the helper -----------------------------------------------------------
#
# Universal where both targets are installed, so one bundle runs on Apple
# silicon and Intel. A single-arch build is not an error, just narrower.
say "building the helper"
cd "$REPO"
ARCHS=()
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  if rustup target list --installed 2>/dev/null | grep -qx "$t"; then
    cargo build --release -p prune-juice-cli --target "$t" >/dev/null
    ARCHS+=("target/$t/release/prune-juice")
  fi
done
if [ "${#ARCHS[@]}" -eq 0 ]; then
  cargo build --release -p prune-juice-cli >/dev/null
  cp target/release/prune-juice "$APP/Contents/MacOS/prune-juice"
  say "helper: host architecture only"
elif [ "${#ARCHS[@]}" -eq 1 ]; then
  cp "${ARCHS[0]}" "$APP/Contents/MacOS/prune-juice"
  say "helper: $(basename "$(dirname "$(dirname "${ARCHS[0]}")")")"
else
  lipo -create -output "$APP/Contents/MacOS/prune-juice" "${ARCHS[@]}"
  say "helper: universal"
fi

# --- the app --------------------------------------------------------------
say "building the app"
cd "$HERE"
swift build -c release >/dev/null
cp "$(swift build -c release --show-bin-path)/PruneJuice" "$APP/Contents/MacOS/PruneJuice"

VERSION="$(grep -m1 '^version' "$REPO/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Prune Juice</string>
  <key>CFBundleDisplayName</key>       <string>Prune Juice</string>
  <key>CFBundleIdentifier</key>        <string>dev.prunejuice.app</string>
  <key>CFBundleExecutable</key>        <string>PruneJuice</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key>           <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>    <string>14.0</string>
  <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <!-- A window app, not an agent. The menu bar icon is a setting, off by default. -->
  <key>LSUIElement</key>               <false/>
</dict>
</plist>
PLIST

cat > "$OUT/PruneJuice.entitlements" <<'ENT'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- No App Sandbox: the helper needs the Docker socket, which the sandbox
       forbids outright. That also rules out the App Store, so distribution is
       a notarised direct download.

       Deliberately absent: com.apple.security.cs.disable-library-validation.
       That is for dlopening unsigned dylibs; spawning a signed child needs no
       entitlement at all, and adding it would weaken the runtime for nothing. -->
  <key>com.apple.security.cs.allow-jit</key><false/>
</dict>
</plist>
ENT

# --- signing --------------------------------------------------------------
#
# Inside-out, always: nested code first, the bundle last. Hardened Runtime is
# per-binary and not inherited, so the helper needs its own --options runtime.
if [ -n "${SIGN_ID:-}" ]; then
  say "signing with Developer ID"
  codesign --force --options runtime --timestamp \
    --sign "$SIGN_ID" "$APP/Contents/MacOS/prune-juice"
  codesign --force --options runtime --timestamp \
    --entitlements "$OUT/PruneJuice.entitlements" \
    --sign "$SIGN_ID" "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
else
  # Ad-hoc: runs on this machine, will not pass Gatekeeper elsewhere. Stated
  # rather than silently produced.
  say "no SIGN_ID set — ad-hoc signing (runs locally, not distributable)"
  codesign --force --sign - "$APP/Contents/MacOS/prune-juice"
  codesign --force --sign - "$APP"
fi

# --- notarisation ---------------------------------------------------------
if [ "$NOTARIZE" = 1 ]; then
  : "${SIGN_ID:?--notarize needs SIGN_ID}"
  : "${NOTARY_PROFILE:?--notarize needs NOTARY_PROFILE (xcrun notarytool store-credentials)}"
  say "notarising"
  ditto -c -k --keepParent "$APP" "$OUT/PruneJuice.zip"
  xcrun notarytool submit "$OUT/PruneJuice.zip" \
    --keychain-profile "$NOTARY_PROFILE" --wait
  # Stapling the .app means the nested helper inherits the ticket. A bare
  # Mach-O cannot be stapled, which is another reason the helper lives inside.
  xcrun stapler staple "$APP"
  spctl -a -vvv -t exec "$APP"
  say "notarised and stapled"
fi

say "built $APP"
