# CLAUDE.md

Orientation for a fresh session. Read this first, then `README.md` for the
design rationale.

## What this is

`prune-juice` reclaims Docker disk space with per-item provenance and a safety
model strong enough that the headline action needs no confirmation. Free MIT
Rust CLI; a free macOS app comes later, architected so a one-time paid tier
could be added without rework.

## Status: M6 — macOS app builds and runs

| milestone | state |
|---|---|
| M1 read-only scan with provenance attribution | done |
| M2 planner, tier classification, apply path | done |
| M3 interactive ratatui TUI | done |
| M4a content probe — native **and** container path | done |
| M4b vault (dump / verify / restore) | done |
| M5 review actions, waivers, host disk measurement | done |
| M6 macOS app — window, scan, review; unsigned | done |

```
cargo test --workspace                  # 201 tests
cargo clippy --workspace --all-targets  # must stay at 0 warnings
cargo build -p prune-juice-cli
./target/debug/prune-juice              # interactive on a TTY; one-shot otherwise
./target/debug/prune-juice --no-tui     # force the one-shot report
./target/debug/prune-juice --apply      # reclaims the safe tier only
./target/debug/prune-juice --container-probe   # force the Docker Desktop path
./target/debug/prune-juice --vault            # list preserved copies
./target/debug/prune-juice --vault-dump VOL   # preserve one now, delete nothing
./target/debug/prune-juice --vault-verify     # re-read every copy
./target/debug/prune-juice --vault-restore ID # put a volume back
./target/debug/prune-juice --waivers          # what is being held back by hand
./target/debug/prune-juice --waive SEL --reason "..."   # hold something back

cargo run -p prune-juice-tui --example preview   # render every screen, no TTY needed

cd app/PruneJuice && ./scripts/bundle.sh        # assemble PruneJuice.app (ad-hoc signed)
SIGN_ID="Developer ID Application: …" ./scripts/bundle.sh --notarize
open app/PruneJuice/dist/PruneJuice.app
```

App diagnostics land in `~/Library/Logs/prune-juice-app.log`. A GUI launch has
no terminal, so that file is the only way to see why a scan failed.

## Deliberately not implemented — do not "fix" these

- **Irreversible items are preserved or refused, never deleted bare.** An
  orphaned or stale volume is dumped to the vault, fsynced, re-read, and only
  deleted once the digest and tar entry count both match. A failed dump leaves
  the volume alone and reports `PreserveFailed`. `--no-vault` does **not** mean
  "delete without a backup" — it means those items are refused, because
  declining a backup is not consent to lose data.
- **Only *empty* and *derivative* volumes can reach the safe tier.** A volume
  holding anything else — a database, user data, or contents we do not
  recognise — stays irreversible until the vault exists. An **unprobed** volume
  is never assumed empty: on Docker Desktop the data root is hidden inside a VM,
  and the honest answer there is "cannot be proven safe". Enforced in
  `plan/tier.rs::destabilises`.
- **The app is an AppKit shell, not a SwiftUI `App` scene.** A bundle
  assembled by hand does not get the LaunchServices registration SwiftUI's
  scene lifecycle needs: the delegate runs, the activation policy is set, and
  the window still never materialises — so the app launches, shows nothing and
  never scans. `AppDelegate` creates the `NSWindow` itself. Do not "simplify"
  this back to `@main struct App`.
- **`Protected` is not a dead end any more.** A tagged image used to end there
  permanently, which parked 111 GB out of reach. Recovery is now computed per
  image and routes to `repullable` (bandwidth only) or `rebuildable` (time, and
  an old build may not reproduce). Both are opt-in via `--tiers`.
- **The app reads only.** It scans, classifies and shows evidence; reclaiming
  is still `prune-juice --apply` in a terminal.
- **Docker Desktop is a first-class target.** Its data root lives inside a VM,
  so `VolumeAccess::detect` reports unavailable and the scan falls back to
  `DockerProbe` — a throwaway container with the volumes bound read-only, no
  network, and a read-only root filesystem. Both paths share `probe::classify`,
  so a volume is judged identically however its bytes were read. Verified: on
  the reference machine both paths independently return the same 50 volumes.
- **The probe never pulls an image.** It picks one already present locally. If
  there is none it says so and reclaims no volumes, rather than making an
  outbound request on someone's metered or air-gapped machine.
- **No image can reach the safe tier.** Image layer stacks are not fetched, so
  base-image relationships are unproven. Recorded honestly as
  `Opacity::ImageLayersUnknown` rather than assumed away.
- **`prune-juice-ffi` does not exist.** A staticlib crate slows every workspace
  build and breaks `cargo install` for no present benefit. Subprocess is the
  shipping architecture for the macOS app; UniFFI is an optimisation to take
  only if profiling demands it.
- **An orphan verdict needs two consecutive absences.** On a cold index the
  first run reports "looks orphaned but has only been missing across 1 scan —
  run again to confirm" and offers nothing. That is not a bug: one observation
  cannot distinguish a deleted project from an unplugged disk. `MIN_ABSENT_SCANS`
  in `providers/mod.rs`.
- **The TUI can now act on reviewed rows**, because the vault makes them
  recoverable. Tick with space, `d` to act, and it goes via a confirm screen
  that states how many will be copied to the vault first. Only `y` proceeds;
  every other key backs out. Never straight from a list keypress to a deletion.
- **`--only-label` skips the build cache entirely.** Build cache records carry
  no labels, so the fence cannot be honoured for them; pruning it anyway would
  break the promise the flag makes.

## Invariants that must not be broken

Each of these was either a bug that got fixed or a deliberate structural choice.
All have regression tests — if you break one, a test will tell you.

1. **`bollard` types never escape `crate::docker`.** That seam is what makes the
   client replaceable and the replay client possible. `cargo build
   --no-default-features` must keep compiling.
2. **`DockerClient` and `DockerMutate` stay separate traits.** This is the
   deletion firewall: a read-only client cannot delete even by accident.
3. **Never pass a force flag.** Every mutating call uses `force: false` so the
   daemon's own in-use check stays armed. A refusal from the daemon is the
   system working, not an obstacle. `remove_container` also uses `v: false` —
   a container removal must never take volumes with it.
4. **The read-set may only contain stable facts.** Recording raw elapsed
   seconds made every witness stale during the pre-apply re-scan, so nothing
   could ever be applied. Record the fixed timestamp and whether the gate
   passed, never a continuously-varying value.
5. **Opacity is scoped to what it could actually conceal.** Three unreadable
   project roots once made all 259 volumes `Unknown`. `Opacity::affects` takes
   the resource, not just its kind.
6. **`Unknown` is not a soft `No`.** It never satisfies a deletion predicate.
7. **Absence of a directory is not absence of a project.** If the parent is also
   unreadable the verdict is `Unverifiable`, never `Absent`. One unplugged disk
   must not orphan a whole root.
8. **Absence of `.git` is never evidence of abandonment.** Three live projects
   on the reference machine have no VCS.
9. **Longest-prefix matching, never regex suffix-stripping.** Stripping suffixes
   from `cedar-wordpress_mysql` yields `cedar` and a false orphan.
10. **A container carrying provenance is not free.** If it mounts named volumes,
    or is the last container recording its project's absolute path, removing it
    is a provenance loss. Container labels are the only place a project path
    lives.
11. **The TUI state machine has no terminal dependency.** `app.rs` must not
    import `ratatui` or `crossterm`; it returns an `Action` and the event loop
    performs the effect. That is what keeps it testable.
12. **Every rendered line must fit its terminal.** Column widths derive from the
    actual area, never a constant. There is a test at widths down to 20.
13. **`core` never prints.** `#![deny(clippy::print_stdout, clippy::print_stderr)]`
    in `lib.rs` makes UI-agnosticism a compile error.
14. **A derivative marker must dominate the volume.** `node_modules` as the
    sole top-level entry is a dependency cache; `vendor` *alongside*
    `composer.json` and `html/` is an application checkout. Matching the marker
    anywhere in the listing marked five 1.2 GB WordPress installs as safe to
    delete. Noise (`lost+found`, `.DS_Store`) is ignored; nothing else is.
15. **A database signature beats every other signature.** Misreading a data
    directory as a cache is the failure that loses data, so engine detection
    runs before cache detection in `probe::classify`.
16. **Probe mounts are always `:ro`, network `none`, rootfs read-only.** The
    `:ro` suffix on every bind is the boundary that stops an inspection from
    changing what it inspects. There is a test asserting it.
17. **The probe container is removed whatever happens.** The result is captured
    before cleanup rather than propagated early, so a failure cannot leak a
    container. Verified by container count before and after a real run.
18. **The vault hashes the file on disk, not the stream.** An early version
    wrapped a hasher above the gzip encoder: it never called `update`, and even
    fixed would have digested the uncompressed bytes while `verify` reads the
    compressed file. Hash what is actually written.
19. **A dump container is created and never started.** Booting an engine
    against a real data directory triggers crash recovery and catalog writes —
    mutating the thing being preserved. `GET /containers/{id}/archive` reads
    through a stopped container without executing anything.
20. **The vault lives on the host, outside any cache directory.** A vault inside
    a Docker volume could be eaten by a later `docker system prune`, making the
    safety net part of the hazard. There is a test asserting the path.
21. **Selections must not survive a rescan.** They index into a list a rescan
    replaces, so carrying them over would act on whatever landed at those
    positions. `App::ready` clears them; there is a test.
22. **A waiver needs a reason of at least 12 characters, and `*` is refused.**
    An exclusion nobody can explain later is worse than none, and a blanket
    waiver is not a waiver.
23. **Docker-reported and host-measured are different numbers.** Shown on
    separate lines, never summed, and where the host cannot be measured the
    answer is "could not be measured" — not the Docker figure in disguise.
24. **The index is written before anything is judged, let alone deleted.** A
    container's labels and mounts are the only place an anonymous volume's
    provenance lives, so recording has to precede removal — otherwise the
    evidence is destroyed before it is written down.
25. **An index edge is Weak evidence.** It is a memory, not a current fact, so
    it can attribute a volume for display but can never license a deletion.
26. **Evidence never leaks between daemons.** Every table is keyed by the
    daemon's `/info` ID. Attributing one engine's volume from another's history
    would be worse than knowing nothing. There is a test.
27. **Drain a subprocess pipe concurrently, always.** A pipe nobody reads
    fills at 64 KB and the writer blocks for ever, so reading stderr only after
    the process exits can deadlock the process being waited on. Both pipes are
    drained while the helper runs.
28. **A scan has a deadline.** "Slow" and "hung" are indistinguishable to a
    user, and a GUI cannot tell them apart either. Two minutes by default; on
    expiry the report is marked stale and says what it gave up on, and nothing
    unread is ever treated as provably safe.
29. **Swift type names mirror UniFFI codegen.** `PJEvent`, lowerCamelCase
    fields, records as structs. If the transport is ever swapped for in-process
    FFI, the view models keep compiling and only `SubprocessService` is deleted.
    Rename them and that stops being true.
30. **A RepoDigests entry is not proof of pullability.** BuildKit stamps one on
    locally built images, so `fen-wordpress@sha256:…` looks pullable and is
    not. Check for a local build context *first*; fall back to the digest.
31. **An image is always `Referenced::Unknown`** (layers unfetched), so the
    live-container check never fires for one. Ask `graph.image_is_live()`
    directly — otherwise an image serving a running container can be offered.
32. **Every opt-in tier must state its price.** `Tier::caveat()` is not
    decoration; a tier a user cannot cost is a tier they cannot consent to.
33. **Permission to delete is a value, not a flag.** `SafeToDelete` has private
    fields and no public constructor; `Fresh` comes only from `revalidate()` and
    is consumed by the executor. Do not add a public constructor or a `Clone`.

## The acceptance gate

Run `./target/debug/prune-juice --json` against the author's machine and read
the `classified` events. Two distinct properties, easy to conflate:

**Must never be `orphan`** — the project exists, so an orphan verdict would be a
false positive:

```
cedar-wordpress_mysql · lantern_postgres_data
time-tracking-app_mysql · helpdesk-ticketing-system_mysql_data
```

Only `nbk` should ever be flagged as orphaned; that project was renamed to
`northbank`.

**Must never be `free`** — these hold real data:

```
saffronfields-mariadb   (24 entries, a live MariaDB datadir)
oak_mysql · redkite_mysql · any *_mysql with contents
```

Note `ivy-mariadb` **is** correctly `free`: ddev created it and never populated
it, so it is genuinely empty (0 entries, 0 B, unreferenced) and ddev recreates
it on `ddev start`. Emptiness is a property of contents, not of the name — do
not add a name-based exception for it.

`plan.irreversible_free_bytes()` must be zero: the Tier 1 Theorem checked on
real data rather than asserted.

A stronger check, worth running after any change to the probe or tier logic —
every volume the tool calls `free` must be both empty and unreferenced:

```sh
./target/debug/prune-juice --json | jq -r 'select(.event=="classified"
  and .tier=="free" and .kind=="volume") | .name' \
| while read -r v; do
    n=$(ls -A ~/OrbStack/docker/volumes/"$v" 2>/dev/null | wc -l)
    [ "$n" -gt 0 ] && echo "NOT EMPTY: $v"
  done
```

That check is what caught the classifier calling five 1.2 GB WordPress
checkouts "derivative" because they contained a `vendor` directory.

## House conventions

- Dry-run by default; mutation only behind explicit `--apply`.
- stdout is the payload, stderr is human progress. `--json` for machine mode.
- Exit codes are semantic: 0 clean, 1 findings, 2 usage, 3 daemon unreachable,
  4 permission denied, 5 partial failure, 130 cancelled.
- Refuse rather than half-do it. An operation that cannot complete cleanly says
  why and changes nothing.
- Exclusions and waivers require a human-authored reason.
- Conventional commit prefixes: `feat:`, `fix:`, `chore:`, `docs:`.
- Honour `NO_COLOR` and TTY detection.

## Known gaps worth picking up

- Image sizes are summed naively, so shared base layers are counted more than
  once (82.5 GB reported vs ~80.5 GB actual). Needs exclusive-size accounting.
- Waivers still live in a JSON file rather than the index. Harmless, but they
  could move now that the index exists.
- Warm scan is ~3–6 s, over the sub-2 s target. The fix is the size cache: an
  orphaned volume's size is immutable, so its cache entry is valid forever, and
  the expensive `df` is only needed for in-use volumes — exactly the ones that
  will never be deleted.
- bollard cannot encode `?type=` on `/system/df` (`serde_urlencoded` rejects a
  `Vec`), so the planned filter optimisation is unavailable. One unfiltered call
  returns volumes and build cache together, which is what `data_usage()` does.
- `--apply` has still never removed anything on a real daemon. The vault's
  dump/verify/restore path **has** been proven end-to-end against one: a
  throwaway volume was dumped, deleted, restored, and compared byte-for-byte
  including labels. What remains unexercised is the delete step itself.
- Old note, still true: The apply path is covered
  by unit tests with a spy client, and dry-run exercises the identical code path
  including per-item revalidation, but a real end-to-end deletion is unverified.
  **Ask before running it** — it is 89 containers, 20 networks and ~12 GB of
  build cache on the author's machine.
