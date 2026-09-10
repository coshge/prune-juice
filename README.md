# Prune Juice

Reclaim Docker disk without unknown risk of deletion.

```
$ prune-juice
scanning orbstack… 3305 ms
  context default      is the same engine as one already scanned — skipping

  orbstack  ·  OrbStack  ·  engine 29.4.0 (API 1.54)
  daemon aeb90dba-23d8-412b-8730-1bd999853f68

    255 containers     144 images (82.5 GB)
    259 volumes (35.8 GB)    24 networks
    329 build cache records, 16.3 GB reclaimable
     88 projects known

  ORPHAN CANDIDATES (11) — a confident claim on a project that is gone
    nbk-wordpress:latest                            984 MB  nbk
        [label] com.docker.compose.project = nbk
        [fs] /Users/x/Documents/Repos/nbk is not present
    nbk_mysql                                       379 MB  nbk
        [label] com.docker.compose.project = nbk
        [fs] /Users/x/Documents/Repos/nbk is not present

  UNATTRIBUTED VOLUMES (112, 5.4 GB) — never offered for deletion
    97 are anonymous (64-hex). Their owning containers are gone, so the
    mapping is unrecoverable from Docker.

  VOLUME ATTRIBUTION
    proven 50   strong 100   weak 0   guess 10   none 99
```

**Status: M1.** This build is read-only. It cannot delete anything, because the
only destructive trait has no implementation in the workspace yet.

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
containers are gone. Anything that prunes containers before recording their
edges has destroyed the information needed to explain what it later deletes.

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

**The build-cache figure is the conservative one.** `docker buildx du` reports
20.52 GB reclaimable; only 16.3 GB is neither in use nor shared. Over-promising
reclaim is a trust bug, so the smaller number is what gets shown.

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
prune-juice                 # human report
prune-juice --json          # NDJSON on stdout, progress on stderr
prune-juice --no-sizes      # skip the expensive `system df`
prune-juice --roots a:b     # where to look for projects
prune-juice --context NAME  # one context only
```

Exit codes: `0` nothing to report, `1` reclaimable space or unattributed
resources found, `2` usage, `3` daemon unreachable, `4` permission denied,
`130` cancelled. Suitable for `set -e` gates.

## Design

Two traits, deliberately separate. `DockerClient` is read-only; `DockerMutate`
is destructive and has no implementation yet. That split is the **deletion
firewall** — the replay client used by tests implements only the first, so no
test can delete anything even by accident.

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

M1 read-only scan (done) · M2 planner and one-click safe tier · M3 interactive
TUI · M4 content probe and vault · M5 review, waivers, host disk measurement ·
M6 macOS app.

MIT.
