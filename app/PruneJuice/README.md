# Prune Juice for Mac

Inspect Docker resources, review cleanup costs, and reclaim space from a native
Mac app. Requires macOS 14 or later and a running local Docker runtime.

For installation, classification rules, vault behavior, and troubleshooting, see
the [main user guide](../../README.md).

## Build and open

With Rust and a Swift 6 toolchain installed, run from this directory:

```sh
./scripts/bundle.sh
open "dist/Prune Juice.app"
```

To rebuild the app *and* update the `prune-juice` on your `PATH` from the same
build, use `./scripts/install-local.sh --open` from the repository root. A
build from source never updates itself, so that script is how a local install
keeps up with the tree.

The icon is generated, not stored: `scripts/make-icon.swift` draws the app's
own half-full droplet in `plum` from `Views.swift` — redrawn rather than taken
from SF Symbols, whose licence does not cover app icons — and `bundle.sh`
turns the result into `PruneJuice.icns`. The CLI draws the same droplet in
half-block glyphs, rasterised from the same curve, from `core::brand`. To
look at the icon without building the whole app:

```sh
swift scripts/make-icon.swift /tmp/PruneJuice.iconset
open /tmp/PruneJuice.iconset
```

The app includes its command-line helper. The default build is signed for local
use, not notarized for redistribution. Move the complete `Prune Juice.app` bundle
to Applications if you want to keep it there.

A build with no `SPARKLE_PUBLIC_KEY` in its environment has no update
mechanism, which is deliberate — an update that cannot be verified is not a
lesser update. The bundle script says so when it happens, and the app leaves
the update controls out rather than showing buttons that do nothing:

```sh
SPARKLE_PUBLIC_KEY="…" ./scripts/bundle.sh   # updates enabled
```

See [RELEASING.md](../../RELEASING.md) for generating that key and for how a
release is cut.

## Scan and review

The app scans on launch. Use **Scan again** or **Command-R** to refresh.

| Screen | What you can do |
| --- | --- |
| Resources | Search resources, filter classifications, and select a row to read its reason and ownership evidence. |
| Reclaim | Select cleanup tiers, see the combined estimate, preview cleanup, and review costs before confirming. |
| Vault | List, verify, preserve, restore, or permanently delete volume copies. |
| Waivers | List, add, or remove exclusions that protect matching resources. |
| Activity | Follow progress, read results and notices, and copy output. |
| Settings | Set the Docker context, project folders, label filter, deadline, inspection options, vault preservation, menu bar icon, and automatic update checks. |

See [resource labels and their exact criteria](../../README.md#resource-labels-and-their-exact-criteria)
for the rules behind Protected, Safe, Pull again, Build again, Orphaned, Dormant,
and Unattributed.

## Reclaim space

Select the tiers you want on **Reclaim**. The estimate updates with your selection;
eligible build cache is included only when Safe is checked. **Preview cleanup**
does not delete resources. **Review cleanup** shows the costs and scope before
you confirm removal.

Every size the app adds up is exclusive: an image counts only the layers no
other image holds, and the Images metric shows what the layers occupy rather
than the sum of stack sizes. Fifteen project images on one base layer occupy
that base once, not fifteen times, so the estimate is a floor and never a
promise the cleanup cannot keep.

Cleanup applies to whole selected tiers, including newly eligible resources found
during its fresh scan. Selecting a row in Resources only opens its details.
Changing settings requires another scan before cleanup. A new scan resets the
selection to Safe. Scan again after cleanup to refresh the resource list.

## Preserve and protect resources

Vault and waiver listings appear in **Activity**. Copy an entry ID or selector
into its action form. Deleting a preserved copy and adding a waiver both require
a reason of at least 12 characters.

Vault preserve and restore use the first discovered local Docker context,
independently of the scan context setting. Restore keeps the archive and refuses
to overwrite an existing volume. Disabling vault preservation causes cleanup to
refuse irreversible volumes rather than remove them without a copy.

## Updates

The app checks for a newer release on launch, at most once a day, using
[Sparkle](https://sparkle-project.org). It never interrupts work to do it:

- A scheduled check is declined outright while a scan, cleanup or vault
  operation is running.
- An update found while the app is busy or in the background puts a line in
  the window and a badge on the Dock icon rather than an alert. **Show update**
  presents it.
- Installing waits until the helper has stopped. An update can never relaunch
  the app between two deletions.
- An update replaces the whole bundle, including the CLI helper, so the two are
  always the same version.

**Check for Updates…** in the Prune Juice menu asks immediately, and is allowed
even mid-operation — only installing waits. **Settings → Updates** turns
automatic checking off and shows when the last check ran. A failed check is
written to the diagnostics log and never shown in the interface.

The first install of an updater-enabled build needs right-click → **Open**,
because the app is not yet signed with an Apple Developer ID. Updates after
that verify against the EdDSA signature in the feed and install without
ceremony.

Allow active operations to finish before quitting. If the app reports a helper
error, rebuild or replace the complete app bundle. Diagnostics are written to
`~/Library/Logs/prune-juice-app.log`.
