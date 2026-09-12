#!/usr/bin/env bash
#
# Update this machine's CLI and app from the working tree.
#
# There is no release key in a checkout, so `--update` and Sparkle are both
# disabled by design (see RELEASING.md) — which leaves rebuilding by hand as
# the way to run what you just wrote. This is that, in one command.
#
#   ./scripts/install-local.sh            # CLI and app, from one build
#   ./scripts/install-local.sh --cli      # just the CLI
#   ./scripts/install-local.sh --app      # just the app bundle
#   ./scripts/install-local.sh --open     # ...and launch the app afterwards
#   PREFIX=~/bin ./scripts/install-local.sh          # install the CLI elsewhere
#
# The default run builds the app bundle and then installs *its* helper as the
# CLI, so the two are the same bytes rather than two builds that happen to
# agree. This is a development convenience: published app releases pin their
# helper independently, and the standalone CLI may have a different version.
set -euo pipefail

# rustup and a user-level bin dir are both outside a non-login shell's PATH.
export PATH="$HOME/.cargo/bin:$PATH"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$HERE/app/PruneJuice"
APP="$APP_DIR/dist/Prune Juice.app"

DO_CLI=1
DO_APP=1
OPEN=0
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --cli)   DO_APP=0 ;;
    --app)   DO_CLI=0 ;;
    --open)  OPEN=1 ;;
    --force) FORCE=1 ;;
    -h|--help) sed -n '3,18p' "${BASH_SOURCE[0]}" | sed 's/^#\{1,2\} \{0,1\}//'; exit 0 ;;
    *) echo "error: unknown option $arg (try --help)" >&2; exit 2 ;;
  esac
done

say() { printf '  %s\n' "$*"; }

# --- where the CLI goes ---------------------------------------------------
#
# Whatever is already on the PATH wins, so an existing install is updated in
# place rather than shadowed by a second copy somewhere else.
BIN="${PREFIX:-}"
if [ -z "$BIN" ]; then
  existing="$(command -v prune-juice 2>/dev/null || true)"
  if [ -n "$existing" ]; then
    BIN="$(dirname "$existing")"
  else
    BIN="$HOME/.local/bin"
  fi
fi

# A binary a package manager installed is not ours to replace: the manager
# would go on serving a version it never installed, and its next upgrade would
# silently undo this one. The updater refuses for the same reason — see
# `update/origin.rs`.
case "$BIN" in
  /opt/homebrew/*|/usr/local/Cellar/*|/home/linuxbrew/*)
    echo "error: $BIN belongs to Homebrew. Use 'brew upgrade prune-juice', or" >&2
    echo "       PREFIX=~/.local/bin $0 to install a separate copy." >&2
    exit 2 ;;
esac

# --- refuse to rebuild underneath a running app ---------------------------
if [ "$DO_APP" = 1 ] && [ "$FORCE" = 0 ] && pgrep -x PruneJuice >/dev/null 2>&1; then
  echo "error: Prune Juice is running, and building replaces the bundle it is" >&2
  echo "       running from. Quit it first, or pass --force." >&2
  exit 2
fi

# --- the app, which also builds the helper --------------------------------
if [ "$DO_APP" = 1 ]; then
  say "building the app bundle"
  ( cd "$APP_DIR" && ./scripts/bundle.sh )
fi

# --- the CLI --------------------------------------------------------------
if [ "$DO_CLI" = 1 ]; then
  if [ "$DO_APP" = 1 ]; then
    # Already built, inside the bundle. Installing that exact file is what
    # makes "the app's helper" and "the CLI on my PATH" the same thing.
    SRC="$APP/Contents/MacOS/prune-juice"
  else
    say "building the helper"
    # Built for the host *target triple* rather than with a bare
    # `cargo build`, because that is what `bundle.sh` does — same triple, same
    # target directory, same cached dependencies. A bare build lands in
    # `target/release` instead, so alternating between `--cli` and a full run
    # would rebuild the whole dependency graph each way round.
    HOST="$(rustc -vV | sed -n 's/^host: //p')"
    if rustup target list --installed 2>/dev/null | grep -qx "$HOST"; then
      ( cd "$HERE" && cargo build --release -p prune-juice-cli --target "$HOST" >/dev/null )
      SRC="$HERE/target/$HOST/release/prune-juice"
    else
      ( cd "$HERE" && cargo build --release -p prune-juice-cli >/dev/null )
      SRC="$HERE/target/release/prune-juice"
    fi
  fi

  mkdir -p "$BIN"
  # `install` replaces by rename, so a copy running right now is not modified
  # underneath itself.
  install -m 755 "$SRC" "$BIN/prune-juice"
  say "installed $BIN/prune-juice ($("$BIN/prune-juice" --version))"

  case ":$PATH:" in
    *":$BIN:"*) ;;
    *) say "note: $BIN is not on your PATH" ;;
  esac
fi

# --- what is now where ----------------------------------------------------
if [ "$DO_APP" = 1 ]; then
  say "app bundle $APP"
  # An older copy elsewhere is the thing that quietly wastes an afternoon.
  # `PruneJuice.app` is the pre-0.3.2 name: a build from before the bundle was
  # renamed is exactly such a copy, and is worth naming for that reason.
  for other in \
    "/Applications/Prune Juice.app" "$HOME/Applications/Prune Juice.app" \
    "/Applications/PruneJuice.app" "$HOME/Applications/PruneJuice.app"; do
    [ -d "$other" ] || continue
    say "note: $other is a separate, now older copy — replace it with:"
    say "      ditto '$APP' '$other'"
  done
  [ "$OPEN" = 1 ] && open "$APP"
fi
