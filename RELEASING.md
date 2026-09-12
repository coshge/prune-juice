# Releasing Prune Juice

The CLI and Mac app are versioned and released independently from this repo.
`.github/workflows/release.yml` selects one product from its tag:

- `cli-vX.Y.Z`: release the Rust CLI for macOS and Linux.
- `app-vX.Y.Z`: release the Mac app with its pinned CLI helper.

The Rust crates share the workspace version in `Cargo.toml`. The app uses
`app/PruneJuice/release.json`: `version` is the app version, `cliVersion` is
the bundled helper version, and `cliRevision` is its full Git commit SHA.
An app-only release does not require a new CLI release. App releases build
the helper from that immutable source revision with its committed Cargo lockfile.

There is one-time setup before the first release, and it is the part that
matters: **the signing keys are what make an update safe.** Without them the
pipeline refuses to publish, on purpose — a release nobody can verify is worse
than no release, because the only thing it can do is teach people to install
unverified binaries.

## Contents

- [What a release contains](#what-a-release-contains)
- [One-time setup](#one-time-setup)
- [Cutting a release](#cutting-a-release)
- [What updates look like to a user](#what-updates-look-like-to-a-user)
- [Verifying a release by hand](#verifying-a-release-by-hand)
- [If the repository is ever private again](#if-the-repository-is-ever-private-again)
- [The Developer ID gap](#the-developer-id-gap)
- [Re-releasing and rolling back](#re-releasing-and-rolling-back)

## What a release contains

| Asset | Read by | Signed with |
| --- | --- | --- |
| `prune-juice-<version>-<target>.tar.gz` | `prune-juice --update` | its SHA-256 in the signed manifest |
| `update-manifest.json` | the CLI's update checker | minisign (`.minisig` beside it) |
| `update-manifest.json.minisig` | the CLI's update checker | — |
| `PruneJuice-<version>.zip` | Sparkle | Sparkle EdDSA, in the appcast |
| `appcast.xml` | Sparkle | Sparkle EdDSA per item |

CLI releases contain the four CLI archives and signed manifest. App releases
contain the app archive and Sparkle appcast. The app archive includes its tested
helper; its version can differ from the app version.

The dedicated `updates` release holds both feeds:

```
https://github.com/coshge/prune-juice/releases/download/updates/update-manifest.json
https://github.com/coshge/prune-juice/releases/download/updates/appcast.xml
```

Product releases are published with `--latest=false`. The `updates` release is
marked Latest so existing installations using `/releases/latest/download/`
continue to find both feeds. Each release changes only its own product's feed.
The first independent release copies both feeds from the previous combined
release before promoting `updates`. Missing legacy feeds stop publication.
This migration assumes the existing combined release has both feeds; it is
not a fresh-repository bootstrap procedure.

Do not manually mark a product release Latest or delete `updates`. Installer
downloads are on the versioned `cli-v*` and `app-v*` releases; feed entries link
to those immutable assets. Signing secrets are unchanged.

## One-time setup

### 1. The CLI signing key (minisign)

```sh
brew install minisign
minisign -G -p prune-juice.pub -s prune-juice.key
cat prune-juice.pub    # two lines; the second is the key
```

Then, in the repository settings under **Secrets and variables → Actions**:

| Secret | Value |
| --- | --- |
| `MINISIGN_PUBLIC_KEY` | the second line of `prune-juice.pub` — the key alone |
| `MINISIGN_SECRET_KEY` | `prune-juice.key`: the whole file, or its second line |
| `MINISIGN_PASSWORD` | the passphrase you chose |

Either shape works for the secret key, because the workflow rebuilds the
two-line file minisign wants from whichever arrived. That is not fussiness:
a minisign key file is an `untrusted comment:` line plus the key, and a paste
of the key alone fails with "Error while loading the secret key file", which
says nothing about the shape being the problem. The comment line carries no
information — minisign never reads it back.

The public key also belongs in `RELEASE_KEY` in
`crates/prune-juice-core/src/update/verify.rs`, and **is already there** — that
is what gives a `cargo install` from source the same update checking a
CI-built binary has. It is public by construction: every binary carries it, and
it is the thing installed copies verify *against*.

What is *not* defensible is losing the secret half, because every installed
copy verifies against the public half and cannot be told to trust a new one.
**Back up `prune-juice.key` and its passphrase somewhere you would trust with a
password**, and keep them out of the working tree — `.gitignore` covers
`*.key` and `*.pub`, which stops `git add .` and not `git add -f`.

### 2. The app signing key (Sparkle EdDSA)

```sh
cd app/PruneJuice
swift package resolve
BIN="$(find .build/artifacts -type f -name generate_keys | head -1)"
"$BIN"                       # generates, and prints the public key
"$BIN" -x sparkle.key        # exports the private key for CI
```

| Secret | Value |
| --- | --- |
| `SPARKLE_PUBLIC_KEY` | the public key `generate_keys` printed |
| `SPARKLE_PRIVATE_KEY` | the contents of the exported `sparkle.key` |

The private key lives in your login keychain as well as in that export. Same
warning as above: this key is the only thing standing between an app install
and an attacker-supplied "update", and it cannot be rotated for copies already
in the wild.

Delete `prune-juice.key` and `sparkle.key` from disk once they are in the
secrets. Neither belongs in the repository, and `.gitignore` does not protect
you from `git add -f`.

### 3. Optional, and absent today: a Developer ID

See [The Developer ID gap](#the-developer-id-gap). When one exists, add
`MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`, `KEYCHAIN_PASSWORD`,
`MACOS_SIGN_ID`, `NOTARY_APPLE_ID`, `NOTARY_TEAM_ID` and `NOTARY_PASSWORD`.
The workflow picks them up and starts signing and notarising; nothing else
changes.

## Cutting a release

### CLI

Bump `[workspace.package].version` in `Cargo.toml`, then run:

```sh
cargo check --workspace                 # refresh Cargo.lock
cargo test --workspace
python3 scripts/release.py validate cli-v0.3.5
```

After committing the version and intended CLI changes, push the exact tag:

```sh
git tag cli-v0.3.5
git push origin cli-v0.3.5
```

The workflow checks the tag, tests Rust, builds four targets, signs the manifest,
publishes the CLI release, and updates only the CLI feed. It then checks that a
published macOS binary can read and verify the live feed. It does not build or
release the Swift app.

### Mac app

Bump only `version` in `app/PruneJuice/release.json`, for example to `0.3.5`.
Leave `cliVersion` and `cliRevision` unchanged for an interface-only update.
To include an engine change, set both helper fields to the desired CLI version
and the full commit SHA containing it (`git rev-parse cli-v0.3.5^{commit}` for
a local release tag). Commit SHAs must exist on the remote before app CI runs.

```sh
python3 scripts/release.py validate app-v0.3.5
swift test --package-path app/PruneJuice
```

After committing the app version and changes:

```sh
git tag app-v0.3.5
git push origin app-v0.3.5
```

The workflow checks out the pinned helper separately, runs its Rust tests,
builds both Mac architectures, checks the CLI and protocol versions, and runs
Swift tests with the pinned helper's argument parser. It then bundles, signs,
optionally notarises, and publishes only the app and its appcast. Full Docker
cleanup acceptance tests remain a separate check for engine changes; the
compatibility gate is not a live Docker test.

Before publishing an appcast, CI verifies the downloaded archive's signature
against the public key inside that app. Missing signatures and incorrect archive
sizes stop publication. The app artifact has an explicit download directory so
its signature file is found even when it is the workflow's only artifact.

App builds require Rust because they compile the source pin. They do not depend
on another release job or download a floating latest CLI. Local `bundle.sh`
and `install-local.sh` keep building the current working tree for development;
the app version still comes from `release.json`. CI supplies the pinned helper
and sets `REQUIRE_PINNED_HELPER=1`, which requires both architectures and checks
the bundled version. `PruneJuiceCLIVersion` in Info.plist records the actual helper.

### Publication rules

Use a version higher than the currently distributed version of that product.
Bump only the product you want to release next.
Only stable `major.minor.patch` versions are accepted. Legacy `v*` tags no longer
trigger this workflow.

Publication is serialized to protect the shared update release. GitHub allows
one pending run in this concurrency group; additional queued releases can
replace that pending run. Push releases one at a time, or rerun a canceled tag
through the manual workflow trigger.

The manual trigger accepts an existing product tag. Published assets are
immutable: a retry with different rebuilt or signed bytes fails and requires a
new version. Failed draft uploads can be retried. Feed downloads fail closed,
and an older version is never promoted over a newer feed. A CLI manifest and
its detached signature are separate assets, so a check during their replacement
can briefly fail verification; clients retry through their normal update flow.

## What updates look like to a user

**The CLI** checks at most once a day, only on a terminal, never under `CI`,
and never in `--json`. It never fails a scan — an unreachable release server
produces silence — and it goes *before* the scan, not after the report: a
release worth installing is worth hearing about instead of scanning. The wait
is bounded at six seconds and a remembered answer returns instantly, so only
the once-a-day refresh can hold anything up. Two lines, on stderr:

```
  Prune Juice 0.2.0 is available. You have 0.1.0.
  Run prune-juice --update to install.
```

The second line changes with how that copy was installed: a Homebrew install
is told `brew upgrade prune-juice`, a `cargo install` is told `cargo install
prune-juice-cli --force`, and the helper inside `Prune Juice.app` is told
nothing to run, because the app updates it. `--update` refuses to overwrite a
binary a package manager owns.

A copy this tool *may* replace — one someone downloaded and put on their PATH
— is asked rather than told:

```
  Prune Juice 0.2.0 is available. You have 0.1.0.
  Update now? [Y/n]
```

Return takes it. `n` or `no` declines and the scan carries on, as does any
answer that is not a yes. The single exception to the default is EOF — a read
of zero bytes, meaning Ctrl-D or a stdin that went away — because a question
nobody answered is not the same as a question answered by pressing return.

Answering yes installs exactly what `--update` installs: verified against the
signed manifest, and run once before it is moved into place. It then
re-executes the new binary with the same arguments, so the run that was
actually asked for continues on the new version rather than dropping the user
back at a shell prompt. The question is never asked when stdin is not a
terminal, and never twice in one chain — the re-executed process carries
`PRUNE_JUICE_UPDATED` and does not offer again.

**The app** checks on launch, before its first scan — a scan makes it busy,
and a background check arriving during one is declined, so the check has to go
first to happen at all. The first scan starts when that check finishes, or six
seconds later if the feed does not answer; if an update is found the scan
waits for the dialog rather than starting behind it. If one is found *later*,
while a scan, cleanup or vault operation is running — or while the app is in
the background — it does not interrupt: a line appears in the window and a
badge on the Dock icon, and the update is presented when the user asks for it.
Installing is postponed until the helper has stopped, so an update can never
relaunch the app between two deletions.

## Verifying a release by hand

Everything a user's copy checks, you can check yourself:

```sh
V=0.2.0
BASE=https://github.com/coshge/prune-juice/releases/download/cli-v$V

# 1. The manifest is signed by the release key.
curl -fsSLO $BASE/update-manifest.json
curl -fsSLO $BASE/update-manifest.json.minisig
minisign -Vm update-manifest.json -P "$(cat prune-juice.pub | tail -1)"

# 2. Every artifact matches the digest the signed manifest names.
curl -fsSLO $BASE/prune-juice-$V-aarch64-apple-darwin.tar.gz
shasum -a 256 prune-juice-$V-aarch64-apple-darwin.tar.gz
grep -A2 aarch64-apple-darwin update-manifest.json

# 3. An installed copy agrees.
prune-juice --check-update      # 0 up to date, 1 update available, 5 could not check
```

For the app, `sign_update --verify` checks an archive against the signature in
the appcast.

## Re-releasing and rolling back

An unpublished draft can be retried. Once published, a version's artifacts
cannot change: an identical retry is accepted, but a rebuild producing different
bytes needs a new version. See [Publication rules](#publication-rules).

Rolling *back* is not something the update system can do — Sparkle and the CLI
both refuse to move to an older version, which is the correct behaviour and not
worth defeating. To withdraw a bad release, publish a higher version that fixes
it. Deleting the release is not enough on its own: copies that have already
cached the answer will keep offering it for up to a day, though they will fail
to download and leave the working binary alone.

## If the repository is ever private again

Release assets on a private repository are not anonymously downloadable: an
unauthenticated request for one returns **404**, not 403. The updater fetches
through `curl` with no credentials by design, so were this repository made
private again:

- every `--check-update` and every background check would resolve to "no
  news", and exit 5 rather than 0 or 1;
- `release.yml`'s final step — "an installed copy can see this release" —
  would fail, and correctly: it asserts the one property a private repository
  denies. The release itself is still built, signed and published; only that
  assertion fails.

Nothing in the code would need to change either way. The repository is public,
and `v0.1.0` — published while it was not — became reachable the moment it
was made so.

## The Developer ID gap

There is no Apple Developer ID for this project yet, which has two
consequences and one non-consequence.

**Updates are still verified.** Sparkle accepts either a Developer ID code
signature or an EdDSA signature on the archive as proof that an update is
genuine, and EdDSA is the documented path for apps distributed outside the App
Store without one. `SUPublicEDKey` is set in the bundle, and with it absent
Sparkle is not started at all — no key, no update mechanism, rather than an
update mechanism that trusts whatever it is handed.

**The first install needs approval.** An ad-hoc signed app downloaded from the
internet is quarantined, so the first install has to be opened once through
right-click → Open, or have the quarantine attribute removed. Every update
after that is silent, because Sparkle unpacks and installs the new bundle
itself once it has verified the signature.

**Existing users need one manual install.** A copy of the app built before
this pipeline has no Sparkle in it and cannot be offered anything. Everyone on
an older build installs one updater-enabled version by hand; from there
updates are offered automatically.

**An install from before 0.3.2 keeps the old file name.** The bundle is now
`Prune Juice.app` — the name on disk is what Finder, the Dock and the app
switcher display, `CFBundleDisplayName` being consulted only for a bundle
carrying a localized `InfoPlist.strings`, which a hand-assembled one does
not. Sparkle installs an update at the *host* bundle's path, so a copy
already installed as `PruneJuice.app` stays spelled that way however many
times it updates. Everything inside it reads "Prune Juice"; only the file
name lags, and renaming it in Finder is the whole fix. Not worth breaking an
install over, and worth knowing before it looks like a bug.

Getting a Developer ID changes only the first of those, and the workflow is
already written to pick the certificate up when the secrets appear.
