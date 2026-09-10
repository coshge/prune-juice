//! The Docker seam.
//!
//! Two traits, deliberately separate:
//!
//! * [`DockerClient`] — read-only. Everything the scan needs.
//! * [`DockerMutate`] — destructive. Nothing in M1 implements it.
//!
//! Splitting them is the **deletion firewall**: the replay client used by tests
//! implements `DockerClient` only, so no unit or fixture test can delete
//! anything even by accident. It is a structural guarantee rather than a
//! convention.
//!
//! `bollard` types never escape this module. That rule is what lets the client
//! be replaced (~600 lines over `hyperlocal` and named pipes if bollard ever
//! becomes a problem) without touching a line of logic elsewhere.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{Bytes, DaemonId, ResourceSummary, RuntimeFlavor};

/// The result of one `/system/df`.
#[derive(Clone, Debug, Default)]
pub struct DataUsage {
    pub volume_sizes: BTreeMap<String, Bytes>,
    pub build_cache_records: u32,
    /// Only records that are neither in use nor shared with a live build.
    /// Deliberately the conservative figure: `docker buildx du` reports the
    /// larger "reclaimable" number, and over-promising reclaim is a trust bug.
    pub build_cache_reclaimable: Bytes,
}

pub mod context;

#[cfg(feature = "bollard-client")]
pub mod bollard_client;

/// Who we are talking to. Printed in the header of every run, and used to key
/// the index — **never** the context name, since two contexts can be aliases
/// for one engine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonIdentity {
    pub id: DaemonId,
    pub api_version: String,
    pub server_version: String,
    pub runtime: RuntimeFlavor,
    /// `DockerRootDir` from `/info`. Readable only on native Linux; inside a VM
    /// this path exists in the guest, not on the host.
    pub data_root: Option<String>,
    /// True when the endpoint is a local unix socket or named pipe. A remote
    /// daemon is refused for destructive work without an explicit override.
    pub local: bool,
    /// Swarm or Kubernetes activity means resources may be referenced by
    /// something we do not model, which collapses affected kinds to `Unknown`.
    pub swarm_active: bool,
}

/// Read-only access to a daemon. Blocking and object-safe on purpose: `async`
/// never appears in this crate's public API, so `tokio` stays an implementation
/// detail of `bollard_client` rather than an architectural commitment.
pub trait DockerClient: Send + Sync {
    fn identity(&self) -> Result<DaemonIdentity>;

    /// All containers in **every** state. Stopped containers are referrers too,
    /// and their `.Mounts` is the only surviving link from an anonymous volume
    /// to a project.
    fn list_containers(&self) -> Result<Vec<ResourceSummary>>;

    fn list_images(&self) -> Result<Vec<ResourceSummary>>;

    /// Volumes with labels and `CreatedAt`, without sizes. This is the ~15 ms
    /// call; sizes cost two orders of magnitude more and are fetched separately.
    fn list_volumes(&self) -> Result<Vec<ResourceSummary>>;

    fn list_networks(&self) -> Result<Vec<ResourceSummary>>;

    /// Volume sizes and build-cache totals, via `/system/df`.
    ///
    /// ONE call, deliberately. The `?type=` filter that would let us ask for
    /// volumes alone is unusable through bollard — its `DataUsageOptions._type`
    /// is a `Vec<String>`, and `serde_urlencoded` cannot encode a sequence
    /// ("Unable to URLEncode: unsupported value"). Since an unfiltered `df`
    /// returns volumes and build cache together, asking once costs about the
    /// same as two filtered calls would have and is simpler.
    ///
    /// Expensive regardless — it `du`s every volume directory, and is reported
    /// at minutes on some setups. Must never sit on a first-paint path.
    fn data_usage(&self) -> Result<DataUsage>;
}

/// What a probe container reports back about one volume.
///
/// Deliberately raw: classification happens in `crate::probe`, so the same
/// rules apply whether the bytes were read natively or through a container.
#[derive(Clone, Debug, Default)]
pub struct RawProbe {
    /// Top-level entries, capped.
    pub entries: Vec<String>,
    /// Files found, capped — a floor, not a total.
    pub file_count: u64,
    /// Newest mtime seen. Sampled rather than exhaustive on large volumes;
    /// [`Self::mtime_sampled`] says which.
    pub newest_mtime: Option<i64>,
    pub mtime_sampled: bool,
}

/// Reading volume contents by mounting them into a throwaway container.
///
/// Needed on every VM-backed runtime — Docker Desktop, Colima, Podman machine —
/// where the data root exists only inside the guest and cannot be read from the
/// host. Without it those runtimes get no volume reclamation at all.
///
/// A third trait rather than part of [`DockerClient`] because it *does* create
/// state, and separate from [`DockerMutate`] because it destroys nothing: the
/// volume is mounted read-only, the container has no network and a read-only
/// root, and it is removed afterwards.
pub trait DockerProbe: Send + Sync {
    /// A locally-present image with a shell, if there is one. `None` means the
    /// probe cannot run, which is reported rather than worked around.
    fn probe_image(&self) -> Option<String>;

    /// Read several volumes in one container. Batching matters: a container per
    /// volume would mean hundreds of spawns per scan.
    fn probe_volumes(&self, volumes: &[String]) -> Result<BTreeMap<String, RawProbe>>;

    /// Stream a volume's entire contents out as a tar archive.
    ///
    /// **The container is created but never started.** Booting an engine
    /// against a real data directory triggers crash recovery and catalog
    /// writes — mutating the very thing being preserved. A stopped container is
    /// only a mount holder, and `GET /containers/{id}/archive` reads through it
    /// without executing anything.
    ///
    /// Returns the number of bytes written to `out`.
    fn dump_volume(&self, name: &str, out: &mut dyn std::io::Write) -> Result<u64>;
}

/// Destructive operations, quarantined behind their own trait.
///
/// Deliberately unimplemented in M1 — the binary shipped at that milestone has
/// no code path that can remove anything.
pub trait DockerMutate: Send + Sync {
    fn remove_volume(&self, name: &str) -> Result<()>;
    fn remove_image(&self, id: &str) -> Result<()>;
    fn remove_container(&self, id: &str) -> Result<()>;
    fn remove_network(&self, id: &str) -> Result<()>;
    fn prune_build_cache(&self, keep_newer_than_secs: u64) -> Result<Bytes>;

    /// Recreate a volume from a tar archive, restoring its labels too.
    ///
    /// Refuses if a volume of that name already exists: a restore must never
    /// silently merge into live data.
    fn restore_volume(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        tar: Vec<u8>,
    ) -> Result<()>;
}

/// Infer the runtime from `/info` plus a couple of filesystem probes.
///
/// Selection is at **runtime**, not `#[cfg]` — one macOS machine can have both
/// Docker Desktop and OrbStack installed, and the answer depends on which
/// daemon the context actually points at.
pub fn detect_runtime(name: &str, os_type: &str, kernel: &str, endpoint: &str) -> RuntimeFlavor {
    let n = name.to_ascii_lowercase();
    let k = kernel.to_ascii_lowercase();

    if endpoint.starts_with("tcp://") || endpoint.starts_with("ssh://") {
        return RuntimeFlavor::Remote;
    }
    if n.contains("orbstack") || k.contains("orbstack") {
        return RuntimeFlavor::OrbStack;
    }
    if n.contains("colima") {
        return RuntimeFlavor::Colima;
    }
    if n.contains("rancher") {
        return RuntimeFlavor::RancherDesktop;
    }
    if n.contains("podman") {
        return RuntimeFlavor::Podman;
    }
    if n.contains("docker-desktop") || n.contains("docker desktop") {
        return if cfg!(target_os = "windows") {
            RuntimeFlavor::DockerDesktopWindows
        } else {
            RuntimeFlavor::DockerDesktopMac
        };
    }
    if os_type == "linux" && cfg!(target_os = "linux") {
        // A Linux binary talking to a Linux daemon over a local socket: unless
        // we are inside WSL, this is a native install that reclaims disk
        // immediately.
        return RuntimeFlavor::NativeLinux;
    }
    RuntimeFlavor::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_endpoints_win_over_every_name_hint() {
        // A staging daemon reached over TCP must never be mistaken for a local
        // engine, whatever it calls itself.
        assert_eq!(
            detect_runtime("orbstack", "linux", "orbstack", "tcp://10.0.0.5:2375"),
            RuntimeFlavor::Remote
        );
        assert_eq!(
            detect_runtime("docker-desktop", "linux", "x", "ssh://build-box"),
            RuntimeFlavor::Remote
        );
    }

    #[test]
    fn orbstack_detected_from_name_or_kernel() {
        assert_eq!(
            detect_runtime("orbstack", "linux", "6.0", "unix:///x.sock"),
            RuntimeFlavor::OrbStack
        );
        assert_eq!(
            detect_runtime("somehost", "linux", "7.0.14-orbstack-abc", "unix:///x.sock"),
            RuntimeFlavor::OrbStack
        );
    }

    #[test]
    fn unknown_is_returned_rather_than_a_confident_guess() {
        let f = detect_runtime("weird-host", "linux", "5.0", "unix:///var/run/docker.sock");
        assert!(matches!(
            f,
            RuntimeFlavor::Unknown | RuntimeFlavor::NativeLinux
        ));
    }
}
