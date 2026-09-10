# Prune Juice

Inspect Docker disk usage, understand which projects own your resources, and
reclaim space with a clear explanation of what each cleanup will cost to recover.

Prune Juice includes a Mac app and an interactive command-line tool. Both inspect
containers, images, volumes, networks, and build cache. Scanning does not delete
resources. Cleanup is an explicit action, and irreversible volumes are preserved
in a verified local vault before removal.

- Browse resources with ownership information and the reason for each classification.
- Reclaim the Safe tier or review resources that need downloading, rebuilding, or restoring.
- Preserve and restore volumes, and protect resources with reasoned exclusions called waivers.
- See Docker-reported reclamation separately from physical space returned to the host.

## Contents

- [Install](#install)
- [Use the Mac app](#use-the-mac-app)
- [Use the CLI](#use-the-cli)
- [Resource labels and their exact criteria](#resource-labels-and-their-exact-criteria)
- [Vault](#vault)
- [Waivers](#waivers)
- [Updates](#updates)
- [Docker contexts and project folders](#docker-contexts-and-project-folders)
- [Command reference](#command-reference)
- [Scripting and exit codes](#scripting-and-exit-codes)
- [Local data](#local-data)
- [Troubleshooting](#troubleshooting)

## Install

### Requirements

- A running Docker daemon accessible through a local Unix socket. Supported setups
  include Docker Desktop, OrbStack, Colima, and native Linux Docker.
- Rust and Cargo to build from source. The package declares Rust 1.85 or later;
  the repository uses the stable toolchain.
- For the Mac app: macOS 14 or later and a Swift 6 toolchain, provided by Xcode.

### CLI

```sh
git clone https://github.com/coshge/prune-juice.git
cd prune-juice
cargo install --path crates/prune-juice-cli --locked
prune-juice --help
```

Cargo installs the command into `~/.cargo/bin` by default. Make sure that directory
is on your `PATH`. To run without installing:

```sh
cargo build --release -p prune-juice-cli
./target/release/prune-juice --no-tui
```

### Mac app

From the repository root:

```sh
cd app/PruneJuice
./scripts/bundle.sh
open dist/PruneJuice.app
```

The bundle includes the CLI helper, so the app does not need a separate CLI
installation. The default build is signed for local use, not notarized for
redistribution. You can move the completed `PruneJuice.app` to Applications.

## Use the Mac app

The app starts with a scan. Use **Scan again** or **Command-R** to refresh it.

| Screen | What you can do |
| --- | --- |
| **Resources** | Search by resource name, project, kind, or Docker context. Filter by classification and select a row to read its reason and ownership evidence. |
| **Reclaim** | Select cleanup tiers and see the combined size estimate. Preview cleanup without deleting, or review the costs and confirm cleanup. |
| **Vault** | List and verify preserved volumes, preserve a volume, restore a copy, or permanently delete a copy with a reason. |
| **Waivers** | List exclusions, add one with a reason, or remove one. |
| **Activity** | Follow scan and cleanup progress, read results and notices, and copy command output. |
| **Settings** | Choose a context, project folders, label filter, scan deadline, inspection options, vault preservation, and the optional menu bar icon. |

Mac cleanup applies to **all eligible resources in the selected tiers**, not to
individual rows in Resources. The estimate changes with your selection. It includes
eligible build cache only when Safe is checked. Cleanup performs a fresh scan, so
newly eligible resources can also be included within the selected scope.

Changing scan settings requires another scan before cleanup. A new scan resets the
tier selection to Safe. After cleanup or changes to waivers, scan again to refresh
which resources are eligible. Finish an active operation before quitting the app.

Vault and waiver listings appear in Activity. Copy the entry ID or selector into
the relevant action form. Vault preserve and restore use the first discovered local
Docker context, independently of the scan context setting.

## Use the CLI

### Inspect interactively

```sh
prune-juice
```

On an interactive terminal, this opens a scan followed by a menu. It uses the first
local Docker context, with the current context preferred.

| Key or action | Behavior |
| --- | --- |
| Arrow keys, Enter | Navigate and open a menu item. |
| **Reclaim** | Immediately reclaim the Safe tier. The terminal interface does not ask for a second confirmation for this action. |
| **Review pullable / rebuildable** | Review images that need downloading or rebuilding after removal. |
| **Review stale / orphaned** | Review resources that need a decision about their contents or owning project. |
| Space | Select or deselect the highlighted review item. |
| `a` | Select or clear all items in the review group. |
| `e` | Expand the selected item's evidence. |
| `d` | Review the costs of acting on selected items. Only `y` on the confirmation screen proceeds. |
| Escape | Return from review. |
| Enter on the results screen | Scan again. |
| `q` | Quit. |

### Preview and apply from the command line

A one-shot command without `--apply` previews cleanup without deleting resources:

```sh
prune-juice --no-tui
```

Preview the tiers you intend to use, then apply the same selection:

```sh
# Preview Safe and Pull again for one context.
prune-juice --no-tui --context default --tiers free,repullable

# Apply that selection after reviewing the output.
prune-juice --apply --context default --tiers free,repullable
```

`--apply` alone selects only `free`. `--tiers` replaces the default selection,
so `--tiers repullable` does not include Safe or build cache. One-shot application
has no interactive confirmation prompt.

Use `--no-tui` when previewing flags such as `--tiers`, `--only-label`, or
`--no-vault`: the interactive interface manages its own cleanup selections.

Scanning can update local ownership history and create temporary inspection
containers. It does not perform resource cleanup. The inspection container reads
volumes without write access and is removed afterward.

## Resource labels and their exact criteria

The Mac app and CLI use the same classifier. Labels describe why a resource can
or cannot be offered for cleanup. They are not simply age or last-used categories.
Select a resource in the Mac app to see its **Why** and **Evidence**.

| Mac app label | CLI tier | Criteria |
| --- | --- | --- |
| **Protected** | `protected` | A known reference or explicit protection keeps the resource out of cleanup. Examples include volumes held by live containers, volumes declared by an existing Compose project, attached networks, built-in Docker networks (`bridge`, `host`, `none`), and matching waivers. Images remain protected while **any container, including a stopped one**, references them. An image without an identified recovery route stays protected unless it qualifies as orphaned. |
| **Safe** | `free` | Proven unreferenced, reconstructible, safe to remove without losing other resources' provenance, and created **at least 24 hours ago**. Creation time must be known. Additional resource-specific checks are listed below. Images never enter this tier. |
| **Pull again** | `repullable` | An image has no referencing containers and its recovery route is a recorded registry digest. Local-build detection takes precedence. The scanner does **not** contact the registry to verify availability. |
| **Build again** | `rebuildable` | An image has no referencing containers, its name matches the local `<project>-<service>` pattern, the project directory exists, and a recognised Dockerfile is found. The suggested recovery command is `docker compose build <service>`. The scanner does **not** run the build or verify that the Compose service reproduces the image. |
| **Orphaned** | `orphan` | A **Strong or Proven** ownership claim, based on authoritative project evidence, identifies a project absent across **at least two consecutive scans**, with no competing claim pointing to an existing project. A missing parent directory makes the project unverifiable instead. Live-container references block this classification. Recoverable images normally become Pull again or Build again first. |
| **Dormant** | `stale` | Generally an unreferenced resource that fails a Safe requirement: meaningful or unreadable volume contents, recent volume writes, container writable-layer data, provenance that would be lost, or creation less than 24 hours ago. Insufficient ownership evidence sends many of these cases to Unattributed instead. **There is no inactivity period that defines Dormant.** |
| **Unattributed** | `unattributed` | There is not enough evidence to offer cleanup. This can mean uncertain references, missing visibility, insufficient ownership evidence for a risky resource, or missing creation time. A project name can still be known. Never offered for deletion. |

**Dormant is broader than its name suggests.** A newly created resource or a volume
written recently can receive this label. It does not prove that the project has
been unused for days or weeks. Read the individual reason before selecting it.

### Additional Safe checks by resource kind

- **Volumes:** inspection must identify empty contents or a recognised regenerable
  dependency/cache tree. The recognised top-level markers are `node_modules`,
  `vendor`, `registry`, `.cache`, `_cacache`, and `cache`. A marker must be the
  **only meaningful top-level entry**, ignoring housekeeping entries such as
  `.DS_Store` and `lost+found`. A checkout containing `vendor` alongside application
  files does not qualify. Database signatures take precedence over cache markers.
  Database contents, user data, unknown contents, and uninspected volumes are not
  Safe. If the newest observed write is **less than 24 hours ago**, the volume
  cannot be Safe.
- **Containers:** the writable-layer size must be **measured** and zero. Any
  positive size makes deletion irreversible; above 50 MB the classifier also
  gives a specific writable-layer warning. An **unmeasured** layer is not an
  empty one: `--no-sizes`, or a daemon that does not report container sizes,
  leaves every container outside Safe with that stated as the reason. Sizing is
  on by default, and the figure comes from the same call that sizes volumes, so
  no extra work is done for it. Mounted-volume provenance must have been durably
  recorded, and removal must not lose the last unrecorded project-path evidence.
- **Networks:** must be unreferenced, non-built-in, and pass the creation-age check.
- **Build cache:** handled separately as Docker-reported reclaimable cache rather
  than classified rows. It is included when Safe is selected, unless sizing is
  disabled or a label filter is active.

### Recovery detection and classification order

Local image recovery uses the first image tag, falling back to its name. A bare
repository name without `/` is split at its last hyphen into project and service.
The scanner looks up that project in its catalog and configured project roots.
It checks `Dockerfile`, `docker/Dockerfile`, `docker/wordpress/Dockerfile`,
`Dockerfile.nginx`, and directories up to two levels below the project for a
`Dockerfile`. This is a recovery heuristic, not a verified build recipe. A
local-looking name with a missing project or Dockerfile is considered unrecoverable
by this route; it does not automatically fall back to a registry digest.

Otherwise, the first recorded registry digest supplies the pull command. Neither
registry availability nor build reproducibility is tested during a scan.

Order matters: live references are checked first; image container references and
recovery routes have their own branch. Built-in networks are protected. Unknown
reference status blocks the general path. Confirmed orphan ownership is considered
before the general reconstructibility, provenance, and age gates. The 24-hour
creation gate therefore applies to **Safe**, not to every offered tier. A waiver
overrides the resulting classification to Protected.

### A label is not a deletion guarantee

Cleanup rechecks the resource against a fresh scan, applies the selected tiers and
label filter, and lets Docker refuse removal without force. Protected and
Unattributed resources are never offered. Images held by stopped containers need
those containers removed and another scan before they can become actionable.

Irreversible **volumes** require a vault copy that has been written, re-read, and
verified before removal. Disabling the vault or failing preservation leaves the
volume alone. This preservation guarantee does **not** extend to container
writable layers or unrecoverable images.

The selected reclaim total is an estimate from known resource sizes, plus eligible
build cache when Safe is selected. Unknown sizes are not counted, and Docker can
refuse items during cleanup. Image figures are **exclusive**: an image counts only
the layers no other image holds, so a shared base layer is counted once rather
than once per image standing on it. Sums are therefore a floor rather than an
over-promise — a group whose members share layers with each other frees at least
its total. Docker logical bytes and host-measured physical reclamation are
separate results.

## Vault

The vault stores volume archives on the host, outside Docker. Before removing an
irreversible volume, Prune Juice writes its archive, then re-reads it and verifies
its checksum and archive entry count. If preservation fails, the volume is left
alone. Vault copies remain until you explicitly delete them.

```sh
# List copies and their entry IDs.
prune-juice --vault

# Preserve a volume without deleting it.
prune-juice --vault-dump example_mysql

# Verify all stored copies.
prune-juice --vault-verify

# Restore using an entry ID from the vault list.
prune-juice --vault-restore ENTRY_ID

# Permanently delete a stored copy.
prune-juice --vault-forget ENTRY_ID --reason "Backup replaced and restore tested"
```

Restore recreates the original volume name and labels, refuses to overwrite an
existing volume, and keeps the vault copy. Ensure the original name is available
on the destination daemon before restoring.

**Preserve and restore use the first discovered local Docker context.** The
`--context` scan option does not select the destination for these commands. Check
your current Docker context and any `DOCKER_HOST` override before running them.

`--vault-forget` is permanent and requires a reason of at least **12 characters**.
It can remove your last remaining copy of the data.

`--no-vault` makes cleanup refuse irreversible volumes rather than deleting them
without a backup. The vault does not preserve container writable layers or images.
A verified archive confirms that the copy is readable and intact; it is not a
substitute for an application-consistent database backup while a database is writing.

## Waivers

A waiver marks matching resources Protected until you remove it. It requires a
reason of at least **12 characters**.

```sh
prune-juice --waive volume:example_mysql \
  --reason "Keep this database for the migration"
prune-juice --waivers
prune-juice --unwaive volume:example_mysql
```

Selectors accept a resource kind and name, a bare name matching any kind, or a
trailing `*` for prefix matching. Quote wildcard selectors so your shell does not
expand them:

```sh
prune-juice --waive 'volume:archive_*' \
  --reason "Retain archived project databases"
```

The blanket selector `*` is refused. Waivers are stored locally and are not scoped
to a single Docker context, so the same selector can protect matching resources
on multiple daemons.

## Updates

Both the CLI and the Mac app check for a newer release in the background and
offer it. Neither installs anything without being asked.

### The CLI

A check runs at most once a day, on a terminal only, and never delays or fails
a scan. If the release server cannot be reached the run is unaffected and
nothing is said. When there is something newer, two lines follow the report:

```
  Prune Juice 0.2.0 is available. You have 0.1.0.
  Run prune-juice --update to install.
```

Notices go to stderr, so they stay out of `--json` and out of anything you
pipe. Nothing is printed under `CI`, when output is redirected, or when
`--json` is used.

```sh
# Ask now, whatever the cache says.
prune-juice --check-update

# Install it.
prune-juice --update

# Stop checking automatically, permanently or for one run.
prune-juice --update-check off
prune-juice --no-update-check
```

`--update` downloads the archive for this machine's exact target, checks it
against the SHA-256 in a manifest signed with the project's release key,
confirms the new binary runs and reports the expected version, and only then
replaces the executable with a single atomic rename. Any failure leaves the
working copy exactly as it was.

**Installations a package manager owns are never overwritten.** The check
still reports the new version, but the second line becomes the command that
manager understands:

| How it was installed | What you are told to run |
| --- | --- |
| A downloaded binary | `prune-juice --update` |
| Homebrew | `brew upgrade prune-juice` |
| `cargo install` | `cargo install prune-juice-cli --force` |
| MacPorts | `sudo port upgrade prune-juice` |
| Nix | `nix profile upgrade prune-juice` |
| Inside `PruneJuice.app` | nothing — the app updates it |

A build with no release key compiled in — which includes a plain `cargo build`
from this repository — has no update system at all and says so, rather than
reporting a version it cannot verify.

### The Mac app

The app checks on launch, at most once a day, and uses
[Sparkle](https://sparkle-project.org) for the download, the release notes,
the signature check, the install and the relaunch.

It will not interrupt you. If an update is found while a scan, cleanup or
vault operation is running, or while the app is in the background, a line
appears in the window and a badge on the Dock icon instead of an alert. The
install itself waits until the operation has finished, so an update can never
relaunch the app between two deletions. **Check for Updates…** in the Prune
Juice menu asks immediately, and **Settings → Updates** turns automatic
checking off.

An update replaces the whole app bundle, including the CLI helper inside it,
so the two are always the same version.

Because the app is not yet signed with an Apple Developer ID, the *first*
install of an updater-enabled build has to be opened once through right-click
→ **Open** to clear Gatekeeper. Updates after that are verified by their EdDSA
signature and install without ceremony.

## Docker contexts and project folders

The Mac app and one-shot CLI scan discovered local Docker contexts by default.
Contexts that refer to the same daemon are counted once. The interactive terminal
uses the first local context.

Remote TCP and SSH contexts are excluded, and each exclusion is named in the run
output rather than passed over in silence. To include one, pass `--remote` with a
`--reason` of at least 12 characters, the same rule a waiver follows: a remote
engine's disk is not this machine's disk, its projects are not the directories
being searched here, and host reclamation cannot be measured at all. Vault
preserve and restore remain local-only regardless of `--remote`.

```sh
# Inspect one named context.
prune-juice --no-tui --context default

# Search custom project folders.
prune-juice --no-tui --roots "$HOME/Projects:$HOME/work"
```

By default, Prune Juice looks for existing folders named `Documents/Repos`, `Repos`,
`repos`, `dev`, `code`, `src`, `Sites`, `Projects`, and `work` under your home folder.
`--roots` replaces that list with colon-separated paths. Use absolute paths.

Docker configuration is read from `DOCKER_CONFIG`, or `~/.docker` by default.
A nonempty `DOCKER_HOST` overrides context discovery and names that connection
`DOCKER_HOST`. For example, select it with `--context DOCKER_HOST` rather than a
stored context name.

Ownership information comes from project labels, declarations, names, and locally
recorded container-to-volume relationships. Scanning before removing stopped
containers helps preserve ownership information that Docker otherwise loses.
Historical or name-only matches do not by themselves establish that a project
has been deleted. A project does not need a `.git` directory to be considered present.

### Volume inspection

Where possible, Prune Juice reads volume contents directly from the host. For
Docker Desktop and other setups where the host cannot access them, it uses a
temporary container with read-only volume mounts, a read-only root filesystem,
and no network access.

The probe uses an image already present locally; it never downloads one itself.
If it reports that no suitable image is available, you can provide one explicitly:

```sh
docker pull alpine
prune-juice --no-tui
```

`--container-probe` always uses container inspection. `--no-probe` skips content
inspection, so no volume can qualify as Safe on the basis of its contents.

## Command reference

| Option | Meaning |
| --- | --- |
| `--apply` | Perform cleanup. Defaults to the Safe tier when no tiers are specified. |
| `--tiers LIST` | Comma-separated selection: `free`, `repullable`, `rebuildable`, `orphan`, `stale`. Replaces the default. |
| `--only-label K=V` | Refuse removal of resources without that exact label. Excludes all build cache because cache records have no labels. |
| `--no-tui` | Use the one-shot report instead of the interactive terminal. |
| `--json` | Emit newline-delimited JSON scan and cleanup events. Vault and waiver commands still produce text. |
| `--no-sizes` | Skip volume sizing and build-cache enumeration. Also excludes build cache from cleanup, and leaves every container outside Safe because its writable layer cannot be proven empty. Volume sizes measured by an earlier run are reported as remembered figures, for volumes nothing is mounting. |
| `--roots PATHS` | Colon-separated project search directories. |
| `--context NAME` | Select a local context for scanning and cleanup. Does not select the context for vault preserve or restore. |
| `--remote` | Include a context that is not a local socket (`tcp://`, `ssh://`). Requires `--reason`. |
| `--deadline SECS` | Scan deadline, default 120 seconds. Use `0` to wait indefinitely. |
| `--no-probe` | Skip volume content inspection. Uninspected volumes cannot be Safe. |
| `--container-probe` | Always inspect volume contents through a temporary container. |
| `--no-vault` | Refuse irreversible volumes instead of preserving them for cleanup. |
| `--vault` | List preserved copies and exit. |
| `--vault-dump VOL` | Preserve a volume without deleting it. |
| `--vault-verify` | Verify all preserved copies. |
| `--vault-restore ID` | Restore a preserved volume. |
| `--vault-forget ID` | Permanently remove a preserved copy. Requires `--reason`. |
| `--waivers` | List waivers and exit. |
| `--waive SELECTOR` | Protect matching resources. Requires `--reason`. |
| `--unwaive SELECTOR` | Remove a waiver. |
| `--reason TEXT` | Reason for adding a waiver, deleting a vault copy, or allowing `--remote`; at least 12 characters. |
| `--check-update` | Ask now whether a newer release exists. Exits 0 up to date, 1 update available, 5 could not check. |
| `--update` | Install the newest release. Refused when a package manager owns the binary, which then names the command to run instead. |
| `--update-check on\|off` | Turn the automatic once-a-day check on or off and remember the answer. |
| `--no-update-check` | Skip the automatic check for this run. `PRUNE_JUICE_NO_UPDATE_CHECK=1` does the same. |
| `-V`, `--version` | Print the version. |
| `-h`, `--help` | Show help. |

Use one vault or waiver operation per invocation. These commands run independently
of the normal scan and cleanup flow.

## Scripting and exit codes

Piping or redirecting output, setting `CI`, or passing `--no-tui`, `--json`, or
`--apply` disables the interactive terminal. Human progress goes to stderr;
reports and JSON events go to stdout.

```sh
# Save a report without deleting anything.
prune-juice --no-tui > report.txt

# Show Safe resources and their known sizes from streaming JSON events.
# `exclusive_size` is the field to add up; `size` is the whole layer stack.
prune-juice --json | jq -r '
  select(.event == "classified" and .tier == "free")
  | [.kind, .name, (.exclusive_size // .size // "unknown")] | @tsv'

# Preview cleanup restricted to a project's label.
prune-juice --no-tui --only-label com.docker.compose.project=example
```

| Exit code | Meaning |
| --- | --- |
| `0` | Successful completion without reported findings requiring attention. |
| `1` | Reclaimable space or other scan findings. This is not a process failure. |
| `2` | Invalid arguments or configuration, including invalid waiver reasons. |
| `3` | Docker daemon unavailable. |
| `4` | Permission denied. |
| `5` | Partial failure: an operation was refused or failed. |
| `130` | Cancelled. |

Handle exit `1` explicitly in scripts that use `set -e`. Cancellation returns
`130`, from the interactive scan and from `Ctrl-C` or `SIGTERM` during a one-shot
run. An interrupted cleanup stops at an item boundary, never inside one, then
prints the receipt for what it did and skips the build cache; the host
measurement is reported as unavailable rather than waited for. A second `Ctrl-C`
exits immediately without that report.

## Local data

| Data | macOS default | Linux default |
| --- | --- | --- |
| Ownership history | `~/Library/Application Support/prune-juice/index.db` | `~/.local/share/prune-juice/index.db` |
| Preserved volumes | `~/Library/Application Support/prune-juice/vault/` | `~/.local/share/prune-juice/vault/` |
| Waivers | `~/.config/prune-juice/waivers.json` | `~/.config/prune-juice/waivers.json` |
| Update preference | `~/.config/prune-juice/config.json` | `~/.config/prune-juice/config.json` |
| Last update check | `~/.cache/prune-juice/update-check.json` | `~/.cache/prune-juice/update-check.json` |

`XDG_DATA_HOME` overrides the base folder for ownership history and vault storage.
`XDG_CONFIG_HOME` overrides the base folder for waivers and the update
preference; `XDG_CACHE_HOME` overrides it for the update check. The Mac app
writes diagnostics to `~/Library/Logs/prune-juice-app.log`.

Deleting the update check costs one extra request. Deleting the update
preference turns automatic checking back on, which is why the two are kept
apart: one is a cache and the other is a decision you made.

Deleting ownership history loses recorded associations and observations used to
confirm missing projects. Deleting the vault loses preserved data. Removing the
app or CLI does not automatically remove these files.

## Troubleshooting

**Docker cannot be reached.** Start your Docker runtime, confirm the selected
context is local, and check for an unexpected `DOCKER_HOST` override.

**Docker reports permission denied.** Check your user's access to the Docker
socket. Starting an already running daemon does not resolve socket permissions.

**An image is Protected even though nothing is running.** Stopped containers also
hold images. Review and reclaim eligible containers, then scan again.

**A volume is not offered.** Read its reason and evidence. It may still be declared
by an existing project, have unreadable or non-reconstructible contents, lack
sufficient ownership evidence, or need another observation to confirm an absent
project. Check that your project folders are included in the scan.

**A scan seems slow or incomplete.** Volume sizing is the slow part, and it runs
alongside the rest of the scan rather than in front of it. Activity shows the
current phase in the Mac app. The default scan deadline is 120 seconds; use
`--deadline 30` for a shorter scan. `--no-sizes` skips sizing, but also hides and
excludes build cache and keeps containers out of Safe. Missing measurements are
not proof that no space is reclaimable.

**The host gained less space than expected.** VM-backed runtimes may not
immediately return freed blocks to the host, and the selection total is an
estimate — a floor, since images share layers and each image is counted only for
the layers it alone holds. Read Docker-reported and host-measured results
separately; an unavailable host measurement is shown as unavailable.

**A vault restore is refused.** Check the entry ID, archive verification result,
and destination context. Restore will not overwrite an existing volume with the
same name.

**The Mac app cannot find its helper.** Rebuild or replace the complete app bundle.
The app does not fall back to a separate CLI installation on your `PATH`.

## License

[MIT](LICENSE).
