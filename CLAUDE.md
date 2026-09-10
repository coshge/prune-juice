# CLAUDE.md

Orientation for a fresh session. Read this first, then `README.md` for the
design rationale.

## What this is

`prune-juice` reclaims Docker disk space with per-item provenance and a safety
model strong enough that the headline action needs no confirmation. Free MIT
Rust CLI; a free macOS app comes later, architected so a one-time paid tier
could be added without rework.

## Status: M8 — known-gap cleanup

| milestone | state |
|---|---|
| M1 read-only scan with provenance attribution | done |
| M2 planner, tier classification, apply path | done |
| M3 interactive ratatui TUI | done |
| M4a content probe — native **and** container path | done |
| M4b vault (dump / verify / restore) | done |
| M5 review actions, waivers, host disk measurement | done |
| M6 macOS app — window, scan, review; unsigned | done |
| M7 updates — GitHub Releases, Sparkle, CLI self-update, CI | done |
| README tutorial | done |
| M8 known-gap cleanup — exclusive image sizes, measured writable layers, SIGINT, remote gate, size cache | done |

```
cargo test --workspace                  # 286 tests
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
./target/debug/prune-juice --no-sizes          # skip df; remembered volume sizes
./target/debug/prune-juice --remote --reason "..."   # allow a tcp:// or ssh:// daemon
./target/debug/prune-juice --waivers          # what is being held back by hand
./target/debug/prune-juice --waive SEL --reason "..."   # hold something back

./target/debug/prune-juice --check-update       # ask now (0 current, 1 newer, 5 could not)
./target/debug/prune-juice --update             # install the newest release
./target/debug/prune-juice --update-check off   # stop checking, and remember that
./target/debug/prune-juice --no-update-check    # skip it for one run

cargo run -p prune-juice-tui --example preview   # render every screen, no TTY needed

cargo run -p xtask -- manifest                  # the document the CLI reads
cargo run -p xtask -- appcast                   # the document Sparkle reads

cd app/PruneJuice && ./scripts/bundle.sh        # assemble PruneJuice.app (ad-hoc signed)
SPARKLE_PUBLIC_KEY=… ./scripts/bundle.sh        # …with updates enabled
SIGN_ID="Developer ID Application: …" ./scripts/bundle.sh --notarize
open app/PruneJuice/dist/PruneJuice.app
```

App diagnostics land in `~/Library/Logs/prune-juice-app.log`. A GUI launch has
no terminal, so that file is the only way to see why a scan failed.

**There is no release-signing key in this checkout**, so nothing here checks
for updates: `verify::release_key()` returns `None` and the whole feature
reports itself absent. That is the correct state for a working tree — see
`RELEASING.md`. To exercise the real path locally:

```
PRUNE_JUICE_UPDATE_PUBKEY=RW… cargo build -p prune-juice-cli
PRUNE_JUICE_UPDATE_URL=https://…/update-manifest.json \
  ./target/debug/prune-juice --check-update
```

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
  an old build may not reproduce). Both are opt-in through the interactive
  review flow; `--tiers` is the non-interactive equivalent.
- **The app uses the bundled CLI for all operations.** It scans, previews and
  reclaims whole tiers after review, manages the vault and waivers, and exposes
  all scan options. Keep the AppKit window lifecycle and CLI safety checks.
  See `app/PruneJuice/README.md` for UI coverage and verification.
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
- **The TUI can act on every offered tier.** Pullable/rebuildable resources and
  stale/orphaned resources have separate review groups on the main screen. Tick
  with space, `d` to act, and it goes via a confirm screen that states the
  re-pull, rebuild, vault or permanent-loss cost. Only `y` proceeds; every other
  key backs out. Never straight from a list keypress to a deletion.
- **Apply progress is emitted around the blocking call.** `ApplyProgress::Removing`
  is sent before asking Docker to delete an item, followed by its terminal
  outcome. The TUI forwards these events directly and keeps only ten activity
  lines, so a slow daemon remains visibly active without large-run UI overhead.
- **There is no HTTP client in the dependency graph.** The updater fetches
  through the platform's `curl`, behind a `Fetcher` trait. A TLS stack would be
  the single heaviest thing in this workspace, added for a peripheral
  convenience — the same reasoning that kept `prune-juice-ffi` out. It is safe
  because nothing is trusted for having been fetched: the manifest is verified
  in-process against a compiled-in Ed25519 key, so a missing, old, or
  subverted `curl` can only ever cause "no update", never a bad one. Swapping
  in `ureq` later means writing one more `Fetcher`.
- **A build with no release key has no update system, not an unverified one.**
  `RELEASE_KEY` is empty in the repository and `verify::release_key()` returns
  `None`, which disables *checking* as well as installing. An unverifiable
  version number is not a lesser form of news; it is a stranger telling you to
  go and download something. Same rule in the app: no `SUPublicEDKey` and
  Sparkle is never started.
- **The updater never overwrites a binary a package manager owns.** The check
  still runs and still reports the version; only the action changes, to the
  command that manager understands. Overwriting a Homebrew binary in place
  would leave the manager serving a version it did not install, and the next
  `brew upgrade` would silently undo the update. `origin.rs`.
- **The app updates the whole bundle, never the helper alone.** Replacing one
  file inside a signed bundle breaks its seal, and an app and helper on
  different versions is a protocol mismatch waiting to happen. So the helper's
  origin is `AppBundle` and it refuses to self-replace.
- **`--only-label` skips the build cache entirely.** Build cache records carry
  no labels, so the fence cannot be honoured for them; pruning it anyway would
  break the promise the flag makes.
- **An unmeasured writable layer is not an empty one.** `size_rw` used to be
  taken as zero when absent, and absent is what it always was — the container
  listing does not ask for sizes, because that is the expensive per-container
  path. So every container was treated as holding nothing, and one holding a
  database dump could reach the tier that deletes without asking. The figure
  now comes from `/system/df`, which is already being called for volumes, and
  `None` means "cannot be proven empty" and stays out of the safe tier with
  that as its stated reason. This is why `--no-sizes` offers no containers.
- **A remote daemon is refused unless it is asked for by name.** `--remote`
  plus a `--reason` of twelve characters or more, the same rule a waiver
  follows. A remote engine's disk is not this machine's disk, its projects are
  not the directories being searched, and host reclamation cannot be measured
  at all — so scanning one is a deliberate act, not a default. Each excluded
  context is named in the output; a remote daemon dropped in silence looks
  exactly like a daemon that is not there. Vault preserve and restore stay
  local-only regardless, because pulling a volume across a network to back it
  up is a different operation from the one that flag authorises.
- **An interrupted apply returns its receipt, not an error.** Ctrl-C sets the
  same `Cancel` the scan already consulted; the executor stops between items —
  never inside one — skips the build cache, and reports what it did. A run
  that deleted eleven things and told you about none of them is worse than the
  interruption. A second Ctrl-C `_exit`s immediately, because the second press
  is not a request to be patient.

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
10. **A container carrying uncheckpointed provenance is not free.** If the
    current container-to-volume edges were not durably committed, removing a
    mounted container is a provenance loss. After a successful checkpoint the
    saved edge can satisfy tier-stability. A mountless last container recording
    its project's absolute path remains protected because there is no edge on
    which to preserve that path.
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
    generic container check never fires for one. Ask `graph.image_containers()`
    directly. Running containers protect the image; stopped containers block
    it until they are reclaimed and a rescan proves the image is unlocked.
    Docker refuses both without force, and force remains forbidden.
32. **Every opt-in tier must state its price.** `Tier::caveat()` is not
    decoration; a tier a user cannot cost is a tier they cannot consent to.
33. **Permission to delete is a value, not a flag.** `SafeToDelete` has private
    fields and no public constructor; `Fresh` comes only from `revalidate()` and
    is consumed by the executor. Do not add a public constructor or a `Clone`.
34. **Classify daemon errors by variant, never by message text.** `map_err` in
    `bollard_client.rs` used to grep for "connection refused" and friends.
    bollard reports a failed connect as `SocketNotFoundError` or, one layer
    down, "client error (Connect)" — neither matched, so an unreachable daemon
    exited 1 (findings) instead of the documented 3. Match the variant, and
    below it the innermost `io::ErrorKind`. Note that `IOError` is
    `#[error(transparent)]`, so `source()` forwards *past* the wrapped error
    and a chain walk alone cannot see it.
35. **An update check never delays or fails a scan.** The cache is read
    synchronously because that is a file read; the network is only ever touched
    on a detached thread whose result is used if it arrives and dropped if it
    does not. Every failure — no network, no `curl`, a 404, a bad signature —
    resolves to "no news". There is a test that a dead server produces
    `Decision::Failed` and never a propagated error.
36. **Nothing in a manifest is read before its signature verifies.** Not the
    version, not a URL, not a digest. `Manifest::parse` is private and
    `Checker::fetch_manifest` is the only way to obtain one, which is the same
    shape as `SafeToDelete`: a type you cannot hold without having done the
    check. Artifacts are then authenticated by the SHA-256 the signed manifest
    names, so the release host is not trusted at all.
37. **Only the pre-hashed minisign form is accepted.** `verify` passes
    `allow_legacy: false`. Accepting both would mean accepting the weaker one,
    and an attacker gets to choose which they present. There is a fixture pair
    — one signature in each format over the same bytes — asserting the modern
    one verifies and the legacy one does not.
38. **The new binary has to run before it replaces the old one.** A signature
    says who built a file; only executing it says the platform will accept it.
    The staged binary is asked for `--version` and must report exactly what the
    manifest promised. Then one `rename`, which is atomic: either the new
    binary is there or the old one is, never neither.
39. **A release archive never chooses where its contents land.** The member is
    found by file name and read into memory; `tar::Archive::unpack` would treat
    the path inside the archive as an instruction. A signature proves who built
    an archive, not that they built it correctly.
40. **A notice is not payload.** Update notices go to stderr, only when both
    streams are terminals, never under `CI`, never with `--json`. `core` cannot
    break this: it returns `Notice::lines()` and does not know what a terminal
    is.
41. **The update preference is config and the last check is cache.** Losing the
    cache costs one request; losing the preference would silently turn checking
    back on. Different directories, and a test asserting they are.
42. **The app never installs over a running helper.** A scheduled check is
    declined while an operation runs, and a relaunch is postponed by holding
    Sparkle's install handler until the helper stops — released on every
    completion, successful or not, and exactly once. Relaunching mid-`--apply`
    would kill the executor between two deletions.
43. **The tag and the workspace version must agree.** Checked in CI before
    anything is built. The updater compares against the version compiled into
    the binary, so a release tagged `v0.2.0` built from `0.1.0` sources is a
    release nobody is ever offered.
44. **Every total is summed over exclusive size, never over `size`.**
    `ResourceSummary::reclaimable_size` and `PlanItem::reclaimable_size` are
    the only figures that may be added up. An image's `size` is its whole
    layer stack, and fifteen project images standing on one 145 MB WordPress
    base each report that base — 82.5 GB of "images" on the reference machine
    that only ever occupied 59.3 GB, and an opt-in tier promising a third more
    than it could deliver. Exclusive size comes from `df`'s `SharedSize`; the
    headline image figure is the daemon's own `LayersSize`, not a sum of ours,
    because only the daemon knows which layers two images share. Where the
    overlap was not computed the fallback is `size`, which over-estimates —
    the honest direction to be wrong in, since the run reports what it
    actually freed.
45. **A remembered size may be reported only for a volume nothing mounts.** An
    unreferenced volume's bytes cannot change, so last run's measurement is
    still true and `--no-sizes` need not report nothing. A mounted volume is
    being written to as we speak, and giving last week's figure for it would
    be inventing a measurement. Labelled `SizeSource::Cache` so the interface
    can say "remembered" rather than implying it just looked, and the report
    stays `stale` either way. Nothing size-derived licenses a deletion, so a
    remembered figure can never widen what is offered.
46. **`df` runs alongside the scan, never in front of it.** `start_data_usage`
    puts it on the runtime's worker threads before the listing, and
    `data_usage` collects it at the sizing phase. It is the slowest call the
    scan makes (~1.4 s here) and nothing between the two points needs its
    answer. Default no-op on the trait, so a client that cannot overlap is
    unaffected and the call sequence is identical either way — still exactly
    one `df`, still degrading to a warning on failure.
47. **Liveness comes from the filesystem; the index only counts.**
    `project_absences` carries a row for every project with any history, and a
    project that is present reads `absent_scans = 0`. The path-recall branch
    read that stored number as the verdict, so every project whose path was
    remembered rather than labelled came out absent — including the ones
    plainly still there. ddev's global services warned "missing across 0
    scan(s) — run again to confirm" on every run, an instruction no number of
    runs could satisfy, and the `Present` claim that exists to veto another
    claim's orphan verdict was thrown away with it (invariant 7's failure mode,
    reached from the other direction). So: `liveness_of` first, and the stored
    count refines the answer only once the directory is actually missing.

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
- A release is a tag. `RELEASING.md` is the whole procedure; nothing is typed
  twice, and the version lives only in `[workspace.package]`.
- Honour `NO_COLOR` and TTY detection.

## Known gaps worth picking up

- Waivers still live in a JSON file rather than the index. Harmless, but they
  could move now that the index exists.
- **No release has been cut yet, so one link is unexercised.** Every piece of
  the update chain is tested — the signature format against a real minisign
  fixture, the whole checker against a signed manifest, the install against a
  tampered archive and a binary that will not run, and the `curl` transport
  against a live 404 — but nothing has yet fetched a *published* manifest over
  HTTPS and verified it, because there is nothing published. The last step of
  `release.yml` does exactly that against the release it just made, which is
  why it is the step to watch on the first tag.
- **There is no Homebrew formula.** `Origin::Homebrew` detection and the
  `brew upgrade prune-juice` hint are correct and tested, but nothing is in a
  tap yet, so that branch is currently unreachable in practice. It costs
  nothing to have ready and is wrong to remove.
- **Warm scan is ~1.6 s per scan, and a dry run does two of them.** Measured
  on the reference machine with `--json` phase timestamps: listing images
  0.6 s, sizing 0.4 s (the rest of `df` having already run alongside the
  listing), probing 160 volumes 0.05 s, attribution 0.05 s. One scan is now
  inside the sub-2 s target; the ~3.3 s a dry run takes is two scans, because
  the executor revalidates every witness against a *fresh* report and that is
  the property that makes dry-run an oracle for the apply path. Reusing the
  planning report would halve the time and make the preview rosier than the
  real thing — a preview that can never report a stale witness. What is left
  to win is the second `df` inside that re-scan, which cannot be skipped for
  the same reason: `size_rw` is part of the evidence a witness rests on, so a
  size-less re-scan would make every witness stale.
- **A remembered volume size is never used when `df` succeeded**, even for a
  volume `df` did not mention. That case means the daemon dropped a volume it
  had previously reported, which is more interesting than a missing number and
  should not be papered over with an old figure.
- **The interrupt handler is unix-only.** `install` is a no-op elsewhere and
  Ctrl-C keeps its default behaviour, which is what it did before. Windows'
  CRT supports `signal(SIGINT, …)` and would need the console-handler path for
  anything more; nothing here is tested on Windows, so it is left honest
  rather than half-claimed.
- bollard cannot encode `?type=` on `/system/df` (`serde_urlencoded` rejects a
  `Vec`), so the planned filter optimisation is unavailable. One unfiltered call
  returns volumes and build cache together, which is what `data_usage()` does.
  **This makes `--no-sizes` sharper than it looks:** it hides the build cache
  as well, so the safe tier reads 0 B and `--apply` prunes none of it (the
  prune is gated on `plan.build_cache_reclaimable > 0` in `execute/mod.rs`).
  It also takes every container out of the safe tier, since a writable layer
  nobody measured cannot be proven empty. Volume sizes are the one thing that
  survives the flag, remembered from an earlier run.
  Documented in the help and the tutorial. The available fix is to prune the
  cache under `--no-sizes` anyway and report what the daemon says it freed —
  `/build/prune` needs no `df` — but that means deleting without a
  pre-measured figure, so it was left as a documented limit rather than a
  silent change of behaviour.
- `--apply` **has** now run for real, twice, with the author's authorisation:
  once scoped to the `nbk` orphans and once across the whole machine. The
  machine went from 255/259/24 containers/volumes/networks to 153/205/3 and
  from 109.16 GB to 99.32 GB host-measured — ~11 GB, and the vault holds three
  verified `nbk` entries (22.9 MB compressed from 390 MB). The dump / verify /
  restore path was separately proven byte-for-byte including labels. So the
  delete step is no longer unexercised — but **still ask before running it**,
  because the opt-in tiers reach a further ~80 GB and a mistake there is not
  recoverable from anything but the vault.
