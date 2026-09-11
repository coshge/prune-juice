#!/usr/bin/env bash
#
# Assemble "Prune Juice.app".
#
# Runs identically here and in CI, so the release is one reproducible command
# rather than a sequence someone remembers.
#
#   ./scripts/bundle.sh                 # ad-hoc signed, runs locally
#   SPARKLE_PUBLIC_KEY=… ./scripts/bundle.sh          # with updates enabled
#   SIGN_ID="Developer ID Application: …" ./scripts/bundle.sh
#   SIGN_ID=… NOTARY_PROFILE=pj ./scripts/bundle.sh --notarize
#
# Environment:
#   SPARKLE_PUBLIC_KEY  the EdDSA public key updates are verified against.
#                       Without it the built app has no update mechanism at
#                       all — see the note by SUPublicEDKey below.
#   UPDATE_FEED_URL     override the appcast URL (a staging feed).
#   SIGN_ID             Developer ID Application identity, when there is one.
#   NOTARY_PROFILE      notarytool keychain profile, for --notarize.
set -euo pipefail

# rustup installs outside the default PATH for a non-login shell.
export PATH="$HOME/.cargo/bin:$PATH"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
OUT="$HERE/dist"
# The bundle's file name, space and all, is what Finder, the Dock and the
# app switcher actually show — CFBundleDisplayName is consulted only when a
# bundle carries a localized InfoPlist.strings, which a hand-assembled one
# does not. So the name on disk is the name, the way "Google Chrome.app" is.
# Every use of $APP from here down is quoted for that reason.
APP="$OUT/Prune Juice.app"
NOTARIZE=0
[ "${1:-}" = "--notarize" ] && NOTARIZE=1

# The appcast Sparkle reads. A release asset rather than a separate site: the
# URL always resolves to the newest release, it needs no extra hosting to be
# set up or kept alive, and it is HTTPS as Sparkle requires.
FEED_URL="${UPDATE_FEED_URL:-https://github.com/coshge/prune-juice/releases/latest/download/appcast.xml}"

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

# --- Sparkle --------------------------------------------------------------
#
# SPM resolves Sparkle as a binary XCFramework and links the app against
# @rpath/Sparkle.framework. A bundle assembled by hand gets none of Xcode's
# copy phases, so the framework has to be put in Contents/Frameworks here —
# the rpath itself is set in Package.swift. Miss either half and the app dies
# at launch in dyld, before any of its own code runs.
say "adding Sparkle"
SPARKLE_SRC="$(find "$HERE/.build/artifacts" -type d -name Sparkle.framework -path '*macos*' | head -1)"
if [ -z "$SPARKLE_SRC" ]; then
  echo "error: Sparkle.framework not found — run 'swift package resolve' first" >&2
  exit 1
fi
mkdir -p "$APP/Contents/Frameworks"
# ditto rather than cp: a framework is a symlink farm (Versions/Current and
# the top-level aliases), and flattening it breaks both dyld and signing.
ditto "$SPARKLE_SRC" "$APP/Contents/Frameworks/Sparkle.framework"

# XPCServices exist so a *sandboxed* app can download and install through a
# separate process. This app cannot be sandboxed — the helper needs the Docker
# socket — so they are dead weight, and two fewer nested binaries to sign is
# two fewer ways for signing to go wrong.
rm -rf "$APP/Contents/Frameworks/Sparkle.framework/Versions/B/XPCServices"

# --- the icon -------------------------------------------------------------
#
# Rendered by scripts/make-icon.swift, so the artwork is source rather than a
# binary blob nobody can diff, and every size is drawn at its own resolution.
# Cached in .build because recompiling a script to redraw an unchanged icon on
# every bundle is pure waiting.
say "rendering the icon"
ICONSET="$HERE/.build/icon/PruneJuice.iconset"
ICNS="$HERE/.build/icon/PruneJuice.icns"
if [ ! -f "$ICNS" ] || [ "$HERE/scripts/make-icon.swift" -nt "$ICNS" ]; then
  rm -rf "$ICONSET"
  mkdir -p "$HERE/.build/icon"
  swift "$HERE/scripts/make-icon.swift" "$ICONSET"
  iconutil -c icns -o "$ICNS" "$ICONSET"
fi
mkdir -p "$APP/Contents/Resources"
cp "$ICNS" "$APP/Contents/Resources/PruneJuice.icns"

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
  <!-- Contents/Resources/PruneJuice.icns, named without its extension as
       LaunchServices expects. CFBundleIconName is deliberately absent: it
       names an entry in an asset catalog, and a bundle assembled by hand has
       no catalog to name. -->
  <key>CFBundleIconFile</key>          <string>PruneJuice</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key>           <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>    <string>14.0</string>
  <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <!-- A window app, not an agent. The menu bar icon is a setting, off by default. -->
  <key>LSUIElement</key>               <false/>

  <!-- Sparkle.

       SUPublicEDKey is what makes an update trustworthy here. This app is not
       Developer ID signed, so Sparkle's code-signature check cannot be the
       thing that vouches for a downloaded build — the EdDSA signature on the
       archive is. With the key absent Sparkle is not started at all
       (see Updater.swift): no key, no update mechanism, rather than an
       update mechanism that trusts whatever it is handed.

       SUAutomaticallyUpdate=false and SUAllowsAutomaticUpdates=false together
       mean an update is always offered and never silently applied. The second
       is the stronger statement: it removes the option, so this is a property
       of the app and not a default someone can flip. A tool that deletes
       things should not change itself without being asked.

       Checks run in the background at most once a day. Whether one may run
       *now* is decided in code, because "not during a scan" is not something
       an interval can express. -->
  <key>SUFeedURL</key>                 <string>${FEED_URL}</string>
  <key>SUPublicEDKey</key>             <string>${SPARKLE_PUBLIC_KEY:-}</string>
  <key>SUEnableAutomaticChecks</key>   <true/>
  <key>SUScheduledCheckInterval</key>  <integer>86400</integer>
  <key>SUAutomaticallyUpdate</key>     <false/>
  <key>SUAllowsAutomaticUpdates</key>  <false/>
</dict>
</plist>
PLIST

if [ -z "${SPARKLE_PUBLIC_KEY:-}" ]; then
  say "no SPARKLE_PUBLIC_KEY — this build will not offer updates (see RELEASING.md)"
fi

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
SPARKLE="$APP/Contents/Frameworks/Sparkle.framework"
if [ -n "${SIGN_ID:-}" ]; then
  say "signing with Developer ID"
  # Sparkle arrives signed by the Sparkle project. That signature stays valid
  # under an ad-hoc outer signature, but notarisation requires every nested
  # binary to be signed by the submitting team — so when there is an identity,
  # Sparkle is re-signed with it. Deepest first: Autoupdate and Updater.app
  # are separate programs, and Hardened Runtime is per-binary, not inherited.
  codesign --force --options runtime --timestamp \
    --sign "$SIGN_ID" "$SPARKLE/Versions/B/Autoupdate"
  codesign --force --options runtime --timestamp \
    --sign "$SIGN_ID" "$SPARKLE/Versions/B/Updater.app"
  codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$SPARKLE"
  codesign --force --options runtime --timestamp \
    --sign "$SIGN_ID" "$APP/Contents/MacOS/prune-juice"
  codesign --force --options runtime --timestamp \
    --entitlements "$OUT/PruneJuice.entitlements" \
    --sign "$SIGN_ID" "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
else
  # Ad-hoc: runs on this machine, and elsewhere only after the person opening
  # it clears Gatekeeper by hand. Stated rather than silently produced.
  #
  # Sparkle's own signature is left alone. It is valid, and replacing a real
  # signature with an ad-hoc one would be strictly worse.
  say "no SIGN_ID set — ad-hoc signing (needs Gatekeeper approval elsewhere)"
  codesign --force --sign - "$APP/Contents/MacOS/prune-juice"
  codesign --force --sign - "$APP"
  codesign --verify --strict --verbose=2 "$APP"
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

# --- the update archive ---------------------------------------------------
#
# What Sparkle downloads. `ditto -c -k --keepParent` is the only archiver that
# preserves the symlinks and extended attributes a signed bundle needs; a
# `zip -r` here produces an archive that unpacks into a broken signature.
#
# The whole bundle, which is the point: the app and its CLI helper are updated
# together and can never end up as two different versions.
say "packaging the update archive"
ditto -c -k --keepParent "$APP" "$OUT/PruneJuice-$VERSION.zip"

say "built $APP"
say "update archive: $OUT/PruneJuice-$VERSION.zip"
