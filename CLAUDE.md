# CLAUDE.md

Orientation for a fresh session. Read this first, then `README.md` for the
design rationale.

## What this is

`prune-juice` reclaims Docker disk space with per-item provenance and a safety
model strong enough that the headline action needs no confirmation. Free MIT
Rust CLI; a free macOS app comes later, architected so a one-time paid tier
could be added without rework.

## Status: M4a complete (content probe)

| milestone | state |
|---|---|
| M1 read-only scan with provenance attribution | done |
| M2 planner, tier classification, apply path | done |
| M3 interactive ratatui TUI | done |
| M4a content probe — **unlocks empty and derivative volumes** | done |
| M4b vault (dump / verify / restore) — unlocks the rest | next |
| M5 review, waivers, host disk measurement | not started |
| M6 macOS app | not started |

```
cargo test --workspace                  # 129 tests
cargo clippy --workspace --all-targets  # must stay at 0 warnings
cargo build -p prune-juice-cli
./target/debug/prune-juice              # interactive on a TTY; one-shot otherwise
./target/debug/prune-juice --no-tui     # force the one-shot report
./target/debug/prune-juice --apply      # reclaims the safe tier only

cargo run -p prune-juice-tui --example preview   # render every screen, no TTY needed
```

## Deliberately not implemented — do not "fix" these

- **Only *empty* and *derivative* volumes can reach the safe tier.** A volume
  holding anything else — a database, user data, or contents we do not
  recognise — stays irreversible until the vault exists. An **unprobed** volume
  is never assumed empty: on Docker Desktop the data root is hidden inside a VM,
  and the honest answer there is "cannot be proven safe". Enforced in
  `plan/tier.rs::destabilises`.
- **The container-based probe is not implemented.** `VolumeAccess::detect`
  returns unavailable on Docker Desktop, Colima and Podman, so those runtimes
  currently get no volume reclamation at all. That is correct, not a bug to
  paper over.
- **No image can reach the safe tier.** Image layer stacks are not fetched, so
  base-image relationships are unproven. Recorded honestly as
  `Opacity::ImageLayersUnknown` rather than assumed away.
- **`prune-juice-ffi` does not exist.** A staticlib crate slows every workspace
  build and breaks `cargo install` for no present benefit. Subprocess is the
  shipping architecture for the macOS app; UniFFI is an optimisation to take
  only if profiling demands it.
- **No SQLite index yet.** Tier 1 deliberately does not depend on it, so this is
  a missing feature rather than a broken one. Attribution is cold-start only.
- **The TUI only ever actions the safe tier.** Orphaned and stale items are
  inspect-only there. Deleting an orphaned volume is irreversible until the
  vault exists, and two keystrokes is the wrong amount of friction for that.
  The CLI still permits it via `--tiers orphan --apply` for someone explicit.
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
16. **Permission to delete is a value, not a flag.** `SafeToDelete` has private
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
- Warm scan is ~3–6 s, over the sub-2 s target. The fix is the size cache: an
  orphaned volume's size is immutable, so its cache entry is valid forever, and
  the expensive `df` is only needed for in-use volumes — exactly the ones that
  will never be deleted.
- bollard cannot encode `?type=` on `/system/df` (`serde_urlencoded` rejects a
  `Vec`), so the planned filter optimisation is unavailable. One unfiltered call
  returns volumes and build cache together, which is what `data_usage()` does.
- `--apply` has never been run against a real daemon. The apply path is covered
  by unit tests with a spy client, and dry-run exercises the identical code path
  including per-item revalidation, but a real end-to-end deletion is unverified.
  **Ask before running it** — it is 89 containers, 20 networks and ~12 GB of
  build cache on the author's machine.
