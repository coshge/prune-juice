#!/usr/bin/env bash
# Grade a scan against the fixture stack, and re-run the Tier 1 Theorem in a
# form that works on a VM-backed daemon.
#
#   ./scripts/acceptance-gate.sh
#   DOCKER_CONTEXT=desktop-linux ./scripts/acceptance-gate.sh
#   PJ=./target/release/prune-juice ./scripts/acceptance-gate.sh
#
# The check in CLAUDE.md reads ~/OrbStack/docker/volumes directly. Docker
# Desktop hides its data root inside a VM, so there is nothing there to read —
# and "no files found" is exactly the answer that must never be trusted
# (invariant 48). So every free-tier volume is re-read through a throwaway
# read-only container instead, which is the one method available everywhere.

set -uo pipefail

PJ="${PJ:-./target/debug/prune-juice}"
BASE=busybox:stable
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

fail=0
pass() { printf '  \033[32mPASS\033[0m  %-26s %s\n' "$1" "${2:-}"; }
bad()  { printf '  \033[31mFAIL\033[0m  %-26s %s\n' "$1" "${2:-}"; fail=$((fail + 1)); }
skip() { printf '  \033[33mSKIP\033[0m  %-26s %s\n' "$1" "${2:-}"; }

[ -x "$PJ" ] || { echo "no binary at $PJ — cargo build -p prune-juice-cli" >&2; exit 2; }

echo "== scanning =="
"$PJ" --json --no-update-check --deadline 240 "$@" > "$OUT/scan.ndjson" 2> "$OUT/scan.err"
code=$?
[ -s "$OUT/scan.ndjson" ] || { echo "scan produced nothing (exit $code)"; cat "$OUT/scan.err"; exit 3; }

runtime=$(jq -r 'select(.event=="scan_started") | .runtime' "$OUT/scan.ndjson" | head -1)
dur=$(jq -r 'select(.event=="scan_finished") | .duration_ms' "$OUT/scan.ndjson" | head -1)
stale=$(jq -r 'select(.event=="scan_finished") | .stale' "$OUT/scan.ndjson" | head -1)
echo "   runtime=$runtime  duration=${dur}ms  stale=$stale  exit=$code"
echo

tier_of() {
  jq -r --arg n "$1" 'select(.event=="classified" and .name==$n) | .tier' \
    "$OUT/scan.ndjson" | head -1
}

why_of() {
  jq -r --arg n "$1" 'select(.event=="classified" and .name==$n) | .because' \
    "$OUT/scan.ndjson" | head -1
}

# Nothing newer than MIN_AGE_SECS (24h) can reach the safe tier, whatever its
# contents. So a fixture created minutes ago is *expected* to sit in quarantine,
# and grading that as a failure would train you to ignore the gate. Re-run the
# gate a day after `fixture-stack.sh up` to convert these to real PASSes.
want_free() {
  local t w; t=$(tier_of "$1"); w=$(why_of "$1")
  [ -z "$t" ] && { skip "$1" "not present — run fixture-stack.sh up"; return; }
  if [ "$t" = free ]; then
    pass "$1" "free"
  elif printf '%s' "$w" | grep -qE "too new to be sure|something is still using it"; then
    # Two separate 24h windows, and a fixture created moments ago trips both:
    # MIN_AGE_SECS (how old the volume is) and RECENT_WRITE_SECS (when its
    # bytes were last touched). Neither is a defect — they are the quarantine
    # working — so they skip rather than fail.
    skip "$1" "quarantined — $w (re-run 24h after fixture-stack.sh up)"
  else
    bad "$1" "expected free, got '$t' — $w"
  fi
}

want_not_free() {
  local t; t=$(tier_of "$1")
  [ -z "$t" ] && { skip "$1" "not present — run fixture-stack.sh up"; return; }
  [ "$t" = free ] && bad "$1" "reached the tier that deletes without asking" \
                  || pass "$1" "$t — $(why_of "$1")"
}

want_not_orphan() {
  local t; t=$(tier_of "$1")
  [ -z "$t" ] && { skip "$1" "not present"; return; }
  [ "$t" = orphan ] && bad "$1" "false orphan — the project is on disk" \
                    || pass "$1" "$t"
}

echo "== fixture verdicts =="
want_free     pjtest-empty
want_free     pjtest-node-modules
want_not_free pjtest-deep-uploads
want_not_free pjtest-vendor-app
want_not_free pjtest-mysql-data
want_not_free pjtest-mysql-with-cache
want_not_free pjtest-fat
want_not_free pjtest-handmade
want_not_free pjtest-handmade-net
want_not_orphan pjtest-live_db
echo "   pjtest-ghost_db is '$(tier_of pjtest-ghost_db)' — $(why_of pjtest-ghost_db)"
echo "     (never 'free' is the property that matters; see fixture-stack.sh orphan-arm)"
echo

echo "== Tier 1 Theorem: every free volume is empty, read through a container =="
jq -r 'select(.event=="classified" and .tier=="free" and .kind=="volume") | .name' \
  "$OUT/scan.ndjson" | sort -u > "$OUT/free.vols"
n=$(wc -l < "$OUT/free.vols" | tr -d ' ')
if [ "$n" -eq 0 ]; then
  echo "   no free-tier volumes on this daemon — the check is vacuous here"
elif ! docker image inspect "$BASE" >/dev/null 2>&1; then
  skip "emptiness" "$BASE not present locally"
else
  # `find ! -type d`, never `ls`: a top level of uploads/ alone looks empty to
  # a listing, and files living below it is the whole point.
  args=(); while read -r v; do [ -n "$v" ] && args+=(-v "$v":/vols/"$v":ro); done < "$OUT/free.vols"
  docker run --rm --network none --read-only "${args[@]}" "$BASE" \
    sh -c 'for d in /vols/*; do n=$(find "$d" ! -type d 2>/dev/null | wc -l); echo "$(basename "$d") $n"; done' \
    > "$OUT/counts.txt" 2>/dev/null
  while read -r vol count; do
    [ -z "$vol" ] && continue
    if [ "$count" -gt 0 ]; then
      bad "$vol" "NOT EMPTY — $count objects, yet offered as free"
    else
      pass "$vol" "empty ($count objects)"
    fi
  done < "$OUT/counts.txt"
  echo "   checked $n free volume(s)"
fi
echo

echo "== irreversible_free_bytes must be zero =="
"$PJ" --no-tui --no-update-check --deadline 240 "$@" > "$OUT/report.txt" 2>/dev/null
line=$(grep -E '[[:space:]]irreversible$' "$OUT/report.txt" | head -1)
if [ -z "$line" ]; then
  skip "irreversible" "line not found in the report"
elif echo "$line" | grep -qE '^\s*0 B\s+irreversible'; then
  pass "irreversible" "0 B"
else
  bad "irreversible" "non-zero: $(echo "$line" | xargs)"
fi
echo

if [ "$fail" -eq 0 ]; then
  printf '\033[32mgate passed\033[0m\n'
else
  printf '\033[31mgate failed — %d check(s)\033[0m\n' "$fail"
fi
exit $((fail > 0))
