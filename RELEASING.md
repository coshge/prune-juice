# Releasing Prune Juice

A release is `git tag v0.2.0 && git push origin v0.2.0`. Everything else is
derived from the tag by `.github/workflows/release.yml`.

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

Both documents describe the same release and are built from the same
artifacts. That is what keeps the app and the CLI helper inside it on the same
version: an app update replaces the whole bundle, helper included.

Two fixed URLs, and neither needs any hosting to be set up:

```
https://github.com/coshge/prune-juice/releases/latest/download/update-manifest.json
https://github.com/coshge/prune-juice/releases/latest/download/appcast.xml
```

`/releases/latest/download/<asset>` always redirects to the newest release, so
there is no feed to keep in sync by hand and no GitHub API rate limit in the
path.

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

```sh
# 1. Bump the workspace version. Everything reads it from here.
$EDITOR Cargo.toml          # [workspace.package] version = "0.2.0"
cargo check --workspace     # refreshes Cargo.lock, which --locked needs
git commit -am "chore: 0.2.0"

# 2. Tag and push.
git tag v0.2.0
git push origin production --tags
```

The workflow then:

1. **verify** — clippy, the Rust tests, the Swift tests, and a check that the
   tag matches the workspace version. A mismatch fails here: the updater
   compares against the version compiled into the binary, so a release tagged
   `v0.2.0` built from `0.1.0` sources would be a release nobody is ever
   offered.
2. **cli** — four targets (Apple silicon and Intel macOS, x86-64 and arm64
   Linux), each with the minisign public key compiled in.
3. **app** — the bundle, its Sparkle framework, the CLI helper inside it, and
   the update archive, signed with the EdDSA key.
4. **publish** — generates and signs `update-manifest.json`, merges the new
   item into `appcast.xml`, uploads everything, and then **checks that a
   freshly built binary can read and verify the release it just published**.
   That last step is the one worth watching: it is the only thing that
   exercises the real URL, over the real network, with real verification.

To re-run against an existing tag, use the workflow's manual trigger and give
it the tag name.

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
prune-juice-cli --force`, and the helper inside `PruneJuice.app` is told
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
BASE=https://github.com/coshge/prune-juice/releases/download/v$V

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

Re-releasing the same version is safe: the appcast generator replaces an
existing item for that version rather than adding a second one, and the release
upload uses `--clobber`.

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

Getting a Developer ID changes only the first of those, and the workflow is
already written to pick the certificate up when the secrets appear.
