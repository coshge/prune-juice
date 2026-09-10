//! Fetching bytes over HTTPS.
//!
//! A trait, not a concrete client, for the same reason `DockerClient` is one:
//! every test in this module has to be able to answer a request without a
//! network, and the transport has to be replaceable without the update logic
//! noticing.
//!
//! The shipping implementation spawns the platform's `curl`. That is a
//! deliberate choice, not an expedient one:
//!
//! * A TLS stack in this crate's dependency graph would be the single heaviest
//!   thing in it, added for a peripheral convenience. The same reasoning that
//!   kept `prune-juice-ffi` out of the workspace applies here.
//! * The trust store is then the platform's, kept current by the OS rather
//!   than by whenever this crate last bumped a roots crate.
//! * **Nothing is trusted because `curl` said so.** Every byte that decides
//!   anything is verified in-process against a compiled-in public key, so a
//!   missing, old, or subverted `curl` can only ever cause "no update" — never
//!   a bad one. That is what makes the choice safe rather than merely cheap.
//!
//! Swapping in a Rust HTTP client later means writing one more `Fetcher`.

use std::process::Command;
use std::time::Duration;

use crate::error::{Error, Result};

/// Nothing this module fetches is large. The manifest is a few hundred bytes;
/// the largest artifact is a compressed CLI binary. A cap that a legitimate
/// release cannot reach means a hostile or broken server cannot fill the disk.
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

pub trait Fetcher: Send + Sync {
    /// GET `url`, following HTTPS redirects only, refusing anything larger
    /// than `limit` and giving up after `timeout`.
    fn get(&self, url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>>;
}

/// The shipping transport.
#[derive(Clone, Copy, Debug, Default)]
pub struct Curl;

impl Curl {
    /// Is the transport usable at all? Answered without making a request, so
    /// a machine with no `curl` reports "cannot check" rather than "no update".
    pub fn available() -> bool {
        Command::new("curl")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

impl Fetcher for Curl {
    fn get(&self, url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>> {
        // Plain http is refused outright, and so is a redirect that tries to
        // downgrade to it — `--proto-redir` is a separate setting from
        // `--proto`, and omitting it is the classic way to think you have
        // required TLS when you have not.
        if !url.starts_with("https://") {
            return Err(Error::Config(format!(
                "refusing to fetch {url}: updates are only ever read over https"
            )));
        }
        let secs = timeout.as_secs().max(1).to_string();
        let out = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--fail",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--tlsv1.2",
                "--connect-timeout",
                "3",
                "--max-time",
                &secs,
                "--max-filesize",
                &limit.to_string(),
                "--user-agent",
                concat!("prune-juice/", env!("CARGO_PKG_VERSION")),
                url,
            ])
            // `output()` reads stdout and stderr concurrently. That is not a
            // detail to gloss over: a pipe nobody reads fills at 64 KB and the
            // writer blocks for ever, so the tempting `wait()`-then-read shape
            // would deadlock on any artifact larger than the buffer. Here the
            // concurrency comes from the standard library rather than from
            // remembering to spawn a drain thread.
            .output()
            .map_err(|e| Error::Config(format!("could not run curl: {e}")))?;

        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let why = if why.is_empty() {
                format!("curl exited {}", out.status.code().unwrap_or(-1))
            } else {
                why
            };
            return Err(Error::Config(format!("could not fetch {url}: {why}")));
        }
        // curl enforces the cap when the server declares a length; when it
        // does not, this is the enforcement.
        if out.stdout.len() as u64 > limit {
            return Err(Error::Config(format!(
                "{url} returned more than the {limit} bytes an update is allowed to be"
            )));
        }
        Ok(out.stdout)
    }
}

/// A fetcher backed by a fixed table. The tests are built on this; it is
/// public so the CLI's own tests can be too.
#[derive(Default)]
pub struct StaticFetcher {
    responses: std::collections::BTreeMap<String, Vec<u8>>,
}

impl StaticFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, url: &str, body: impl Into<Vec<u8>>) -> Self {
        self.responses.insert(url.to_string(), body.into());
        self
    }
}

impl Fetcher for StaticFetcher {
    fn get(&self, url: &str, limit: u64, _timeout: Duration) -> Result<Vec<u8>> {
        match self.responses.get(url) {
            Some(b) if b.len() as u64 > limit => {
                Err(Error::Config(format!("{url} is over the {limit} byte cap")))
            }
            Some(b) => Ok(b.clone()),
            None => Err(Error::Config(format!("no response configured for {url}"))),
        }
    }
}

/// A fetcher that always fails, for asserting that a failure degrades rather
/// than propagates.
pub struct OfflineFetcher;

impl Fetcher for OfflineFetcher {
    fn get(&self, _url: &str, _limit: u64, _timeout: Duration) -> Result<Vec<u8>> {
        Err(Error::Config("the network is unavailable".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_refused_before_a_process_is_spawned() {
        let e = Curl
            .get(
                "http://example.invalid/m.json",
                1024,
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(e.to_string().contains("https"), "{e}");
    }

    #[test]
    fn a_response_over_the_cap_is_an_error_not_a_truncation() {
        let f = StaticFetcher::new().with("https://x/m", vec![b'x'; 100]);
        assert!(f.get("https://x/m", 10, Duration::from_secs(1)).is_err());
        assert!(f.get("https://x/m", 100, Duration::from_secs(1)).is_ok());
    }
}
