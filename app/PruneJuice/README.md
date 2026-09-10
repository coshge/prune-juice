# Prune Juice for Mac

A native AppKit window hosting SwiftUI, with system typography, translucent sidebar,
plum accents, and automatic light and dark appearance. Requires macOS 14 or later.

Build the runnable app from this directory:

```sh
./scripts/bundle.sh
open dist/PruneJuice.app
```

See [resource labels and their exact criteria](../../README.md#resource-labels-and-their-exact-criteria)
for the rules behind Protected, Safe, Pull again, Build again, Orphaned, Dormant,
and Unattributed.

The app uses only its bundled CLI helper. It opens with a scan. Cleanup is an
explicit action from the Reclaim screen, with the selected tier costs and context
scope shown before confirmation. Selections return to the safe tier on a new scan.

| Screen | CLI functionality |
| --- | --- |
| Resources | Scan all classifications, search by name/project/kind/context, inspect reasons and provenance |
| Reclaim | Preview selected tiers without mutation, apply all five offered tiers, show preservation and recovery costs |
| Vault | List, verify, preserve a volume, restore an entry, delete a preserved copy with a reason |
| Waivers | List, add with a reason, remove a selector |
| Activity | Streaming progress, warnings, copyable command output, separate logical and physical reclamation |
| Settings | Context, project roots, label fence, deadline, sizes, content inspection, container inspection, vault toggle, CLI help |

Vault and waiver listings use the CLI's text output in Activity. Entry IDs and
selectors can be copied into their action forms. Vault preserve and restore use
the first discovered local context, matching the current CLI. Cleanup applies to
whole tiers, including newly eligible resources found in its fresh scan. The
resource browser does not imply individual deletion selection.

Every subprocess drains stdout and stderr concurrently. App actions are serialized,
cleanup requires a successful scan with matching settings, and operations that
change resources invalidate the displayed scan. Quit waits for an operation to
finish. The engine retains all deletion, revalidation, and verified-backup rules.
A disabled vault refuses irreversible items. App copy and displayed CLI messages
are normalized to avoid em dashes.

Verification:

```sh
swift test
PJ_SCREENSHOTS=/tmp/prune-juice-ui swift test
```

Tests exercise argument boundaries, duplicate-action prevention, scan invalidation,
context identity, repeated scan totals, label cache fencing, incomplete results,
protocol compatibility, and native rendering. The screenshot test uses fixture
resources and invokes no Docker operations. Screenshots are written outside the
repository. Live destructive workflows must be tested on disposable resources.
