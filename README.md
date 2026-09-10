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

## Usage

```
prune-juice                    # interactive on a terminal, one-shot otherwise
prune-juice --no-tui           # force the one-shot report + dry run
prune-juice --apply            # reclaim the safe tier
prune-juice --tiers free       # which tiers to act on (default: free)
prune-juice --only-label K=V   # hard fence: nothing else is reachable
prune-juice --json             # NDJSON on stdout, progress on stderr
prune-juice --no-sizes         # skip the expensive `system df`
prune-juice --roots a:b        # where to look for projects
prune-juice --context NAME     # one context only

prune-juice --vault            # list preserved copies
prune-juice --vault-dump VOL   # preserve one now, delete nothing
prune-juice --vault-verify     # re-read every copy and confirm it
prune-juice --vault-restore ID # put a volume back
prune-juice --waivers          # what is being held back by hand
prune-juice --waive volume:x --reason "..."   # hold something back
```

The executor always runs; without `--apply` it runs in dry-run mode, which
exercises the identical path including per-item revalidation. Dry-run is an
oracle for the apply path, not a separate and less-tested branch.

Exit codes: `0` nothing to report, `1` reclaimable space or unattributed
resources found, `2` usage, `3` daemon unreachable, `4` permission denied,
`5` partial failure, `130` cancelled. Suitable for `set -e` gates.

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
