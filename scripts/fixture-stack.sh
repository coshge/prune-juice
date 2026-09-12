#!/usr/bin/env bash
# Build a Docker stack with known answers, so a scan can be graded rather than
# eyeballed. Runtime-agnostic on purpose: the same fixtures on OrbStack and on
# Docker Desktop are what make the two comparable.
#
#   ./scripts/fixture-stack.sh up      # create
#   ./scripts/fixture-stack.sh down    # remove every pjtest-* resource
#   ./scripts/fixture-stack.sh expect  # print the expected verdicts
#
# Everything is namespaced `pjtest`. Nothing outside that prefix is touched.
# Total footprint is about 40 MB.
#
# Honours DOCKER_HOST / DOCKER_CONTEXT, so:
#   DOCKER_CONTEXT=desktop-linux ./scripts/fixture-stack.sh up

set -euo pipefail

BASE=busybox:stable
GHOST=pjtest-ghost          # a compose project that does not exist on disk
LIVE=pjtest-live            # a compose project that does (created below)
LIVE_DIR="${TMPDIR:-/tmp}/pjtest-live"
GHOST_DIR="${TMPDIR:-/tmp}/pjtest-ghost"

say() { printf '  %s\n' "$*"; }

need_base() {
  if ! docker image inspect "$BASE" >/dev/null 2>&1; then
    say "pulling $BASE (the fixture needs it; the probe itself never pulls)"
    docker pull -q "$BASE" >/dev/null
  fi
}

# Write contents through a container, never from the host. On Docker Desktop
# the host cannot reach a volume's bytes at all, and a fixture that only works
# on OrbStack would defeat the point.
fill() {
  local vol="$1" script="$2"
  docker run --rm -v "$vol":/v "$BASE" sh -c "$script" >/dev/null
}

up() {
  need_base
  mkdir -p "$LIVE_DIR"

  # --- expected FREE ------------------------------------------------------
  # Created and never written. Genuinely nothing to lose.
  docker volume create pjtest-empty >/dev/null
  say "pjtest-empty            created, never written"

  # A dependency tree as the sole top-level entry: invariant 14's cache case.
  docker volume create pjtest-node-modules >/dev/null
  fill pjtest-node-modules 'mkdir -p /v/node_modules/left-pad && echo x > /v/node_modules/left-pad/index.js'
  say "pjtest-node-modules     node_modules alone — derivative"

  # --- expected NOT free --------------------------------------------------
  # Invariant 48, the sharpest one: every file lives below the depth a shallow
  # walk reaches. A top-level listing sees one directory and no files.
  docker volume create pjtest-deep-uploads >/dev/null
  fill pjtest-deep-uploads 'mkdir -p /v/uploads/2024/01 && dd if=/dev/urandom of=/v/uploads/2024/01/photo.jpg bs=1k count=64 2>/dev/null'
  say "pjtest-deep-uploads     files only at depth 3 — must NOT read as empty"

  # Invariant 14's other half: vendor/ beside application files is a checkout,
  # not a cache.
  docker volume create pjtest-vendor-app >/dev/null
  fill pjtest-vendor-app 'mkdir -p /v/vendor/acme /v/html && echo "{}" > /v/composer.json && echo "<?php" > /v/html/index.php && echo y > /v/vendor/acme/a.php'
  say "pjtest-vendor-app       vendor + composer.json + html — a checkout"

  # Invariant 15: a database signature must beat every other signature.
  docker volume create pjtest-mysql-data >/dev/null
  fill pjtest-mysql-data 'mkdir -p /v/mysql /v/performance_schema && dd if=/dev/urandom of=/v/ibdata1 bs=1k count=512 2>/dev/null && dd if=/dev/urandom of=/v/ib_logfile0 bs=1k count=256 2>/dev/null && echo x > /v/mysql/user.frm && echo "8.0.35" > /v/mysql_upgrade_info'
  say "pjtest-mysql-data       a MySQL datadir — precious"

  # A datadir that ALSO carries a cache marker, to prove ordering in classify.
  docker volume create pjtest-mysql-with-cache >/dev/null
  fill pjtest-mysql-with-cache 'mkdir -p /v/mysql /v/node_modules && dd if=/dev/urandom of=/v/ibdata1 bs=1k count=256 2>/dev/null && echo x > /v/mysql/user.frm'
  say "pjtest-mysql-with-cache datadir + node_modules — database must win"

  # --- expected ORPHAN (after two scans) ----------------------------------
  # Labelled for a project that is not on disk. One absence is not a verdict:
  # MIN_ABSENT_SCANS means the first scan says "run again to confirm".
  # Its directory exists *now*, on purpose. An orphan verdict needs a path to
  # have been recorded before it can be observed missing — a volume whose
  # project was never located anywhere stays unattributed for ever, which is
  # correct but tests nothing. Run `orphan-arm` to delete the directory once a
  # scan has recorded it.
  docker volume create --label com.docker.compose.project="$GHOST" \
    --label com.docker.compose.volume=db pjtest-ghost_db >/dev/null
  fill pjtest-ghost_db 'mkdir -p /v/pgdata && echo "PG_VERSION" > /v/pgdata/PG_VERSION && dd if=/dev/urandom of=/v/pgdata/base bs=1k count=128 2>/dev/null'
  mkdir -p "$GHOST_DIR"
  cat > "$GHOST_DIR/docker-compose.yml" <<'YML'
services:
  db:
    image: busybox:stable
    volumes: [db:/var/lib/postgresql/data]
volumes:
  db:
YML
  # A project path only reaches the index through a *container's* labels —
  # invariant 24. A compose file found on disk attributes the volume for
  # display but is never checkpointed, so a labelled volume whose project was
  # only ever seen on the filesystem stays unattributed for ever. Hence a
  # created-but-never-started container carrying compose's own working_dir.
  docker rm -f pjtest-ghost-db-1 >/dev/null 2>&1 || true
  docker create --name pjtest-ghost-db-1 \
    --label com.docker.compose.project="$GHOST" \
    --label com.docker.compose.project.working_dir="$GHOST_DIR" \
    --label com.docker.compose.service=db \
    -v pjtest-ghost_db:/var/lib/postgresql/data "$BASE" true >/dev/null
  say "pjtest-ghost_db         project present for now — see 'orphan-arm'"

  # The control: same shape, but its project directory exists. Must never be
  # called an orphan. This is invariant 50 — a present project reads absent=0.
  docker volume create --label com.docker.compose.project="$LIVE" \
    --label com.docker.compose.volume=db pjtest-live_db >/dev/null
  fill pjtest-live_db 'mkdir -p /v/pgdata && echo "PG_VERSION" > /v/pgdata/PG_VERSION'
  cat > "$LIVE_DIR/docker-compose.yml" <<'YML'
services:
  db:
    image: busybox:stable
    volumes: [db:/var/lib/postgresql/data]
volumes:
  db:
YML
  say "pjtest-live_db          owner project present at $LIVE_DIR — never orphan"

  # --- containers ---------------------------------------------------------
  # Invariant 46: a writable layer holding 20 MB cannot reach the safe tier.
  docker rm -f pjtest-fat >/dev/null 2>&1 || true
  docker run --name pjtest-fat "$BASE" \
    sh -c 'dd if=/dev/zero of=/blob bs=1M count=20 2>/dev/null' >/dev/null
  say "pjtest-fat              stopped, ~20 MB writable layer — not free"

  # Invariant 49: an empty writable layer is still not "recreated by config"
  # without a compose/ddev/devcontainer mark. Its definition lives nowhere else.
  docker rm -f pjtest-handmade >/dev/null 2>&1 || true
  docker run --name pjtest-handmade "$BASE" true >/dev/null
  say "pjtest-handmade         stopped, empty layer, no label — price is Gone"

  # --- network ------------------------------------------------------------
  # Same rule for a hand-made network: no label, so removing it destroys the
  # only copy of its definition.
  docker network create pjtest-handmade-net >/dev/null 2>&1 || true
  say "pjtest-handmade-net     hand-made network — not recreated by config"

  echo
  say "done. grade it with: ./scripts/acceptance-gate.sh"
}

down() {
  docker rm -f pjtest-fat pjtest-handmade pjtest-ghost-db-1 >/dev/null 2>&1 || true
  for v in pjtest-empty pjtest-node-modules pjtest-deep-uploads pjtest-vendor-app \
           pjtest-mysql-data pjtest-mysql-with-cache pjtest-ghost_db pjtest-live_db; do
    docker volume rm -f "$v" >/dev/null 2>&1 || true
  done
  docker network rm pjtest-handmade-net >/dev/null 2>&1 || true
  rm -rf "$LIVE_DIR" "$GHOST_DIR"
  say "removed every pjtest-* resource"
}

expect() {
  cat <<'TXT'
  resource                  expected                reason
  ------------------------- ----------------------- ----------------------------
  pjtest-empty              free                    empty and unreferenced
  pjtest-node-modules       free                    derivative (node_modules)
  pjtest-deep-uploads       NOT free                files below the walk depth
  pjtest-vendor-app         NOT free                vendor beside an app
  pjtest-mysql-data         NOT free                database datadir
  pjtest-mysql-with-cache   NOT free                database beats cache
  pjtest-ghost_db           opt-in + vaulted        owner project absent (see orphan-arm)
  pjtest-live_db            NOT orphan              owner project present
  pjtest-fat                NOT free                20 MB writable layer
  pjtest-handmade           NOT free                no config to recreate it
  pjtest-handmade-net       NOT free                no config to recreate it
TXT
}

# Delete the ghost project's directory, so the next scans see it go missing.
# An orphan needs two consecutive absences (MIN_ABSENT_SCANS): one observation
# cannot tell a deleted project from an unplugged disk.
orphan_arm() {
  docker rm -f pjtest-ghost-db-1 >/dev/null 2>&1 || true
  rm -rf "$GHOST_DIR"
  say "removed the ghost container and $GHOST_DIR"
  cat <<'TXT'

  Scan at least once BEFORE arming: that is the scan that checkpoints the
  container-to-volume edge carrying the project's path. Without it there is no
  remembered path to observe going missing, and the volume stays unattributed.

  After arming, scan twice more. One absence is never a verdict — it cannot
  tell a deleted project from an unplugged disk (MIN_ABSENT_SCANS).

  The volume then lands in an opt-in, vaulted tier. Which one depends on its
  contents: a datadir nobody recognises is held as `stale` by the content gate,
  which runs before the orphan gate. Either way it is offered for review, never
  reclaimed without asking, and dumped to the vault before any deletion.
TXT
}

case "${1:-up}" in
  up) up ;;
  down) down ;;
  expect) expect ;;
  orphan-arm) orphan_arm ;;
  *) echo "usage: $0 [up|down|expect|orphan-arm]" >&2; exit 2 ;;
esac
