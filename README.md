# Prune Juice

Reclaim Docker disk without unknown risk of deletion.

```
$ prune-juice

  orbstack  ·  OrbStack  ·  engine 29.4.0 (API 1.54)

    255 containers     144 images (82.5 GB)     259 volumes (35.8 GB)
     24 networks       274 build cache records (12.1 GB reclaimable)

  SAFE TO RECLAIM — 12.1 GB
       12.1 GB  rebuildable   (109 items + build cache)
           0 B  irreversible
    Nothing here can be lost.

  NEEDS REVIEW — 9 orphaned (390 MB), 151 stale
    nbk_mysql                                   379 MB  belongs to "nbk", whose directory is gone
    nbk_tmp                                    10.5 MB  belongs to "nbk", whose directory is gone
    nbk_s3                                     20.8 kB  belongs to "nbk", whose directory is gone
    nbk-wordpress-1                                  —  belongs to "nbk", whose directory is gone
    nbk-s3-init-1                                    —  belongs to "nbk", whose directory is gone
    nbk-mysql-1                                      —  belongs to "nbk", whose directory is gone
    nbk-nginx-1                                      —  belongs to "nbk", whose directory is gone
    nbk-s3-1                                         —  belongs to "nbk", whose directory is gone
    nbk_default                                      —  belongs to "nbk", whose directory is gone
```

**Status: M5.** `prune-juice` on a terminal opens an interactive interface;
piped or under CI it prints a one-shot report. Dry-run by default. The content
probe now lets *empty* and *derivative* volumes be reclaimed; everything else
stays irreversible until the vault lands.

Irreversible items are now preserved before removal, not merely inspected: the
volume is dumped, fsynced, re-read, and deleted only once its digest and tar
entry count both match. Verified end-to-end — a volume dumped, deleted and
restored comes back byte-for-byte, labels included.

Works on Docker Desktop as well as OrbStack and native Linux. Where the data
root can be read from the host it is; where it cannot — every VM-backed runtime
— the volumes are bound read-only into a throwaway container instead. Both paths
share the same classifier and return the same answers. `--apply` reclaims the safe tier and nothing else.

## The problem

`docker system prune -a` cannot tell a build cache layer from the only copy of a
client database, so the safe-feeling option is to run nothing, and the footprint
grows until the disk fills. On the machine above that is 136 GB.

The gap is that Docker keeps provenance and disk accounting in separate
namespaces. It knows `oak_mysql` is 516 MB. It does not surface that `oak` lives
at `~/Documents/Repos/oak` and last ran five days ago. A human has to hold that
mapping, and at 259 volumes nobody does.

## The one thing to understand

Provenance in Docker is **asymmetric**:

| | carries an absolute host path? |
|---|---|
| containers | **yes** — `com.docker.compose.project.working_dir`, `com.ddev.approot`, `devcontainer.local_folder` |
| volumes | **no.** Never. Not one of 259 on the reference machine. |
| images | no — a project name only |

So containers are harvested first into a catalog of project → path, and
everything else is resolved against it.

The consequence is the single most important fact about this tool:
**`docker container prune` destroys provenance permanently.** Of 97 anonymous
volumes here, 47 were still traceable through a stopped container's mounts. The
other 50 are already lost — no back-reference, no label, no path — because their
containers are gone.

So the tool keeps its own notes. Every scan records container→volume→project
edges to a local index *before* anything is judged, which is what makes those 47
mappings durable rather than one `docker container prune` away from oblivion.
The index also answers a question Docker cannot: how long has this been like
this. An orphan verdict needs the project directory missing across two
consecutive scans, because one observation cannot tell a deleted project from an
unplugged disk.

## Things worth knowing

**Contexts are not identities.** `default` and `orbstack` on this machine are the
same engine, because `/var/run/docker.sock` is a symlink. Everything is keyed by
the daemon's own `/info` ID, so the two collapse into one and 136 GB is not
counted twice.

**A present-but-empty label is not a value.** The supabase CLI stamps
`com.docker.compose.project.working_dir` with `""`. A "does the key exist" check
attributes every supabase resource to a project rooted at `""`.

**Absence of a directory is not evidence of deletion.** An unmounted external
disk and a not-yet-cloned repo look exactly like a deleted project. If the parent
directory is also unreadable the verdict is *unverifiable*, never *absent* — a
single run must not be able to orphan an entire root.

**Absence of a `.git` directory is not evidence of abandonment.** Three current
projects here have no VCS at all. This is a hard rule with a regression test.

**Longest-prefix matching, not suffix-stripping.** Stripping `_mysql` and
`wordpress` from `cedar-wordpress_mysql` leaves `cedar`, which matches nothing,
and the volume gets reported as an orphan while `cedar-wordpress` sits on disk.
Matching forward against known project names, longest first, is correct.

**A declaration is a reference.** A volume named in a live project's compose file
is referenced whether or not anything is running. Without that edge, a project
you simply have not started today looks abandoned.

**The build-cache figure is the conservative one.** `docker buildx du` reports a
"reclaimable" total that includes records shared between builders; only the
subset that is neither in use nor shared is counted here, which on this machine
runs about 25% lower. Over-promising reclaim is a trust bug, so the tool
under-promises and over-delivers rather than the reverse.

## Tier 1 is a conjunction, not a synonym for "unused"

> **Tier 1 Theorem.** Deleting a Tier 1 item destroys no information that cannot
> be recovered from something that still exists afterwards.

Three independent properties, all required:

1. **Unreferenced** — with a proof. Every container is a referrer in every
   state, and `Unknown` blocks: swarm, a remote daemon, or an unreadable
   project root all mean something could hold a reference we cannot see.
2. **Reconstructible** — the bytes come back, or were worthless.
3. **Tier-stable** — removing it degrades nothing else. This is the one that
   gets forgotten: deleting an exited container is safe by the first two and
   destroys the only link between its anonymous volumes and their project, so a
   container that mounts anything is excluded.

The run-level summary states the third property as a number rather than a
promise, and the word "irreversible" only ever appears beside a non-zero one.

## Two bugs worth knowing about

Both were found by running the tool against a real machine, and both now have
regression tests.

**Opacity must be scoped.** Three unreadable project roots were making all 259
volumes `Unknown` — one unplugged disk darkened everything. An unreadable root
now only conceals the volumes that could plausibly belong to it.

**A read-set must contain stable facts.** Recording raw elapsed seconds meant
every witness went stale during the pre-apply re-scan, so nothing could ever be
applied. It now records the fixed creation timestamp and whether the age gate
passed — the facts the verdict actually rested on.

## Confidence gates deletion

| level | meaning |
|---|---|
| `Proven` | an authoritative label naming a path that exists, **and** the project's config declares this resource |
| `Strong` | an authoritative label whose path exists |
| `Weak` | no label; a historical edge or a naming convention |
| `Guess` | pure name shape. The heuristic provider is capped here |

A `Guess` may raise a question. It may never justify removing anything. And any
claim from a project that still exists vetoes an orphan verdict from a claim on
one that does not.

## Tutorial

Every command below was run for real, and every block of output is copied from
what it printed — nothing here is illustrative. Two provenances, since the
numbers differ and the reason matters: the text reports come from a live daemon
as it stands now, and the interactive screens come from
`cargo run -p prune-juice-tui --example preview`, which renders the same state
machine against a snapshot captured *before* this machine was cleaned up. That
is why the interactive view below shows 255 containers and 12.1 GB of build
cache where the report shows 153 and 29.5 MB: the earlier figures are what a
neglected machine actually looks like, which is more useful to see.

### Install

```sh
git clone https://github.com/…/prune-juice && cd prune-juice
cargo build --release -p prune-juice-cli
install -m 755 target/release/prune-juice /usr/local/bin/
```

You need a Docker daemon reachable on a local unix socket. OrbStack, Docker
Desktop, Colima and native Linux all work, and the tool detects which one it is
talking to — that detection is what decides whether volume contents can be read
from the host or have to be read through a container.

### 1. Look first

```sh
prune-juice
```

That is the whole interface. On a terminal it opens an interactive view; piped,
redirected, with `CI` set in the environment, or given `--json` or `--apply`,
it prints a one-shot report instead — so it drops into a script without
special-casing. The scan is read-only either way: there is no argument you can
forget that turns a look into a deletion.

```
  Prune Juice · orbstack · OrbStack
   12.1 GB  safe to reclaim

        12.1 GB  rebuildable   (2 items + build cache)
            0 B  irreversible

     Nothing here can be lost.

   ❱ Reclaim 12.1 GB
     Review 3 items
     Scan again
     Quit

   255 containers · 144 images (82.5 GB) · 259 volumes (35.8 GB) · 24 networks
   274 build cache records (12.1 GB reclaimable) · 88 projects known

   ↑↓ move   ↵ select   q quit
```

`Reclaim` acts immediately, without a confirmation prompt, and that is
deliberate: the number beside `irreversible` is `0 B`, so there is nothing to
confirm. If that line were not zero it would not be in this bucket.

**`Review`** opens the items that need a decision:

```
 needs review — 3 items ───────────────────────────────────────────────────
❱ nbk_mysql                    379 MB  orphan   belongs to "nbk", whose directory is gone
  nbk_tmp                     10.5 MB  orphan   belongs to "nbk", whose directory is gone
  saffronfields-mariadb        240 MB  stale    contents could not be read, so it cannot …

  ↑↓ move   space tick   a all   e evidence   d act on ticked   esc back
```

Press `e` on a row and it expands the evidence that produced the verdict:

```
❱ nbk_mysql                    379 MB  orphan   belongs to "nbk", whose directory is gone
      irreversible — deleting this cannot be undone until the vault exists
      daemon_in_use = false
      referrer_count = 0
```

Tick rows with `space` (or `a` for all), then `d`. That never deletes directly —
it goes to a confirm screen that states what will be copied to the vault first:

```
  about to act on 3 items · 630 MB

    nbk_mysql                 379 MB  copied to the vault first
    nbk_tmp                  10.5 MB  copied to the vault first
    saffronfields-mariadb     240 MB  copied to the vault first

  3 will be copied to the vault and verified before removal.
  If a copy cannot be made, that item is left exactly where it is.

  press y to go ahead, anything else to go back
```

Only `y` proceeds. Every other key backs out.

### 2. Read the one-shot report

Add `--no-tui` (or pipe it anywhere) to get the same information as text. This
is the real output on the author's machine:

```
  orbstack  ·  OrbStack  ·  engine 29.4.0 (API 1.54)

    153 containers     144 images (82.5 GB)     205 volumes (35.4 GB)
      3 networks        97 build cache records (29.5 MB reclaimable)

  SAFE TO RECLAIM — 29.5 MB
       29.5 MB  rebuildable   (0 items + build cache)
           0 B  irreversible
    Nothing here can be lost.

  IF YOU KNOW YOU CAN REBUILD — opt in with --tiers <name>
    orphan          2 items    1.0 GB   owning project is gone; volumes are vaulted first
    repullable     52 items   31.5 GB   pulled again on next use — costs bandwidth, nothing else
    rebuildable    89 items   49.2 GB   rebuilt from a context that still exists — costs time
    stale         154 items     474 B   dormant, project still exists; volumes are vaulted first

  NEEDS REVIEW — 2 orphaned (1.0 GB), 154 stale
    nbk-wordpress:latest      984 MB  belongs to "nbk", whose directory is gone, and no
                                      project directory for "nbk" — nothing left to build from
    nbk-nginx:latest         63.3 MB  belongs to "nbk", whose directory is gone, and no
                                      project directory for "nbk" — nothing left to build from

  UNATTRIBUTED — 9 volumes (6.2 GB), never offered for deletion
  attribution: proven 136  strong 11  weak 47  guess 4  none 7

  Nothing was changed. Re-run with --apply to reclaim the safe tier.

  DRY RUN — nothing was touched
  would delete 0 ()   skipped 0   problems 0
  would free 29.5 MB (as Docker counts it)
  build cache: 29.5 MB reclaimed
```

Five blocks, and it is worth knowing what each one is claiming:

| Block | What it means |
|---|---|
| `SAFE TO RECLAIM` | proven reconstructible. `irreversible` is `0 B` or it is not here |
| `IF YOU KNOW YOU CAN REBUILD` | real space, behind a cost you have to accept yourself |
| `NEEDS REVIEW` | a verdict was reached, and it wants you to look at the evidence |
| `UNATTRIBUTED` | ownership unknown. **Never offered for deletion, in any tier** |
| `attribution` | how the 205 volumes were identified. `guess` can never license a delete |

Progress goes to stderr, the report to stdout, so `prune-juice --no-tui > report.txt`
gives you a clean file and still shows you the spinner.

### 3. Reclaim the safe tier

```sh
prune-juice --apply
```

That reclaims tier `free` and nothing else. On this machine it is build cache,
empty volumes and unused networks — items where deleting destroys no
information that cannot be re-derived from something that still exists
afterwards. There is no prompt because there is nothing to weigh up.

Two things happen that are easy to miss:

- The provenance index is written **before** anything is judged. A container's
  labels are the only place an anonymous volume's origin lives, so removing
  containers is what makes volumes unidentifiable — the record is committed
  first, on purpose.
- Host disk is measured before and after, then polled until it settles. That
  number is reported separately from Docker's own figure and never added to it.

### 4. Go further, deliberately

Everything past `free` is opt-in with `--tiers`, and each tier states its price
because a cost you cannot see is a cost you cannot consent to:

| Tier | What it is | What being wrong costs you |
|---|---|---|
| `free` | proven reconstructible | nothing — this is the default |
| `repullable` | images pullable again from a registry | bandwidth, and time on next use |
| `rebuildable` | images built from a context that still exists | build time, and an old build with network-install steps may not reproduce |
| `orphan` | the owning project's directory is gone | the project really being gone. Volumes are vaulted first |
| `stale` | dormant, but the project still exists | a slow first start next time. Volumes are vaulted first |

**`--tiers` replaces the set, it does not add to it.** `--tiers orphan` acts on
orphans only — build cache is left alone. To do both, list both:

```sh
prune-juice --tiers free,repullable --apply
```

Always dry-run first. Without `--apply` the executor still runs, through the
identical code path including per-item revalidation, and tells you exactly what
it would touch:

```sh
$ prune-juice --no-tui --tiers orphan
  DRY RUN — nothing was touched
  would delete 2 (2 images)   skipped 0   problems 0
  would free 1.0 GB (as Docker counts it)
    nbk-wordpress:latest  ·  nbk-nginx:latest
```

Dry-run is an oracle for the apply path, not a cheaper and less-tested branch.

One deliberate limit on the figures: image sizes are summed naively, so a
shared base layer is counted once per image that uses it. The `repullable` and
`rebuildable` totals are therefore upper bounds. An actual run reports what it
really freed, host-measured.

#### Why an orphan needs two runs

The first time a project directory goes missing you get this, and no offer:

> looks orphaned but has only been missing across 1 scan — run again to confirm

One observation cannot tell a deleted project from an unplugged external disk.
Run it again another day and the verdict firms up. If the *parent* directory is
also unreadable the answer is `Unverifiable` and never `Absent` — one unmounted
disk must not orphan an entire root.

Point it at where your projects actually live:

```sh
prune-juice --roots ~/Documents/Repos:~/work
```

### 5. The vault: the only real undo

There is no trash can for Docker volumes. Docker has no volume rename, and on
macOS the bytes live inside a Linux VM the host cannot touch. A verified copy
is the only honest deferred deletion, so that is what the vault is.

Anything irreversible is dumped, fsynced, re-read, and only deleted once the
sha256 **and** the tar entry count both match. A failed dump leaves the volume
exactly where it was and reports `PreserveFailed`.

```sh
prune-juice --vault                 # what is preserved
prune-juice --vault-dump oak_mysql  # preserve one now, delete nothing
prune-juice --vault-verify          # re-read every copy and confirm it
prune-juice --vault-restore ID      # put a volume back, labels and all
prune-juice --vault-forget ID --reason "…"   # discard a copy, irreversibly
```

```
$ prune-juice --vault

  vault: /Users/x/Library/Application Support/prune-juice/vault
  3 entries, 22.9 MB on disk

    nbk_s3-1789021964      nbk_s3       5.9 kB  —
    nbk_tmp-1789021964     nbk_tmp      5.0 MB  —
    nbk_mysql-1789021964   nbk_mysql   17.9 MB  MySQL

  restore with:  prune-juice --vault-restore <id>
```

The last column is what the content probe recognised. The vault lives on the
host filesystem and outside any cache directory, so a later
`docker system prune` cannot eat your backups.

`--vault-dump` reads through a container that is **created and never started**.
Booting Postgres against a real data directory triggers crash recovery and
catalog writes — that would mutate the thing being preserved.

`--no-vault` does not mean "delete without a backup". It means irreversible
items are **refused**, because declining a backup is not consent to lose data.

### 6. Waivers: leave this alone, and say why

```sh
$ prune-juice --waive volume:saffronfields-mariadb \
    --reason "live client DB, keep until the March migration"
  volume:saffronfields-mariadb will be left alone: live client DB, keep until the March migration

$ prune-juice --waivers
  waivers: /Users/x/.config/prune-juice/waivers.json
    volume:saffronfields-mariadb     live client DB, keep until the March migration

$ prune-juice --unwaive volume:saffronfields-mariadb
  volume:saffronfields-mariadb is no longer waived
```

Selectors are `volume:name`, a bare `name` matching any kind, or a `prefix*`.
Two rules are enforced rather than suggested:

```sh
$ prune-juice --waive volume:x --reason "later"
error: a waiver needs a real reason (12 characters or more) — six months from
now it has to explain itself                                          # exit 2

$ prune-juice --waive '*' --reason "a perfectly long reason here"
error: a waiver of `*` would silence the whole tool; waive specific resources
                                                                      # exit 2
```

An exclusion nobody can explain later is worse than no exclusion, and a blanket
waiver is not a waiver — it is uninstalling the tool while leaving it on disk.

Each waiver stores the evidence hash it was granted on. When those facts change
the waiver is surfaced as stale rather than honoured silently for ever; it is
never revoked for you.

### 7. Docker Desktop

Docker Desktop keeps its data root inside a VM, so the host cannot read volume
contents. The tool detects this and falls back to a probe container: the volumes
bound **read-only**, no network, a read-only root filesystem. Both paths share
the same classifier, so a volume is judged identically however its bytes were
read — verified on the reference machine, where both independently returned the
same 50 volumes.

```sh
prune-juice --container-probe   # force it even where the host could read directly
prune-juice --no-probe          # skip reading contents entirely
```

`--no-probe` only ever *shrinks* what is offered. An unprobed volume is never
assumed empty: on Docker Desktop the honest answer is "cannot be proven safe",
so it stays out of the safe tier.

The probe never pulls an image. It picks one already present locally, and if
there is none it says so and reclaims no volumes rather than making an outbound
request on a metered or air-gapped machine.

### 8. Scripting and CI

```sh
prune-juice --json                          # NDJSON on stdout, progress on stderr
prune-juice --apply --only-label com.docker.compose.project=ci-run-42
prune-juice --no-sizes                      # skip the expensive `system df`
prune-juice --deadline 30                   # give up and report what was gathered
prune-juice --context orbstack              # one context only
```

**`--no-sizes` is for scanning fast, not for reclaiming.** bollard cannot encode
`?type=` on `/system/df`, so a single unfiltered call is what supplies volume
sizes *and* the build cache record list. Skipping it hides both:

```
$ prune-juice --no-tui              →  97 build cache records (29.5 MB reclaimable)
                                       SAFE TO RECLAIM — 29.5 MB
$ prune-juice --no-tui --no-sizes   →   0 build cache records (0 B reclaimable)
                                       SAFE TO RECLAIM — 0 B
```

Both figures are honest about what was measured, but the second is easy to
misread as "nothing to clean". It also changes what `--apply` does: the build
cache prune is gated on a non-zero reclaimable figure, so `--no-sizes --apply`
reclaims no build cache at all and reports that it freed nothing.

`--json` emits one envelope per line, flushed as it goes, so a consumer renders
progressively:

```sh
prune-juice --json | jq -r 'select(.event=="classified" and .tier=="free")
  | "\(.kind)\t\(.name)\t\(.bytes)"'
```

**`--only-label` is a hard fence**, not a filter: nothing without that label is
reachable at all. It is how the integration tests are kept from touching real
resources, which is also why it is trustworthy. One caveat, stated in the
`--help`: it skips build cache entirely, because build cache records carry no
labels and the fence could not be honoured for them.

`--deadline` defaults to 120 seconds (`0` waits indefinitely). "Slow" and "hung"
are indistinguishable to a user, and `docker system df` walks every volume
directory — a known multi-minute stall on some setups. On expiry the report is
marked stale, says what it gave up on, and nothing unread is treated as
provably safe.

Exit codes are semantic, so `set -e` gates work:

| Code | Meaning |
|---|---|
| `0` | nothing needing attention |
| `1` | reclaimable space or unattributed resources found |
| `2` | usage error — including a refused waiver |
| `3` | daemon unreachable |
| `4` | permission denied |
| `5` | partial failure — something was refused or failed |
| `130` | cancelled |

```sh
# fail a nightly job if Docker debris is accumulating
prune-juice --no-tui > /dev/null || [ $? -ne 1 ] || echo "docker debris found"
```

Exit `130` comes from cancelling a scan in the interactive view. Note that
there is **no signal handler yet**: Ctrl-C during a one-shot run terminates the
process the ordinary way. That is harmless during a scan, which is read-only,
but it means an `--apply` interrupted by Ctrl-C stops wherever it was rather
than finishing the item in flight. Let an apply run to completion.

### Command reference

```
prune-juice                    # interactive on a terminal, one-shot otherwise
prune-juice --no-tui           # force the one-shot report + dry run
prune-juice --apply            # actually delete; without it nothing is touched
prune-juice --tiers LIST       # comma-separated; replaces the set (default: free)
prune-juice --only-label K=V   # hard fence: nothing else is reachable
prune-juice --json             # NDJSON on stdout, progress on stderr
prune-juice --no-sizes         # skip the expensive `system df`
prune-juice --roots a:b        # where to look for projects
prune-juice --context NAME     # one context only
prune-juice --deadline SECS    # default 120, 0 for indefinite
prune-juice --no-probe         # do not read volume contents
prune-juice --container-probe  # always read contents through a container
prune-juice --no-vault         # refuse irreversible items instead of vaulting

prune-juice --vault            # list preserved copies
prune-juice --vault-dump VOL   # preserve one now, delete nothing
prune-juice --vault-verify     # re-read every copy and confirm it
prune-juice --vault-restore ID # put a volume back
prune-juice --vault-forget ID --reason "…"    # discard a copy
prune-juice --waivers          # what is being held back by hand
prune-juice --waive SEL --reason "…"          # hold something back
prune-juice --unwaive SEL      # release it
```

### Troubleshooting

**`cannot reach the Docker daemon` (exit 3).** Docker is not running, or
`DOCKER_HOST` points somewhere wrong. The message includes the endpoint it
tried.

**`permission denied talking to Docker` (exit 4).** The socket is root-owned, or
your user is not in the `docker` group. Starting Docker will not help; this is
the one case where it is already running.

**A volume you expected to be offered is not.** Almost always one of three
things, and the report says which: its contents could not be read (so it cannot
be proven safe), it is `UNATTRIBUTED` (never offered, in any tier), or its
project has only been missing across one scan.

**The number went down by less than promised.** Docker-reported and
host-measured are different numbers, shown on separate lines and never summed.
Overlay2 shares layers between images, so per-image figures over-count; the
host-measured delta after a run is the real one. Where the host cannot be
measured — a remote daemon — it says "could not be measured" rather than
reprinting Docker's figure in disguise.

**Nothing at all is offered on Docker Desktop.** Expected on a first run if no
image is present locally for the probe container to use. Pull any small image
and run again.

## Design

Two traits, deliberately separate. `DockerClient` is read-only; `DockerMutate`
is destructive. That split is the **deletion firewall** — a read-only client
cannot delete even by accident, and every mutating call passes `force: false` so
the daemon's own in-use check stays armed. A refusal from the daemon is the
system working.

Permission to delete is a value, not a flag. `SafeToDelete` has private fields
and no public constructor, so only the planner can mint one. `Fresh` is produced
solely by `revalidate()` and consumed by the executor, so a witness cannot
outlive its own check.

`bollard` types never escape `crate::docker`. If bollard ever becomes a problem,
replacing it is roughly 600 lines over `hyperlocal` and named pipes, and touches
nothing else.

Events are delivered through a callback trait rather than a `Stream`, because a
stream cannot cross an FFI boundary without an executor on the foreign side. A
stream or channel can be built from a callback in ten lines; the reverse is not
true. `tokio` is contained inside one module and never appears in the public API.

## Dependencies

Rust 1.85+. No system libraries — SQLite is bundled. `cargo build --workspace`.

## Roadmap

M1 read-only scan · M2 planner and safe tier · M3 interactive TUI · M4 content
probe and vault · M5 review actions, waivers, host disk measurement — all done.
The provenance index is in too. M6, the macOS app, is next.

MIT.
