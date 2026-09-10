//! The `bollard`-backed [`DockerClient`].
//!
//! This is the ONLY module that may name a `bollard` type. Everything crossing
//! its boundary is one of our own `model` types. That containment is what makes
//! the client replaceable and what makes `--no-default-features` compile.
//!
//! It owns its own tokio runtime and blocks internally, so `async` never leaks
//! into the public API and there is no nested-`block_on` hazard: the scan
//! pipeline above is plain synchronous code.

use std::collections::BTreeMap;

use bollard::query_parameters::{
    DataUsageOptions, ListContainersOptionsBuilder, ListImagesOptionsBuilder, ListNetworksOptions,
    ListVolumesOptions,
};
use bollard::Docker;

use crate::error::{Error, Result};
use crate::model::{Bytes, ContainerState, DaemonId, ResourceId, ResourceKind, ResourceSummary};

use super::{detect_runtime, DaemonIdentity, DataUsage, DockerClient};

pub struct BollardClient {
    docker: Docker,
    runtime: tokio::runtime::Runtime,
    endpoint: String,
}

impl BollardClient {
    /// Connect to one endpoint. `unix://` and `npipe://` go over the socket;
    /// anything else is handed to bollard's URL handling and will be flagged
    /// non-local by [`DaemonIdentity`].
    pub fn connect(endpoint: &str) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| Error::Config(format!("could not start async runtime: {e}")))?;

        let docker = if endpoint.starts_with("unix://") || endpoint.starts_with("npipe://") {
            Docker::connect_with_socket(endpoint, 120, bollard::API_DEFAULT_VERSION)
        } else {
            Docker::connect_with_http(endpoint, 120, bollard::API_DEFAULT_VERSION)
        }
        .map_err(|e| map_err(e, endpoint))?;

        Ok(Self {
            docker,
            runtime,
            endpoint: endpoint.to_string(),
        })
    }

    fn block<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.runtime.block_on(fut)
    }
}

impl Drop for BollardClient {
    fn drop(&mut self) {
        // Dropping a runtime from a context where blocking is disallowed panics.
        // We cannot know where our owner is dropped, so never block here.
        //
        // (Replacing the runtime with a fresh throwaway lets us shut the real
        // one down in the background instead of inline.)
        let rt = std::mem::replace(
            &mut self.runtime,
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("trivial current-thread runtime"),
        );
        rt.shutdown_background();
    }
}

fn map_err(e: bollard::errors::Error, endpoint: &str) -> Error {
    let s = e.to_string();
    let lower = s.to_ascii_lowercase();
    if lower.contains("permission denied") {
        Error::PermissionDenied(s)
    } else if lower.contains("no such file")
        || lower.contains("connection refused")
        || lower.contains("could not connect")
        || lower.contains("timed out")
    {
        Error::DaemonUnreachable(format!("{s} (endpoint: {endpoint})"))
    } else {
        Error::Api(s)
    }
}

/// Docker hands us RFC3339 strings. We keep the original for display and a unix
/// timestamp for arithmetic, and simply have no timestamp when it will not
/// parse — never a fabricated one.
fn parse_rfc3339(s: &str) -> Option<i64> {
    // Hand-rolled rather than pulling in chrono for one field. Accepts
    // `YYYY-MM-DDTHH:MM:SS` with an optional fraction and offset.
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }

    // Days from civil epoch (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let mut epoch = days * 86_400 + h * 3_600 + mi * 60 + sec;

    // Trailing numeric offset, if present.
    let tail = &s[19..];
    if let Some(pos) = tail.rfind(['+', '-']) {
        let sign = if tail.as_bytes()[pos] == b'+' { -1 } else { 1 };
        let off = &tail[pos + 1..];
        if let (Some(oh), Some(om)) = (
            off.get(0..2).and_then(|v| v.parse::<i64>().ok()),
            off.get(3..5).and_then(|v| v.parse::<i64>().ok()),
        ) {
            epoch += sign * (oh * 3_600 + om * 60);
        }
    }
    Some(epoch)
}

fn to_map(m: Option<std::collections::HashMap<String, String>>) -> BTreeMap<String, String> {
    m.unwrap_or_default().into_iter().collect()
}

impl DockerClient for BollardClient {
    fn identity(&self) -> Result<DaemonIdentity> {
        let info = self
            .block(self.docker.info())
            .map_err(|e| map_err(e, &self.endpoint))?;
        let version = self
            .block(self.docker.version())
            .map_err(|e| map_err(e, &self.endpoint))?;

        let name = info.name.clone().unwrap_or_default();
        let os_type = info.os_type.clone().unwrap_or_default();
        let kernel = info.kernel_version.clone().unwrap_or_default();

        let swarm_active = info
            .swarm
            .as_ref()
            .and_then(|s| s.local_node_state.as_ref())
            .map(|s| s.to_string() == "active")
            .unwrap_or(false);

        Ok(DaemonIdentity {
            // `/info` ID is the only stable identity. Context names are aliases.
            id: DaemonId(info.id.clone().unwrap_or_else(|| self.endpoint.clone())),
            api_version: version.api_version.clone().unwrap_or_default(),
            server_version: version.version.clone().unwrap_or_default(),
            runtime: detect_runtime(&name, &os_type, &kernel, &self.endpoint),
            data_root: info.docker_root_dir.clone(),
            local: self.endpoint.starts_with("unix://") || self.endpoint.starts_with("npipe://"),
            swarm_active,
        })
    }

    fn list_containers(&self) -> Result<Vec<ResourceSummary>> {
        // `all: true` is mandatory — stopped containers are referrers, and their
        // mounts are the only surviving provenance for anonymous volumes.
        // `size` is deliberately NOT requested: it is the expensive path.
        let opts = ListContainersOptionsBuilder::new().all(true).build();
        let list = self
            .block(self.docker.list_containers(Some(opts)))
            .map_err(|e| map_err(e, &self.endpoint))?;

        Ok(list
            .into_iter()
            .map(|c| {
                let id = c.id.clone().unwrap_or_default();
                let name = c
                    .names
                    .as_ref()
                    .and_then(|n| n.first().cloned())
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| id.chars().take(12).collect());

                let state = c
                    .state
                    .as_ref()
                    .map(|s| ContainerState::parse(s.as_ref()))
                    .unwrap_or(ContainerState::Unknown);

                let mut r = ResourceSummary::new(ResourceKind::Container, id, name);
                r.created_unix = c.created;
                r.labels = to_map(c.labels);
                r.in_use = state.is_live();
                r.state = Some(state);
                r.image_id = c.image_id.map(ResourceId);
                r.size_rw = c.size_rw.filter(|v| *v > 0).map(|v| Bytes(v as u64));
                r.mounts = c
                    .mounts
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|m| m.name)
                    .filter(|n| !n.is_empty())
                    .collect();
                r
            })
            .collect())
    }

    fn list_images(&self) -> Result<Vec<ResourceSummary>> {
        let opts = ListImagesOptionsBuilder::new().all(false).build();
        let list = self
            .block(self.docker.list_images(Some(opts)))
            .map_err(|e| map_err(e, &self.endpoint))?;

        Ok(list
            .into_iter()
            .map(|i| {
                let name = i
                    .repo_tags
                    .iter()
                    .find(|t| *t != "<none>:<none>")
                    .cloned()
                    .unwrap_or_else(|| {
                        i.id.strip_prefix("sha256:")
                            .unwrap_or(&i.id)
                            .chars()
                            .take(12)
                            .collect()
                    });
                let mut r = ResourceSummary::new(ResourceKind::Image, i.id.clone(), name);
                r.created_unix = Some(i.created);
                r.labels = i.labels.into_iter().collect();
                r.size = Some(Bytes(i.size.max(0) as u64));
                r.in_use = i.containers > 0;
                r.repo_tags = i
                    .repo_tags
                    .into_iter()
                    .filter(|t| t != "<none>:<none>")
                    .collect();
                r.repo_digests = i.repo_digests;
                r
            })
            .collect())
    }

    fn list_volumes(&self) -> Result<Vec<ResourceSummary>> {
        let resp = self
            .block(self.docker.list_volumes(None::<ListVolumesOptions>))
            .map_err(|e| map_err(e, &self.endpoint))?;

        Ok(resp
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(|v| {
                let mut r =
                    ResourceSummary::new(ResourceKind::Volume, v.name.clone(), v.name.clone());
                // `CreatedAt` is the ONLY timestamp a volume carries. There is no
                // last-written time anywhere in the API, and CreatedAt diverges
                // from real last-write by months, so it must never be used as a
                // staleness signal on its own.
                r.created_at = v.created_at.clone();
                r.created_unix = v.created_at.as_deref().and_then(parse_rfc3339);
                r.labels = v.labels.into_iter().collect();
                if let Some(u) = v.usage_data {
                    if u.size >= 0 {
                        r.size = Some(Bytes(u.size as u64));
                    }
                    // RefCount is corroboration only. The reference graph is
                    // built from container mounts; a disagreement between the
                    // two downgrades the verdict to Unknown rather than
                    // licensing a deletion.
                    r.in_use = u.ref_count > 0;
                }
                r
            })
            .collect())
    }

    fn list_networks(&self) -> Result<Vec<ResourceSummary>> {
        let list = self
            .block(self.docker.list_networks(None::<ListNetworksOptions>))
            .map_err(|e| map_err(e, &self.endpoint))?;

        Ok(list
            .into_iter()
            .map(|n| {
                let id = n.id.clone().unwrap_or_default();
                let name = n.name.clone().unwrap_or_else(|| id.clone());
                let mut r = ResourceSummary::new(ResourceKind::Network, id, name);
                r.created_at = n.created.clone();
                r.created_unix = n.created.as_deref().and_then(parse_rfc3339);
                r.labels = to_map(n.labels);
                r.in_use = n
                    .containers
                    .as_ref()
                    .map(|c| !c.is_empty())
                    .unwrap_or(false);
                r
            })
            .collect())
    }

    fn data_usage(&self) -> Result<DataUsage> {
        // `None` rather than a `type` filter: see the trait doc. bollard cannot
        // encode the filter, and one unfiltered call returns both halves.
        let usage = self
            .block(self.docker.df(None::<DataUsageOptions>))
            .map_err(|e| map_err(e, &self.endpoint))?;

        let mut volume_sizes = BTreeMap::new();
        for v in usage.volumes.unwrap_or_default() {
            if let Some(u) = v.usage_data {
                if u.size >= 0 {
                    volume_sizes.insert(v.name, Bytes(u.size as u64));
                }
            }
        }

        let records = usage.build_cache.unwrap_or_default();
        let reclaimable: u64 = records
            .iter()
            .filter(|r| !r.in_use.unwrap_or(false) && !r.shared.unwrap_or(false))
            .map(|r| r.size.unwrap_or(0).max(0) as u64)
            .sum();

        Ok(DataUsage {
            volume_sizes,
            build_cache_records: records.len() as u32,
            build_cache_reclaimable: Bytes(reclaimable),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_with_offset() {
        // 2026-05-29T14:55:43+10:00 == 2026-05-29T04:55:43Z
        let a = parse_rfc3339("2026-05-29T14:55:43+10:00").unwrap();
        let b = parse_rfc3339("2026-05-29T04:55:43Z").unwrap();
        assert_eq!(a, b, "offset must be applied");
    }

    #[test]
    fn parses_epoch_reference_points() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2000-01-01T00:00:00Z"), Some(946_684_800));
    }

    #[test]
    fn rejects_junk_rather_than_fabricating_a_time() {
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("not-a-date"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-00-01T00:00:00Z"), None);
    }
}
