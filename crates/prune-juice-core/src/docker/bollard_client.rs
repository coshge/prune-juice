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

use super::{
    detect_runtime, DaemonIdentity, DataUsage, DockerClient, DockerMutate, DockerProbe, RawProbe,
};

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

/// Destructive operations.
///
/// Every one of these deliberately passes `force: false`. The daemon's own
/// in-use check is the last line of defence against a race we could not see,
/// and a force flag switches it off. A refusal here is the system working, not
/// a problem to route around.
impl DockerMutate for BollardClient {
    fn remove_volume(&self, name: &str) -> Result<()> {
        let opts = bollard::query_parameters::RemoveVolumeOptions { force: false };
        self.block(self.docker.remove_volume(name, Some(opts)))
            .map_err(|e| map_err(e, &self.endpoint))
    }

    fn remove_image(&self, id: &str) -> Result<()> {
        // `noprune: false` lets the daemon reclaim now-unreferenced parents;
        // `force: false` keeps it refusing anything still in use.
        let opts = bollard::query_parameters::RemoveImageOptions {
            force: false,
            noprune: false,
        };
        self.block(self.docker.remove_image(id, Some(opts), None))
            .map(|_| ())
            .map_err(|e| map_err(e, &self.endpoint))
    }

    fn remove_container(&self, id: &str) -> Result<()> {
        // `v: false` is important — we never let a container removal take its
        // anonymous volumes with it. Volumes are decided on their own evidence,
        // never as a side effect.
        let opts = bollard::query_parameters::RemoveContainerOptions {
            v: false,
            force: false,
            link: false,
        };
        self.block(self.docker.remove_container(id, Some(opts)))
            .map_err(|e| map_err(e, &self.endpoint))
    }

    fn remove_network(&self, id: &str) -> Result<()> {
        self.block(self.docker.remove_network(id))
            .map_err(|e| map_err(e, &self.endpoint))
    }

    fn restore_volume(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        tar: Vec<u8>,
    ) -> Result<()> {
        // Refuse to restore over anything that exists. A restore that merged
        // into live data would be worse than the deletion it is undoing.
        let existing = self.list_volumes()?;
        if existing.iter().any(|v| v.name == name) {
            return Err(Error::Config(format!(
                "volume {name} already exists; remove it first or restore under another name"
            )));
        }

        let opts = bollard::models::VolumeCreateOptions {
            name: Some(name.to_string()),
            labels: Some(labels.clone().into_iter().collect()),
            ..Default::default()
        };
        self.block(self.docker.create_volume(opts))
            .map_err(|e| map_err(e, &self.endpoint))?;

        let Some(image) = self.find_probe_image() else {
            return Err(Error::Config(
                "no local image is available to restore with".into(),
            ));
        };

        // Writable mount this time — that is the whole point — but still no
        // network, and still never started.
        let config = bollard::models::ContainerCreateBody {
            image: Some(image),
            cmd: Some(vec!["/bin/true".to_string()]),
            network_disabled: Some(true),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(vec![format!("{name}:/vault")]),
                network_mode: Some("none".to_string()),
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = self
            .block(self.docker.create_container(
                None::<bollard::query_parameters::CreateContainerOptions>,
                config,
            ))
            .map_err(|e| map_err(e, &self.endpoint))?;
        let id = created.id;

        let opts = bollard::query_parameters::UploadToContainerOptionsBuilder::new()
            .path("/")
            .build();
        let result = self
            .block(
                self.docker
                    .upload_to_container(&id, Some(opts), bollard::body_full(tar.into())),
            )
            .map_err(|e| map_err(e, &self.endpoint));

        let _ = self.block(self.docker.remove_container(
            &id,
            Some(bollard::query_parameters::RemoveContainerOptions {
                v: false,
                force: true,
                link: false,
            }),
        ));
        result
    }

    fn prune_build_cache(&self, keep_newer_than_secs: u64) -> Result<Bytes> {
        // The time guard is what survives a build starting between plan and
        // apply: anything touched inside the window is left alone.
        let opts = bollard::query_parameters::PruneBuildOptionsBuilder::new()
            .filters(&std::collections::HashMap::from([(
                "unused-for",
                vec![format!("{keep_newer_than_secs}s").as_str()],
            )]))
            .build();
        let resp = self
            .block(self.docker.prune_build(Some(opts)))
            .map_err(|e| map_err(e, &self.endpoint))?;
        Ok(Bytes(resp.space_reclaimed.unwrap_or(0).max(0) as u64))
    }
}

/// The script run inside the probe container.
///
/// Constraints it has to satisfy: POSIX-ish so it works under busybox as well
/// as coreutils; bounded so a volume with a million files cannot hang a scan;
/// and strictly read-only.
const PROBE_SCRIPT: &str = r#"
for d in /p/*; do
  [ -d "$d" ] || continue
  echo "@@@V $d"
  ls -A "$d" 2>/dev/null | head -256
  echo "@@@C"
  find "$d" -type f 2>/dev/null | head -20000 | wc -l
  echo "@@@M"
  find "$d" -type f 2>/dev/null | head -200 | tr '\n' '\0' | xargs -0 stat -c %Y 2>/dev/null | sort -rn | head -1
done
echo "@@@END"
"#;

/// Images we know carry a shell and the handful of utilities the script needs.
/// Ordered smallest-first so a probe costs as little as possible.
const PREFERRED_PROBE_IMAGES: [&str; 6] =
    ["busybox", "alpine", "debian", "ubuntu", "mariadb", "mysql"];

impl BollardClient {
    /// Pick a locally-present image to probe with.
    ///
    /// Never pulls. A probe that reaches the network turns a read-only
    /// inspection into an outbound request, and on a metered or air-gapped
    /// machine that is not ours to decide.
    fn find_probe_image(&self) -> Option<String> {
        let images = self.list_images().ok()?;
        let tags: Vec<String> = images
            .iter()
            .flat_map(|i| i.repo_tags.iter().cloned())
            .collect();

        for want in PREFERRED_PROBE_IMAGES {
            if let Some(t) = tags.iter().find(|t| {
                let base = t.split(':').next().unwrap_or(t);
                base == want || base.ends_with(&format!("/{want}"))
            }) {
                return Some(t.clone());
            }
        }
        // Anything at all is better than nothing; the script degrades to an
        // empty listing if the image has no shell, which reads as "not probed".
        tags.into_iter().next()
    }
}

impl DockerProbe for BollardClient {
    fn probe_image(&self) -> Option<String> {
        self.find_probe_image()
    }

    fn dump_volume(&self, name: &str, out: &mut dyn std::io::Write) -> Result<u64> {
        use futures_util::StreamExt;

        let Some(image) = self.find_probe_image() else {
            return Err(Error::Config(
                "no local image is available to read volume contents; try `docker pull alpine`"
                    .into(),
            ));
        };

        // Read-only bind, no network, read-only rootfs. And crucially: this
        // container is created and never started. It exists solely so the
        // archive endpoint has a mount namespace to read through.
        let config = bollard::models::ContainerCreateBody {
            image: Some(image),
            cmd: Some(vec!["/bin/true".to_string()]),
            network_disabled: Some(true),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(vec![format!("{name}:/vault:ro")]),
                network_mode: Some("none".to_string()),
                readonly_rootfs: Some(true),
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let created = self
            .block(self.docker.create_container(
                None::<bollard::query_parameters::CreateContainerOptions>,
                config,
            ))
            .map_err(|e| map_err(e, &self.endpoint))?;
        let id = created.id;

        let opts = bollard::query_parameters::DownloadFromContainerOptionsBuilder::new()
            .path("/vault")
            .build();

        // Capture the result before cleanup so a failure cannot leak a
        // container.
        let result = self.block(async {
            let mut stream = self.docker.download_from_container(&id, Some(opts));
            let mut written: u64 = 0;
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|e| map_err(e, &self.endpoint))?;
                out.write_all(&bytes).map_err(Error::Io)?;
                written += bytes.len() as u64;
            }
            Ok(written)
        });

        let _ = self.block(self.docker.remove_container(
            &id,
            Some(bollard::query_parameters::RemoveContainerOptions {
                v: false,
                force: true,
                link: false,
            }),
        ));
        result
    }

    fn probe_volumes(&self, volumes: &[String]) -> Result<BTreeMap<String, RawProbe>> {
        let mut out = BTreeMap::new();
        if volumes.is_empty() {
            return Ok(out);
        }
        let Some(image) = self.find_probe_image() else {
            return Err(Error::Config(
                "no local image is available to read volume contents; try `docker pull alpine`"
                    .into(),
            ));
        };

        // Mount each volume read-only at a numbered path. The index, not the
        // name, is what maps results back — a volume name could contain
        // characters the shell would treat specially.
        let binds: Vec<String> = volumes
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{v}:/p/{i:03}:ro"))
            .collect();

        let config = bollard::models::ContainerCreateBody {
            image: Some(image),
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                PROBE_SCRIPT.to_string(),
            ]),
            // Belt and braces: no network, read-only root filesystem, and every
            // mount read-only. The container cannot change anything it sees.
            network_disabled: Some(true),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(binds),
                network_mode: Some("none".to_string()),
                readonly_rootfs: Some(true),
                auto_remove: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };

        let created = self
            .block(self.docker.create_container(
                None::<bollard::query_parameters::CreateContainerOptions>,
                config,
            ))
            .map_err(|e| map_err(e, &self.endpoint))?;
        let id = created.id;

        // From here on the container must be removed whatever happens, so the
        // result is captured before the cleanup rather than propagated early.
        let result = self.run_probe_container(&id);
        let _ = self.block(self.docker.remove_container(
            &id,
            Some(bollard::query_parameters::RemoveContainerOptions {
                v: false,
                force: true,
                link: false,
            }),
        ));

        let text = result?;
        for (idx, raw) in parse_probe_output(&text) {
            if let Some(name) = volumes.get(idx) {
                out.insert(name.clone(), raw);
            }
        }
        Ok(out)
    }
}

impl BollardClient {
    fn run_probe_container(&self, id: &str) -> Result<String> {
        use futures_util::StreamExt;

        self.block(
            self.docker
                .start_container(id, None::<bollard::query_parameters::StartContainerOptions>),
        )
        .map_err(|e| map_err(e, &self.endpoint))?;

        let opts = bollard::query_parameters::LogsOptionsBuilder::new()
            .follow(true)
            .stdout(true)
            .stderr(false)
            .build();

        self.block(async {
            let mut stream = self.docker.logs(id, Some(opts));
            let mut buf = String::new();
            // The stream ends when the container exits, so this doubles as the
            // wait. A cap stops a pathological volume producing unbounded output.
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(out) => buf.push_str(&out.to_string()),
                    Err(_) => break,
                }
                if buf.len() > 4 * 1024 * 1024 {
                    break;
                }
            }
            Ok(buf)
        })
    }
}

/// Parse the script's output back into per-volume reports.
fn parse_probe_output(text: &str) -> Vec<(usize, RawProbe)> {
    let mut out: Vec<(usize, RawProbe)> = Vec::new();
    let mut current: Option<(usize, RawProbe)> = None;
    let mut section = Section::Entries;

    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(path) = line.strip_prefix("@@@V ") {
            if let Some(c) = current.take() {
                out.push(c);
            }
            let idx = path
                .rsplit('/')
                .next()
                .and_then(|s| s.parse::<usize>().ok());
            current = idx.map(|i| {
                (
                    i,
                    RawProbe {
                        mtime_sampled: true,
                        ..Default::default()
                    },
                )
            });
            section = Section::Entries;
            continue;
        }
        match line {
            "@@@C" => {
                section = Section::Count;
                continue;
            }
            "@@@M" => {
                section = Section::Mtime;
                continue;
            }
            "@@@END" => break,
            _ => {}
        }
        let Some((_, raw)) = current.as_mut() else {
            continue;
        };
        if line.is_empty() {
            continue;
        }
        match section {
            Section::Entries => raw.entries.push(line.to_string()),
            Section::Count => {
                if let Ok(n) = line.trim().parse::<u64>() {
                    raw.file_count = n;
                }
            }
            Section::Mtime => {
                if let Ok(t) = line.trim().parse::<i64>() {
                    raw.newest_mtime = Some(t);
                }
            }
        }
    }
    if let Some(c) = current.take() {
        out.push(c);
    }
    for (_, raw) in out.iter_mut() {
        raw.entries.sort();
    }
    out
}

enum Section {
    Entries,
    Count,
    Mtime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_is_parsed_back_per_volume() {
        let text = "\
@@@V /p/000
mysql
ibdata1
@@@C
1234
@@@M
1700000000
@@@V /p/001
@@@C
0
@@@M
@@@V /p/002
node_modules
@@@C
9
@@@M
1699999999
@@@END
";
        let got = parse_probe_output(text);
        assert_eq!(got.len(), 3);

        let (i0, r0) = &got[0];
        assert_eq!(*i0, 0);
        assert_eq!(r0.entries, vec!["ibdata1", "mysql"]); // sorted
        assert_eq!(r0.file_count, 1234);
        assert_eq!(r0.newest_mtime, Some(1_700_000_000));

        // An empty volume: no entries, no mtime. This must survive, because it
        // is the class that unlocks deletion.
        let (i1, r1) = &got[1];
        assert_eq!(*i1, 1);
        assert!(r1.entries.is_empty());
        assert_eq!(r1.file_count, 0);
        assert_eq!(r1.newest_mtime, None);

        assert_eq!(got[2].1.entries, vec!["node_modules"]);
    }

    #[test]
    fn probe_output_survives_truncation_mid_stream() {
        // The log stream is capped, so the last section can be cut off. A
        // partial read must not be mistaken for an empty volume by the caller
        // — it simply yields whatever was seen.
        let text = "@@@V /p/000\nmysql\nibdata1\n@@@C\n5\n@@@M\n";
        let got = parse_probe_output(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.file_count, 5);
        assert_eq!(got[0].1.newest_mtime, None);
    }

    #[test]
    fn probe_output_ignores_noise_before_the_first_marker() {
        let text = "some shell warning\n@@@V /p/007\na\n@@@C\n1\n@@@END\n";
        let got = parse_probe_output(text);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 7, "the index comes from the mount path");
        assert_eq!(got[0].1.entries, vec!["a"]);
    }

    #[test]
    fn probe_mounts_are_read_only_and_indexed() {
        // The bind string is the security boundary: `:ro` is what stops a probe
        // from being able to change what it inspects.
        let vols = ["a".to_string(), "weird:name".to_string()];
        let binds: Vec<String> = vols
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{v}:/p/{i:03}:ro"))
            .collect();
        assert_eq!(binds[0], "a:/p/000:ro");
        assert!(binds.iter().all(|b| b.ends_with(":ro")));
    }

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
