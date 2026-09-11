//! Finding out whether a newer Prune Juice exists, and installing it.
//!
//! Five rules shape everything here. They are in priority order, and where
//! they conflict the earlier one wins.
//!
//! 1. **A scan is never delayed by an update check.** The cache is read
//!    synchronously because that is a file read; the network is only ever
//!    touched on a background thread whose result is used if it arrives and
//!    dropped if it does not. A check that misses this run lands in the cache
//!    and is reported by the next one.
//! 2. **The update server is never load-bearing.** Every failure — no network,
//!    no `curl`, a 404, a truncated body, a bad signature — resolves to "no
//!    news", never to a non-zero exit or a missing report. Reclaiming disk
//!    must not depend on GitHub being up.
//! 3. **Nothing is believed unsigned.** The manifest carries the version and
//!    the digests, and it is checked against a compiled-in Ed25519 key before
//!    a single field of it is read. With no key in the build there is no
//!    update system at all, which is stated rather than silently degraded.
//! 4. **Whoever installed it owns updating it.** See [`origin`].
//! 5. **Notices are for humans.** They go to stderr, only on a terminal, never
//!    in `--json`, never under `CI`. The gate itself lives in the CLI, because
//!    this crate does not look at terminals.

pub mod install;
pub mod net;
pub mod origin;
pub mod verify;

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use net::Fetcher;
pub use origin::Origin;

/// The target triple this binary was built for, from Cargo via `build.rs`.
/// Guessing it from `std::env::consts` cannot tell gnu from musl, and picking
/// the wrong artifact is a download that will not run.
pub const HOST_TARGET: &str = env!("PRUNE_JUICE_TARGET");

/// The manifest format this build understands. A release that bumps this is
/// telling older copies to stand aside rather than misread it.
pub const SCHEMA: u32 = 1;

/// The default feed: a release asset served from a fixed URL that always
/// resolves to the newest release.
///
/// Deliberately not the GitHub API. `/releases/latest/download/<asset>` is a
/// plain redirect to a file we published, so it has no rate limit to be
/// throttled by, no JSON shape that can change under us, and no dependence on
/// GitHub's API remaining free to unauthenticated callers.
pub const MANIFEST_ASSET: &str = "update-manifest.json";

/// How long a successful answer is trusted. Once a day, as asked.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a *failed* answer is trusted. Much shorter: a five-minute network
/// outage should not blind the tool for a day, and retrying every single run
/// would hammer a server that is already having a bad time.
pub const FAILURE_TTL: Duration = Duration::from_secs(60 * 60);

/// Short enough that nobody notices it, and it runs on another thread anyway.
pub const MANIFEST_TIMEOUT: Duration = Duration::from_secs(5);
/// An artifact download is an explicit, foreground request, so it may take as
/// long as a large file needs.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Where the manifest is read from.
///
/// `PRUNE_JUICE_UPDATE_URL` overrides it, which is how a staging feed gets
/// tested. It is not a security hole: the signature still has to verify
/// against the key compiled into *this* binary, so pointing the tool at a
/// different server cannot make it install anything new.
pub fn manifest_url() -> String {
    feed_url(std::env::var("PRUNE_JUICE_UPDATE_URL").ok().as_deref())
}

/// The decision, separated from the environment so it can be tested without
/// setting a variable other tests in this binary would see.
fn feed_url(override_url: Option<&str>) -> String {
    match override_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => u.to_string(),
        None => format!(
            "{}/releases/latest/download/{MANIFEST_ASSET}",
            env!("CARGO_PKG_REPOSITORY").trim_end_matches('/')
        ),
    }
}

/// The detached signature always sits beside what it signs.
pub fn signature_url(manifest_url: &str) -> String {
    format!("{manifest_url}.minisig")
}

// --- the manifest --------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// The released version, as a semver string.
    pub version: String,
    /// RFC 3339, for display only. Never used to decide anything: a clock is
    /// not evidence, and the version is.
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A `.tar.gz` holding the `prune-juice` executable.
    Cli,
    /// The Mac app archive. Listed so the manifest is a complete description
    /// of a release, but never installed from here — Sparkle owns that path.
    App,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    /// A Rust target triple, matched exactly against [`HOST_TARGET`].
    pub target: String,
    pub url: String,
    /// Lower-case hex SHA-256. Authenticated by the manifest's signature.
    pub sha256: String,
    #[serde(default)]
    pub bytes: u64,
}

impl Manifest {
    /// Parse a manifest that has *already* had its signature verified.
    ///
    /// Private on purpose: the only way to obtain a `Manifest` from bytes is
    /// through [`Checker::fetch_manifest`], which verifies first. That is the
    /// same shape as `SafeToDelete` — a type you cannot construct without
    /// having done the check.
    fn parse(bytes: &[u8]) -> Result<Self> {
        let m: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| Error::Config(format!("the update manifest is not readable: {e}")))?;
        if m.schema > SCHEMA {
            return Err(Error::Config(format!(
                "the release feed uses manifest schema {} and this build understands {SCHEMA}; \
                 update by hand this once",
                m.schema
            )));
        }
        Ok(m)
    }

    /// The CLI artifact for this machine.
    pub fn cli_for_host(&self) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::Cli && a.target == HOST_TARGET)
    }
}

// --- comparing versions --------------------------------------------------

/// Is `latest` newer than `current`?
///
/// Pre-releases are only offered to someone already running one. Handing
/// `0.3.0-beta.1` to a `0.2.0` user would be an unrequested change of channel.
pub fn is_newer(current: &str, latest: &str) -> Result<bool> {
    let cur = semver::Version::parse(current.trim())
        .map_err(|e| Error::Config(format!("cannot read this build's own version: {e}")))?;
    let new = semver::Version::parse(latest.trim())
        .map_err(|e| Error::Config(format!("the release version {latest} is not semver: {e}")))?;
    if !new.pre.is_empty() && cur.pre.is_empty() {
        return Ok(false);
    }
    Ok(new > cur)
}

// --- the user's preference ----------------------------------------------

/// Where the "do not check" answer came from. Worth keeping so the CLI can
/// say *why* checking is off rather than just that it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    Config,
    Environment,
    NoReleaseKey,
    NoTransport,
}

impl Off {
    pub fn describe(self) -> &'static str {
        match self {
            Off::Config => "automatic update checks are off (prune-juice --update-check on)",
            Off::Environment => "automatic update checks are off (PRUNE_JUICE_NO_UPDATE_CHECK)",
            Off::NoReleaseKey => {
                "this build has no release-signing key, so it cannot verify an update"
            }
            Off::NoTransport => "curl is not available, so updates cannot be fetched",
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ConfigFile {
    /// Absent means "not decided", which is on. Only an explicit `false`
    /// turns checking off, so a config file written for something else never
    /// disables it by accident.
    #[serde(skip_serializing_if = "Option::is_none")]
    update_check: Option<bool>,
    /// Anything else in the file is preserved across a write, so this can
    /// grow into a real settings file without this code losing keys it does
    /// not know about.
    #[serde(flatten)]
    rest: std::collections::BTreeMap<String, serde_json::Value>,
}

/// `$XDG_*_HOME/prune-juice`, or `$HOME/<fallback>/prune-juice`.
///
/// Takes the environment rather than reading it, so the tests below can check
/// both branches without setting a variable the rest of this test binary
/// would see. Matches the hand-rolled XDG resolution in `waiver`.
fn base_dir(xdg: Option<&str>, home: Option<&str>, fallback: &str) -> Result<PathBuf> {
    if let Some(x) = xdg.filter(|x| !x.is_empty()) {
        return Ok(PathBuf::from(x).join("prune-juice"));
    }
    match home.filter(|h| !h.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(fallback).join("prune-juice")),
        None => Err(Error::Config(format!(
            "cannot locate a {fallback} directory"
        ))),
    }
}

fn config_dir() -> Result<PathBuf> {
    base_dir(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        ".config",
    )
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

/// Is automatic checking wanted here?
///
/// The environment beats the file, because the environment is how a one-off
/// run and a CI image say "not now" without editing anyone's preferences.
pub fn automatic_checks() -> std::result::Result<(), Off> {
    if std::env::var("PRUNE_JUICE_NO_UPDATE_CHECK").is_ok_and(|v| v != "0" && !v.is_empty()) {
        return Err(Off::Environment);
    }
    if verify::release_key().is_none() {
        return Err(Off::NoReleaseKey);
    }
    let stored = config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<ConfigFile>(&t).ok())
        .and_then(|c| c.update_check);
    if stored == Some(false) {
        return Err(Off::Config);
    }
    Ok(())
}

/// Persist the preference. Rewrites only the one key, so an unrelated setting
/// in the same file survives.
pub fn set_automatic_checks(on: bool) -> Result<PathBuf> {
    let path = config_path()?;
    let mut cfg: ConfigFile = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    cfg.update_check = Some(on);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?).map_err(Error::Io)?;
    Ok(path)
}

// --- the cached answer ---------------------------------------------------

/// The remembered result of the last check.
///
/// A *cache*, correctly: it lives under the cache directory, losing it costs
/// nothing but one extra request, and nothing in it is a decision the user
/// made. The preference above is config; this is not.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cached {
    pub checked_unix: i64,
    /// `None` records a check that failed. Kept rather than discarded so a
    /// failing server is retried on an hour, not on every run.
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
}

impl Cached {
    fn fresh_at(&self, now_unix: i64) -> bool {
        let ttl = if self.latest.is_some() {
            CACHE_TTL
        } else {
            FAILURE_TTL
        };
        let age = now_unix.saturating_sub(self.checked_unix);
        (0..ttl.as_secs() as i64).contains(&age)
    }
}

fn cache_dir() -> Result<PathBuf> {
    base_dir(
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        ".cache",
    )
}

pub fn cache_path() -> Result<PathBuf> {
    Ok(cache_dir()?.join("update-check.json"))
}

// --- the answer ----------------------------------------------------------

/// What to tell the user, and what they can do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub current: String,
    pub latest: String,
    pub notes_url: Option<String>,
    pub origin: Origin,
}

impl Notice {
    /// The two lines the CLI prints. Returned rather than printed: this crate
    /// never writes to a stream.
    ///
    /// The second line is the *actionable* one and it changes with the origin,
    /// because telling a Homebrew user to run `--update` would be telling them
    /// to break their installation.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "Prune Juice {} is available. You have {}.",
            self.latest, self.current
        )];
        out.push(match (&self.origin, self.origin.upgrade_command()) {
            (Origin::AppBundle, _) => {
                "Prune Juice.app updates itself, including this helper.".into()
            }
            (_, Some(cmd)) => format!("Run {cmd} to install."),
            (_, None) => "Run prune-juice --update to install.".into(),
        });
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Checking did not happen, and here is why.
    Off(Off),
    /// A check happened and there is nothing newer.
    UpToDate,
    /// Something newer exists.
    Available(Notice),
    /// A check was attempted and failed. Carried rather than swallowed so
    /// `--check-update`, which was asked for explicitly, can say what went
    /// wrong — while the automatic path ignores it.
    Failed(String),
}

impl Decision {
    pub fn notice(&self) -> Option<&Notice> {
        match self {
            Decision::Available(n) => Some(n),
            _ => None,
        }
    }
}

// --- the check -----------------------------------------------------------

pub struct Checker<'a> {
    fetcher: &'a dyn Fetcher,
    now_unix: i64,
    cache: Option<PathBuf>,
    origin: Origin,
    current: String,
    url: String,
    key: Option<&'static str>,
}

impl<'a> Checker<'a> {
    /// A checker that talks to `fetcher` and remembers its answers in the
    /// user's cache directory.
    pub fn new(fetcher: &'a dyn Fetcher, now_unix: i64) -> Self {
        Self {
            fetcher,
            now_unix,
            cache: cache_path().ok(),
            origin: std::env::current_exe()
                .map(|p| Origin::detect(&p))
                .unwrap_or(Origin::Standalone),
            current: current_version().to_string(),
            url: manifest_url(),
            key: verify::release_key(),
        }
    }

    /// Verify against a different key.
    ///
    /// Crate-private, and it stays that way. The trust anchor is a compiled-in
    /// constant precisely so that nothing outside this crate can choose it —
    /// no flag, no environment variable, no argument from the CLI. The tests
    /// need to sign something, and that is the whole reason this exists.
    #[cfg(test)]
    fn with_release_key(mut self, key: Option<&'static str>) -> Self {
        self.key = key;
        self
    }

    /// Override the cache location. Every test uses this; nothing else should.
    pub fn with_cache(mut self, path: Option<PathBuf>) -> Self {
        self.cache = path;
        self
    }

    pub fn with_origin(mut self, origin: Origin) -> Self {
        self.origin = origin;
        self
    }

    pub fn with_current_version(mut self, v: impl Into<String>) -> Self {
        self.current = v.into();
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// The remembered answer, if it is still fresh. No network, no spawn — a
    /// single small file read, which is why it is safe on the launch path.
    pub fn cached(&self) -> Option<Decision> {
        let cached: Cached = self
            .cache
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())?;
        if !cached.fresh_at(self.now_unix) {
            return None;
        }
        Some(self.decide(cached.latest.as_deref(), cached.notes_url))
    }

    /// Fetch, verify, and remember. This one makes a request, so it belongs on
    /// a background thread or behind an explicit `--check-update`.
    pub fn refresh(&self) -> Decision {
        match self.fetch_manifest() {
            Ok(m) => {
                self.remember(Some(&m.version), m.notes_url.clone());
                self.decide(Some(&m.version), m.notes_url)
            }
            Err(e) => {
                self.remember(None, None);
                Decision::Failed(e.to_string())
            }
        }
    }

    /// The fresh answer if one is due, otherwise the remembered one.
    pub fn check(&self) -> Decision {
        match self.cached() {
            Some(d) => d,
            None => self.refresh(),
        }
    }

    /// Fetch the manifest and its signature, and verify before parsing.
    ///
    /// The order is the security property. Bytes that have not been verified
    /// are never handed to a parser that will act on their contents.
    pub fn fetch_manifest(&self) -> Result<Manifest> {
        let Some(key) = self.key else {
            return Err(Error::Config(
                "this build has no release-signing key, so an update cannot be verified".into(),
            ));
        };
        let body = self
            .fetcher
            .get(&self.url, net::MAX_MANIFEST_BYTES, MANIFEST_TIMEOUT)?;
        let sig = self.fetcher.get(
            &signature_url(&self.url),
            net::MAX_MANIFEST_BYTES,
            MANIFEST_TIMEOUT,
        )?;
        let sig = String::from_utf8(sig)
            .map_err(|_| Error::Config("the update signature is not text".into()))?;

        verify::signature(key, &body, &sig)?;

        Manifest::parse(&body)
    }

    fn decide(&self, latest: Option<&str>, notes_url: Option<String>) -> Decision {
        let Some(latest) = latest else {
            return Decision::Failed("the last update check did not succeed".into());
        };
        match is_newer(&self.current, latest) {
            Ok(true) => Decision::Available(Notice {
                current: self.current.clone(),
                latest: latest.to_string(),
                notes_url,
                origin: self.origin.clone(),
            }),
            Ok(false) => Decision::UpToDate,
            Err(e) => Decision::Failed(e.to_string()),
        }
    }

    /// Write the cache. A failure here is ignored: not being able to remember
    /// an answer is a reason to ask again next time, not a reason to fail a
    /// scan.
    fn remember(&self, latest: Option<&str>, notes_url: Option<String>) {
        let Some(path) = &self.cache else { return };
        let entry = Cached {
            checked_unix: self.now_unix,
            latest: latest.map(str::to_string),
            notes_url,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&entry) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// How long a run will wait for the check before getting on with the scan.
///
/// A bound, not a promise of speed. The check's own request timeouts are
/// [`MANIFEST_TIMEOUT`] each for the manifest and its signature, so an
/// answer that is coming arrives well inside this; what this protects
/// against is a server that accepts a connection and then says nothing.
pub const NOTICE_WAIT: Duration = Duration::from_secs(6);

/// Wait for the background check, and return only news.
///
/// This is the deliberate exception to "never delay a scan": a release worth
/// installing is worth hearing about before the work starts rather than as a
/// footnote under a report that has already scrolled past. The cost is paid
/// at most once a day, because a cached answer returns from this in about a
/// millisecond — the thread reads the cache before it reaches for the
/// network.
///
/// `UpToDate`, `Failed` and `Off` all read as `None`. A check that found
/// nothing, or could not run, is not something to put in front of anyone.
pub fn await_notice(rx: &std::sync::mpsc::Receiver<Decision>, wait: Duration) -> Option<Notice> {
    match rx.recv_timeout(wait) {
        Ok(Decision::Available(n)) => Some(n),
        _ => None,
    }
}

/// Run a check on a background thread.
///
/// This is rule 1 made concrete. The caller reads the receiver whenever it
/// next has nothing better to do — typically at the end of a run — and a
/// result that has not arrived by then is simply not shown. The thread is
/// detached and its work is already recorded in the cache, so nothing is
/// wasted: the next run reports it immediately.
pub fn spawn_check(now_unix: i64) -> std::sync::mpsc::Receiver<Decision> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Err(off) = automatic_checks() {
            let _ = tx.send(Decision::Off(off));
            return;
        }
        // Asked before spawning anything, so a machine without curl reports
        // "cannot check" instead of a confusing process-spawn error.
        if !net::Curl::available() {
            let _ = tx.send(Decision::Off(Off::NoTransport));
            return;
        }
        let curl = net::Curl;
        let _ = tx.send(Checker::new(&curl, now_unix).check());
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use net::{OfflineFetcher, StaticFetcher};

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pj-update-{tag}-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn manifest(version: &str) -> String {
        serde_json::json!({
            "schema": 1,
            "version": version,
            "artifacts": [{
                "kind": "cli",
                "target": HOST_TARGET,
                "url": "https://example.test/prune-juice.tar.gz",
                "sha256": "00",
                "bytes": 10
            }]
        })
        .to_string()
    }

    #[test]
    fn newer_is_semver_ordering_not_string_ordering() {
        assert!(is_newer("0.9.0", "0.10.0").unwrap());
        assert!(!is_newer("0.10.0", "0.9.0").unwrap());
        assert!(!is_newer("0.2.0", "0.2.0").unwrap());
        assert!(is_newer("0.1.0", "0.2.0").unwrap());
    }

    #[test]
    fn a_prerelease_is_not_offered_to_someone_on_a_stable_build() {
        assert!(!is_newer("0.2.0", "0.3.0-beta.1").unwrap());
        // …but is offered to someone already on a pre-release.
        assert!(is_newer("0.3.0-beta.1", "0.3.0-beta.2").unwrap());
        assert!(is_newer("0.3.0-beta.2", "0.3.0").unwrap());
    }

    #[test]
    fn an_unparseable_release_version_is_a_failure_not_a_panic() {
        let f = StaticFetcher::new();
        let c = Checker::new(&f, 1000)
            .with_cache(None)
            .with_current_version("0.1.0");
        assert!(matches!(
            c.decide(Some("not-a-version"), None),
            Decision::Failed(_)
        ));
    }

    #[test]
    fn a_dead_server_yields_no_news_and_never_an_error() {
        let path = tmp("offline");
        let d = Checker::new(&OfflineFetcher, 1000)
            .with_cache(Some(path.clone()))
            .with_current_version("0.1.0")
            .check();
        // Failed, not Available, and above all not a propagated error: the
        // caller has no way to turn this into a non-zero exit.
        assert!(matches!(d, Decision::Failed(_)), "{d:?}");
        assert!(d.notice().is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failed_check_is_remembered_so_a_down_server_is_not_asked_every_run() {
        let path = tmp("failttl");
        let c = Checker::new(&OfflineFetcher, 1000)
            .with_cache(Some(path.clone()))
            .with_current_version("0.1.0");
        assert!(matches!(c.check(), Decision::Failed(_)));

        let stored: Cached =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored.checked_unix, 1000);
        assert!(stored.latest.is_none());
        // Inside the failure window the cache answers; outside it does not.
        assert!(stored.fresh_at(1000 + FAILURE_TTL.as_secs() as i64 - 1));
        assert!(!stored.fresh_at(1000 + FAILURE_TTL.as_secs() as i64 + 1));
        // And a failure expires far sooner than a success would.
        assert!(FAILURE_TTL < CACHE_TTL);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_successful_answer_is_trusted_for_a_day_and_no_longer() {
        let c = Cached {
            checked_unix: 1000,
            latest: Some("0.2.0".into()),
            notes_url: None,
        };
        assert!(c.fresh_at(1000));
        assert!(c.fresh_at(1000 + CACHE_TTL.as_secs() as i64 - 1));
        assert!(!c.fresh_at(1000 + CACHE_TTL.as_secs() as i64));
    }

    #[test]
    fn a_cache_written_in_the_future_is_not_trusted() {
        // A clock that moved backwards must not pin the answer for ever.
        let c = Cached {
            checked_unix: 5000,
            latest: Some("0.2.0".into()),
            notes_url: None,
        };
        assert!(!c.fresh_at(1000));
    }

    #[test]
    fn the_cached_path_makes_no_request_at_all() {
        let path = tmp("cachehit");
        std::fs::write(
            &path,
            serde_json::to_string(&Cached {
                checked_unix: 1000,
                latest: Some("0.2.0".into()),
                notes_url: Some("https://example.test/notes".into()),
            })
            .unwrap(),
        )
        .unwrap();
        // The fetcher has nothing configured, so any request would error.
        let f = StaticFetcher::new();
        let d = Checker::new(&f, 1001)
            .with_cache(Some(path.clone()))
            .with_current_version("0.1.0")
            .with_origin(Origin::Standalone)
            .check();
        let n = d.notice().expect("available");
        assert_eq!(n.latest, "0.2.0");
        assert_eq!(n.current, "0.1.0");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unsigned_manifest_is_refused_even_when_it_parses_perfectly() {
        let url = "https://example.test/m.json";
        let f = StaticFetcher::new()
            .with(url, manifest("9.9.9"))
            .with(&signature_url(url), "untrusted comment: not a signature\n");
        let d = Checker::new(&f, 1000)
            .with_cache(None)
            .with_current_version("0.1.0")
            .with_url(url)
            .refresh();
        assert!(matches!(d, Decision::Failed(_)), "{d:?}");
    }

    #[test]
    fn a_signed_manifest_is_read_end_to_end_and_offered() {
        // The whole chain in one test: fetch, verify against a real minisign
        // signature, parse, compare versions, cache the answer.
        use verify::fixture;
        let url = "https://example.test/update-manifest.json";
        let f = StaticFetcher::new()
            .with(url, fixture::MANIFEST)
            .with(&signature_url(url), fixture::SIGNATURE);
        let cache = tmp("e2e");

        let d = Checker::new(&f, 1000)
            .with_cache(Some(cache.clone()))
            .with_current_version("0.1.0")
            .with_origin(Origin::Standalone)
            .with_url(url)
            .with_release_key(Some(fixture::PUBLIC_KEY))
            .check();

        let n = d.notice().expect("0.2.0 is newer than 0.1.0");
        assert_eq!(n.latest, "0.2.0");
        assert_eq!(
            n.lines()[0],
            "Prune Juice 0.2.0 is available. You have 0.1.0."
        );

        // …and the answer is now cached, so the next run needs no request.
        let stored: Cached =
            serde_json::from_str(&std::fs::read_to_string(&cache).unwrap()).unwrap();
        assert_eq!(stored.latest.as_deref(), Some("0.2.0"));
        let _ = std::fs::remove_file(cache);
    }

    /// What `cached_notice` rests on: an answer already on disk can be turned
    /// into a notice with a fetcher that cannot make a request at all. That is
    /// the property that lets the CLI say it before the scan instead of after
    /// the report — there is no network in this path to delay anything.
    #[test]
    fn a_remembered_answer_becomes_a_notice_with_no_request_at_all() {
        let cache = tmp("cached-notice");
        std::fs::write(
            &cache,
            serde_json::json!({"checked_unix": 1000, "latest": "0.2.0"}).to_string(),
        )
        .unwrap();

        let read = |now: i64, current: &str| {
            // OfflineFetcher fails every request, so anything this returns
            // came from the file and nothing else.
            Checker::new(&OfflineFetcher, now)
                .with_cache(Some(cache.clone()))
                .with_current_version(current)
                .with_release_key(None)
                .cached()
        };

        let d = read(1000, "0.1.0").expect("the entry is fresh");
        assert_eq!(d.notice().expect("0.2.0 is newer").latest, "0.2.0");

        // Already on it: an answer, but not news.
        assert_eq!(read(1000, "0.2.0"), Some(Decision::UpToDate));

        // Past the day it is trusted for, so there is nothing to say and the
        // caller falls back to the background check.
        assert_eq!(read(1000 + CACHE_TTL.as_secs() as i64, "0.1.0"), None);

        let _ = std::fs::remove_file(&cache);
    }

    /// A cache recording a *failed* check is not news either. It exists to
    /// stop the tool retrying for an hour, not to be shown to anyone.
    #[test]
    fn a_remembered_failure_is_never_put_in_front_of_a_scan() {
        let cache = tmp("cached-failure");
        std::fs::write(
            &cache,
            serde_json::json!({"checked_unix": 1000, "latest": null}).to_string(),
        )
        .unwrap();
        let d = Checker::new(&OfflineFetcher, 1000)
            .with_cache(Some(cache.clone()))
            .with_current_version("0.1.0")
            .with_release_key(None)
            .cached();
        assert!(matches!(d, Some(Decision::Failed(_))), "{d:?}");
        assert!(d.unwrap().notice().is_none(), "a failure is not a notice");
        let _ = std::fs::remove_file(&cache);
    }

    /// The wait a run performs before scanning. News comes back; everything
    /// else is silence, and a check that never answers costs the bound and
    /// nothing more.
    #[test]
    fn the_pre_scan_wait_returns_news_and_never_anything_else() {
        let notice = Notice {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            notes_url: None,
            origin: Origin::Standalone,
        };

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Decision::Available(notice.clone())).unwrap();
        assert_eq!(await_notice(&rx, NOTICE_WAIT), Some(notice));

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Decision::UpToDate).unwrap();
        assert_eq!(await_notice(&rx, NOTICE_WAIT), None);

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Decision::Failed("no".into())).unwrap();
        assert_eq!(await_notice(&rx, NOTICE_WAIT), None);

        // Nobody is going to send, and the sender is gone: this must return
        // rather than hold the scan open.
        let (tx, rx) = std::sync::mpsc::channel::<Decision>();
        drop(tx);
        assert_eq!(await_notice(&rx, NOTICE_WAIT), None);

        // A sender that never sends costs the bound, once.
        let (_tx, rx) = std::sync::mpsc::channel::<Decision>();
        let began = std::time::Instant::now();
        let short = Duration::from_millis(60);
        assert_eq!(await_notice(&rx, short), None);
        assert!(began.elapsed() < NOTICE_WAIT, "waited past the bound");
    }

    #[test]
    fn the_same_release_is_no_news_when_you_are_already_on_it() {
        use verify::fixture;
        let url = "https://example.test/update-manifest.json";
        let f = StaticFetcher::new()
            .with(url, fixture::MANIFEST)
            .with(&signature_url(url), fixture::SIGNATURE);
        let d = Checker::new(&f, 1000)
            .with_cache(None)
            .with_current_version("0.2.0")
            .with_url(url)
            .with_release_key(Some(fixture::PUBLIC_KEY))
            .refresh();
        assert_eq!(d, Decision::UpToDate);
    }

    #[test]
    fn a_build_with_no_release_key_cannot_be_talked_into_a_check() {
        use verify::fixture;
        let url = "https://example.test/update-manifest.json";
        let f = StaticFetcher::new()
            .with(url, fixture::MANIFEST)
            .with(&signature_url(url), fixture::SIGNATURE);
        // Everything else is in place: a reachable server, a real signature.
        // With no key compiled in there is still nothing to trust it against.
        let d = Checker::new(&f, 1000)
            .with_cache(None)
            .with_current_version("0.1.0")
            .with_url(url)
            .with_release_key(None)
            .refresh();
        assert!(matches!(d, Decision::Failed(_)), "{d:?}");
    }

    #[test]
    fn a_manifest_from_a_newer_schema_is_stood_aside_from_not_misread() {
        let body = serde_json::json!({"schema": SCHEMA + 1, "version": "9.9.9"}).to_string();
        let e = Manifest::parse(body.as_bytes()).unwrap_err();
        assert!(e.to_string().contains("by hand"), "{e}");
    }

    #[test]
    fn a_manifest_offering_no_build_for_this_machine_offers_nothing() {
        let body = serde_json::json!({
            "schema": 1,
            "version": "9.9.9",
            "artifacts": [{
                "kind": "cli",
                "target": "sparc64-unknown-linux-gnu",
                "url": "https://example.test/x.tar.gz",
                "sha256": "00"
            }]
        })
        .to_string();
        let m = Manifest::parse(body.as_bytes()).unwrap();
        assert!(m.cli_for_host().is_none());
    }

    #[test]
    fn the_app_artifact_is_never_mistaken_for_the_cli_one() {
        let body = serde_json::json!({
            "schema": 1,
            "version": "9.9.9",
            "artifacts": [{
                "kind": "app",
                "target": HOST_TARGET,
                "url": "https://example.test/PruneJuice.zip",
                "sha256": "00"
            }]
        })
        .to_string();
        let m = Manifest::parse(body.as_bytes()).unwrap();
        assert!(m.cli_for_host().is_none(), "the app archive is Sparkle's");
    }

    #[test]
    fn the_notice_says_what_to_run_for_the_way_this_copy_was_installed() {
        let n = |origin| Notice {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            notes_url: None,
            origin,
        };
        let brew = n(Origin::Homebrew).lines();
        assert_eq!(brew[0], "Prune Juice 0.2.0 is available. You have 0.1.0.");
        assert_eq!(brew[1], "Run brew upgrade prune-juice to install.");

        assert_eq!(
            n(Origin::Standalone).lines()[1],
            "Run prune-juice --update to install."
        );
        assert_eq!(
            n(Origin::Cargo).lines()[1],
            "Run cargo install prune-juice-cli --force to install."
        );
        // The bundled helper is told nothing to run, because there is nothing
        // the user should run.
        assert!(!n(Origin::AppBundle).lines()[1].contains("--update"));
    }

    #[test]
    fn the_target_triple_is_the_one_cargo_built_for() {
        assert!(!HOST_TARGET.is_empty());
        assert!(HOST_TARGET.contains('-'), "{HOST_TARGET}");
    }

    #[test]
    fn the_signature_always_sits_beside_what_it_signs() {
        assert_eq!(
            signature_url("https://example.test/a/update-manifest.json"),
            "https://example.test/a/update-manifest.json.minisig"
        );
    }

    #[test]
    fn the_default_feed_is_https_and_points_at_a_release_asset() {
        let u = feed_url(None);
        assert!(u.starts_with("https://"), "{u}");
        assert!(u.ends_with(MANIFEST_ASSET), "{u}");
        assert!(u.contains("/releases/latest/download/"), "{u}");
        // A staging feed overrides it; an empty variable does not.
        assert_eq!(
            feed_url(Some("https://staging/m.json")),
            "https://staging/m.json"
        );
        assert_eq!(feed_url(Some("  ")), u);
        assert_eq!(feed_url(Some("")), u);
    }

    #[test]
    fn the_cache_is_a_cache_and_the_preference_is_config() {
        // Losing the cache costs one request; losing the config would lose a
        // decision the user made. They must not live in the same place.
        let cache = base_dir(None, Some("/home/x"), ".cache").unwrap();
        let config = base_dir(None, Some("/home/x"), ".config").unwrap();
        assert_eq!(cache, PathBuf::from("/home/x/.cache/prune-juice"));
        assert_eq!(config, PathBuf::from("/home/x/.config/prune-juice"));
        assert_ne!(cache, config);

        // XDG wins where it is set, and an empty value is not a setting.
        assert_eq!(
            base_dir(Some("/xdg"), Some("/home/x"), ".cache").unwrap(),
            PathBuf::from("/xdg/prune-juice")
        );
        assert_eq!(
            base_dir(Some(""), Some("/home/x"), ".cache").unwrap(),
            PathBuf::from("/home/x/.cache/prune-juice")
        );
        // Nowhere to put it is an error, not a path under the working
        // directory.
        assert!(base_dir(None, None, ".cache").is_err());
    }
}
