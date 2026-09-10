//! Docker context discovery.
//!
//! Contexts are how a user names a daemon, but they are *not* identity. On the
//! author's machine `default` and `orbstack` are the same engine, because
//! `/var/run/docker.sock` is a symlink into `~/.orbstack/run/`. Scanning both
//! would double-count 136 GB.
//!
//! So discovery produces candidate endpoints; deduplication happens later,
//! against the daemon's own `/info` `ID`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One place a daemon might be listening.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerContext {
    pub name: String,
    /// A docker host URL: `unix://…`, `npipe://…`, `tcp://…` or `ssh://…`.
    pub endpoint: String,
    /// True when this is the context the CLI would use right now.
    pub current: bool,
}

impl DockerContext {
    /// A local socket or pipe. Remote daemons are never scanned implicitly and
    /// are refused for destructive work without an explicit override.
    pub fn is_local(&self) -> bool {
        self.endpoint.starts_with("unix://") || self.endpoint.starts_with("npipe://")
    }

    /// Filesystem path of a unix socket endpoint, if that is what this is.
    pub fn socket_path(&self) -> Option<PathBuf> {
        self.endpoint.strip_prefix("unix://").map(PathBuf::from)
    }
}

fn docker_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".docker"))
}

#[derive(Deserialize)]
struct MetaFile {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Endpoints")]
    endpoints: Option<BTreeMap<String, EndpointFile>>,
}

#[derive(Deserialize)]
struct EndpointFile {
    #[serde(rename = "Host")]
    host: Option<String>,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(rename = "currentContext")]
    current_context: Option<String>,
}

/// The platform's conventional socket location, used when nothing else says
/// otherwise.
pub fn default_endpoint() -> String {
    if cfg!(target_os = "windows") {
        "npipe:////./pipe/docker_engine".to_string()
    } else {
        "unix:///var/run/docker.sock".to_string()
    }
}

/// Parse the stored context metadata under a docker config directory.
///
/// Split out from [`discover`] so it can be tested against a fixture tree
/// without touching the real `~/.docker`.
pub fn contexts_from_dir(config_dir: &Path) -> Vec<DockerContext> {
    let current = std::fs::read_to_string(config_dir.join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<ConfigFile>(&s).ok())
        .and_then(|c| c.current_context);

    let mut out = Vec::new();
    let meta_root = config_dir.join("contexts").join("meta");
    if let Ok(entries) = std::fs::read_dir(&meta_root) {
        for entry in entries.flatten() {
            let meta = entry.path().join("meta.json");
            let Ok(text) = std::fs::read_to_string(&meta) else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<MetaFile>(&text) else {
                continue;
            };
            let Some(name) = parsed.name else { continue };
            let host = parsed
                .endpoints
                .as_ref()
                .and_then(|e| e.get("docker"))
                .and_then(|e| e.host.clone())
                .unwrap_or_default();
            if host.trim().is_empty() {
                continue;
            }
            let is_current = current.as_deref() == Some(name.as_str());
            out.push(DockerContext {
                name,
                endpoint: host,
                current: is_current,
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Every endpoint worth trying, most-likely first.
///
/// Order of precedence:
///   1. `DOCKER_HOST`, which overrides everything.
///   2. Stored contexts, with the current one first.
///   3. The platform default socket, as `default`.
pub fn discover() -> Vec<DockerContext> {
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if !host.trim().is_empty() {
            return vec![DockerContext {
                name: "DOCKER_HOST".to_string(),
                endpoint: host,
                current: true,
            }];
        }
    }

    let mut out = docker_config_dir()
        .map(|d| contexts_from_dir(&d))
        .unwrap_or_default();

    let default_ep = default_endpoint();
    if !out.iter().any(|c| c.name == "default") {
        out.push(DockerContext {
            name: "default".to_string(),
            endpoint: default_ep,
            current: !out.iter().any(|c| c.current),
        });
    }

    // Current context first; everything else alphabetical.
    out.sort_by(|a, b| b.current.cmp(&a.current).then(a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pj-ctx-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_contexts_and_marks_the_current_one() {
        let dir = tmpdir("read");
        write(&dir.join("config.json"), r#"{"currentContext":"orbstack"}"#);
        write(
            &dir.join("contexts/meta/aaa/meta.json"),
            r#"{"Name":"orbstack","Endpoints":{"docker":{"Host":"unix:///Users/x/.orbstack/run/docker.sock"}}}"#,
        );
        write(
            &dir.join("contexts/meta/bbb/meta.json"),
            r#"{"Name":"staging","Endpoints":{"docker":{"Host":"tcp://10.0.0.5:2375"}}}"#,
        );

        let ctxs = contexts_from_dir(&dir);
        assert_eq!(ctxs.len(), 2);

        let orb = ctxs.iter().find(|c| c.name == "orbstack").unwrap();
        assert!(orb.current);
        assert!(orb.is_local());
        assert_eq!(
            orb.socket_path().unwrap(),
            PathBuf::from("/Users/x/.orbstack/run/docker.sock")
        );

        let staging = ctxs.iter().find(|c| c.name == "staging").unwrap();
        assert!(!staging.current);
        assert!(!staging.is_local(), "tcp:// must not read as local");
        assert!(staging.socket_path().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_metadata_is_skipped_not_fatal() {
        let dir = tmpdir("malformed");
        write(&dir.join("contexts/meta/aaa/meta.json"), "{not json");
        write(
            &dir.join("contexts/meta/bbb/meta.json"),
            r#"{"Name":"noendpoint"}"#,
        );
        write(
            &dir.join("contexts/meta/fen/meta.json"),
            r#"{"Name":"good","Endpoints":{"docker":{"Host":"unix:///tmp/d.sock"}}}"#,
        );

        let ctxs = contexts_from_dir(&dir);
        assert_eq!(ctxs.len(), 1, "only the well-formed context survives");
        assert_eq!(ctxs[0].name, "good");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_config_dir_yields_nothing_rather_than_panicking() {
        let ctxs = contexts_from_dir(Path::new("/nonexistent/pj/docker"));
        assert!(ctxs.is_empty());
    }
}
