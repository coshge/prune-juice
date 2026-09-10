# Prune Juice for Mac

Inspect Docker resources, review cleanup costs, and reclaim space from a native
Mac app. Requires macOS 14 or later and a running local Docker runtime.

For installation, classification rules, vault behavior, and troubleshooting, see
the [main user guide](../../README.md).

## Build and open

With Rust and a Swift 6 toolchain installed, run from this directory:

```sh
./scripts/bundle.sh
open dist/PruneJuice.app
```

The app includes its command-line helper. The default build is signed for local
use, not notarized for redistribution. Move the complete `PruneJuice.app` bundle
to Applications if you want to keep it there.

## Scan and review

The app scans on launch. Use **Scan again** or **Command-R** to refresh.

| Screen | What you can do |
| --- | --- |
| Resources | Search resources, filter classifications, and select a row to read its reason and ownership evidence. |
| Reclaim | Select cleanup tiers, see the combined estimate, preview cleanup, and review costs before confirming. |
| Vault | List, verify, preserve, restore, or permanently delete volume copies. |
| Waivers | List, add, or remove exclusions that protect matching resources. |
| Activity | Follow progress, read results and notices, and copy output. |
| Settings | Set the Docker context, project folders, label filter, deadline, inspection options, vault preservation, and menu bar icon. |

See [resource labels and their exact criteria](../../README.md#resource-labels-and-their-exact-criteria)
for the rules behind Protected, Safe, Pull again, Build again, Orphaned, Dormant,
and Unattributed.

## Reclaim space

Select the tiers you want on **Reclaim**. The estimate updates with your selection;
eligible build cache is included only when Safe is checked. **Preview cleanup**
does not delete resources. **Review cleanup** shows the costs and scope before
you confirm removal.

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

Allow active operations to finish before quitting. If the app reports a helper
error, rebuild or replace the complete app bundle. Diagnostics are written to
`~/Library/Logs/prune-juice-app.log`.
