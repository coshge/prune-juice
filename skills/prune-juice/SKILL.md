---
name: prune-juice
description: Inspect Docker disk usage and work out which project owns which container, image, volume or network. Use whenever the task mentions Docker disk space, reclaiming space, a full disk with Docker installed, orphaned or dangling volumes, "what is using all my disk", stale project containers, docker system prune, or which project a Docker volume belongs to.
---

Thin wrapper skill over the `prune-juice` CLI. The tool does the analysis and,
on a terminal, provides the complete review and reclaim workflow; this file
tells you what its answers mean.

## Default interactive workflow

Run `prune-juice` with no arguments on a terminal. The initial scan is
read-only. All offered cleanup is reachable inside the interface:

- **Reclaim** acts on the safe tier only. These items are proven
  reconstructible and need no second confirmation.
- **Review pullable / rebuildable** exposes images that cost bandwidth or build
  time to restore. Tick with `space` (or `a` for the group), inspect with `e`,
  press `d`, then read the cost summary. Only `y` proceeds.
- **Review stale / orphaned** exposes dormant or abandoned resources. The same
  review flow applies; irreversible volumes are copied to the vault and
  verified before removal.
- The Reclaim row says when its stopped-container cleanup will unlock images.
  Its receipt makes the next step explicit: press `Enter` to scan again and
  continue (`q` quits). Newly-unreferenced images and volumes then move into
  their appropriate review groups.
- Applying shows the current resource, completed/total count, and a bounded
  rolling activity log. A slow Docker deletion should still show its
  `removing` line immediately; a static apply screen indicates an old build.

No tier requires the user to leave the interface and rerun the program with a
flag. Flags remain available for scripting and CI.

## Read-only and automation commands

```
prune-juice                 # interactive on a terminal; initial scan is read-only
prune-juice --json          # NDJSON, one Envelope per line, on stdout
prune-juice --no-tui        # human dry-run report without opening the interface
prune-juice --no-sizes      # skip `system df`; seconds faster, sizes omitted
prune-juice --roots a:b     # colon-separated dirs to search for projects
prune-juice --context NAME  # one context only
```

Progress goes to stderr and the payload to stdout in non-interactive mode, so
`prune-juice --json | jq` composes. If the command is not on `PATH`, call it by
path from the checkout.

## Reading the output

**Orphan candidates.** A `Strong` or better claim naming a project directory
that is not present, with nothing live contesting it. These are the genuinely
interesting rows. Every one carries its evidence — quote it to the user rather
than paraphrasing:

```
nbk_mysql                    379 MB  nbk
    [label] com.docker.compose.project = nbk
    [fs] /Users/x/Documents/Repos/nbk is not present
```

**Unattributed.** No claim reached `Strong`. Usually anonymous 64-hex volumes
whose owning container has been removed, which makes the mapping unrecoverable
from Docker. The honest answer is "I don't know what this was" — **never guess
on the user's behalf**, and never suggest deleting one because it looks unused.

**Unverifiable.** The project path could not be checked, typically because a
parent directory is missing — an unmounted disk, or a repo not cloned on this
machine. This is deliberately *not* the same as absent. Do not treat it as an
orphan.

**Confidence.** `Proven` > `Strong` > `Weak` > `Guess`. A `Guess` comes from
name shape alone (ddev volumes carry no labels at all, so that is all there is).
It may raise a question; it may never justify a deletion.

## Hard rules

- **Never tell the user to run `docker volume prune -a`.** Plain `volume prune`
  removes anonymous volumes; `-a` also removes *named* unused ones, which on a
  typical machine includes databases. On the reference machine that flag would
  have destroyed three.
- **Never suggest `docker container prune` as a first step.** Removing
  containers destroys the only surviving link between anonymous volumes and
  their projects. If containers are to go, the mapping must be recorded first.
- **Never call something abandoned because it has no `.git` directory.**
  Plenty of live projects are not repos.
- **Never call something an orphan because its directory is missing** unless the
  tool says `absent` rather than `unverifiable`.
- Do not act on sizes when the report says they are incomplete; say so instead.

## Exit codes

`0` nothing to report · `1` reclaimable space or unattributed resources found ·
`2` usage error · `3` daemon unreachable · `4` permission denied · `130`
cancelled. A `1` is informational, not a failure.

## When the daemon is unreachable

Exit 3 with a hint. Check that Docker is running before suggesting anything
else; do not retry in a loop.
