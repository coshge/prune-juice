//! The scan pipeline.
//!
//! Read-only, always. There is no deletion code path anywhere in this
//! milestone's binary — destructive operations live behind [`DockerMutate`],
//! which nothing implements yet.
//!
//! [`DockerMutate`]: crate::docker::DockerMutate

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::docker::{DaemonIdentity, DataUsage, DockerClient, DockerProbe};
use crate::error::Result;
use crate::event::{Cancel, Event, EventSink, Phase};
use crate::index::Index;
use crate::model::{
    Claim, Confidence, Liveness, ResourceKind, ResourceSummary, SizeSource, Totals,
};
use crate::probe::{ContentReport, VolumeAccess};
use crate::providers::{
    best_claim, claims_for, declared_by_live_project, is_orphan_candidate, Catalog,
};

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Directories to look in for projects that have no containers.
    pub project_roots: Vec<PathBuf>,
    /// Fetch volume sizes. This is the expensive call — minutes on some setups —
    /// so it is opt-in and must never sit on a first-paint path.
    pub with_sizes: bool,
    /// Read volume contents to classify them. Strictly read-only, and cheap
    /// where the data root is reachable: 259 volumes in under half a second.
    /// Without it no volume can ever be proven safe to delete.
    pub probe_volumes: bool,
    /// Ignore host access to the data root and always go through a container.
    ///
    /// Exists so the container path — the only one available on Docker Desktop
    /// — can be exercised and cross-checked on a machine where the native path
    /// also works. Not something a user needs.
    pub force_container_probe: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            project_roots: Vec::new(),
            with_sizes: true,
            probe_volumes: true,
            force_container_probe: false,
        }
    }
}

/// One resource with everything we concluded about it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attributed {
    pub resource: ResourceSummary,
    pub claims: Vec<Claim>,
    /// Winning claim's project name, if any.
    pub owner: Option<String>,
    pub confidence: Option<Confidence>,
    pub liveness: Option<Liveness>,
    /// A `Strong`+ claim on an absent project, with nothing live contesting it.
    pub orphan_candidate: bool,
    /// No claim reached `Strong`, or there is no claim at all. Never offered
    /// for deletion — the tool says "I don't know" rather than guessing.
    pub unattributed: bool,
    /// What the volume actually contains. `None` means not probed — which is
    /// never treated as "empty".
    pub content: Option<ContentReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanReport {
    pub daemon: DaemonIdentity,
    pub context: String,
    pub totals: Totals,
    pub resources: Vec<Attributed>,
    pub projects_known: usize,
    pub duration_ms: u64,
    /// True when sizes were skipped or a deadline cut the scan short.
    pub stale: bool,
    /// Non-fatal problems. Kept on the report as well as emitted, so a caller
    /// that discards events still sees them.
    pub warnings: Vec<String>,
}

impl ScanReport {
    pub fn of_kind(&self, kind: ResourceKind) -> impl Iterator<Item = &Attributed> {
        self.resources
            .iter()
            .filter(move |a| a.resource.kind == kind)
    }

    pub fn orphan_candidates(&self) -> impl Iterator<Item = &Attributed> {
        self.resources.iter().filter(|a| a.orphan_candidate)
    }

    pub fn unattributed(&self) -> impl Iterator<Item = &Attributed> {
        self.resources.iter().filter(|a| a.unattributed)
    }
}

/// Wall-clock seconds. The scan already performs IO, so reading a clock here
/// costs nothing in testability — the *classifier* is the part that takes time
/// as an input, and it still does.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Push a warning unless an identical one is already there.
fn warnings_once(warnings: &mut Vec<String>, msg: String) {
    if !warnings.contains(&msg) {
        warnings.push(msg);
    }
}

pub struct Scanner<'a> {
    client: &'a dyn DockerClient,
    /// Used only when the data root cannot be read from the host — which is
    /// every VM-backed runtime, Docker Desktop included.
    probe: Option<&'a dyn DockerProbe>,
    /// Remembers what Docker forgets. Without it, provenance dies with the
    /// container that carried it, and "absent for how long?" has no answer.
    index: Option<&'a Index>,
}

impl<'a> Scanner<'a> {
    pub fn new(client: &'a dyn DockerClient) -> Self {
        Self {
            client,
            probe: None,
            index: None,
        }
    }

    /// Supply a container-based probe for runtimes that hide their data root.
    pub fn with_probe(client: &'a dyn DockerClient, probe: &'a dyn DockerProbe) -> Self {
        Self {
            client,
            probe: Some(probe),
            index: None,
        }
    }

    /// Give the scan a memory.
    pub fn with_index(mut self, index: &'a Index) -> Self {
        self.index = Some(index);
        self
    }

    /// A scan with an index but no container probe. Used by tests and by
    /// runtimes where the host can read the data root directly.
    pub fn with_index_only(client: &'a dyn DockerClient, index: &'a Index) -> Self {
        Self {
            client,
            probe: None,
            index: Some(index),
        }
    }

    pub fn scan(
        &self,
        context: &str,
        opts: &ScanOptions,
        sink: Arc<dyn EventSink>,
        cancel: &Cancel,
    ) -> Result<ScanReport> {
        let started = Instant::now();

        let daemon = self.client.identity()?;
        sink.emit(Event::ScanStarted {
            daemon: daemon.id.clone(),
            context: context.to_string(),
            runtime: daemon.runtime,
            api_version: daemon.api_version.clone(),
        });
        cancel.check()?;

        // --- listing -----------------------------------------------------
        sink.emit(Event::Phase {
            phase: Phase::Listing,
            done: 0,
            total: Some(4),
        });

        let containers = self.client.list_containers()?;
        sink.emit(Event::Phase {
            phase: Phase::Listing,
            done: 1,
            total: Some(4),
        });
        cancel.check()?;

        let images = self.client.list_images()?;
        sink.emit(Event::Phase {
            phase: Phase::Listing,
            done: 2,
            total: Some(4),
        });
        cancel.check()?;

        let mut volumes = self.client.list_volumes()?;
        sink.emit(Event::Phase {
            phase: Phase::Listing,
            done: 3,
            total: Some(4),
        });
        cancel.check()?;

        let networks = self.client.list_networks()?;
        sink.emit(Event::Phase {
            phase: Phase::Listing,
            done: 4,
            total: Some(4),
        });
        cancel.check()?;

        // --- the reference graph -----------------------------------------
        //
        // Built from container mounts in EVERY state, not from the daemon's
        // RefCount. A stopped container is a referrer, and its mount list is
        // the only surviving link from an anonymous volume to a project.
        let mut mounted: BTreeSet<String> = BTreeSet::new();
        for c in &containers {
            for m in &c.mounts {
                mounted.insert(m.clone());
            }
        }

        // --- sizes -------------------------------------------------------
        //
        // One `df`. It is the single most fragile call in the API — minutes on
        // some setups — so a failure degrades the report rather than aborting
        // the scan.
        let mut stale = !opts.with_sizes;
        let mut warnings: Vec<String> = Vec::new();
        let mut usage = DataUsage::default();

        if opts.with_sizes {
            sink.emit(Event::Phase {
                phase: Phase::Sizing,
                done: 0,
                total: Some(1),
            });
            match self.client.data_usage() {
                Ok(u) => {
                    for v in &mut volumes {
                        if let Some(b) = u.volume_sizes.get(&v.name) {
                            v.size = Some(*b);
                            sink.emit(Event::SizeUpdated {
                                id: v.id.clone(),
                                bytes: *b,
                                source: SizeSource::DaemonDf,
                            });
                        }
                    }
                    usage = u;
                }
                Err(e) => {
                    stale = true;
                    let msg = format!("volume and build-cache sizes could not be measured: {e}");
                    sink.emit(Event::Warning {
                        code: "sizes_unavailable".into(),
                        message: msg.clone(),
                        resource: None,
                    });
                    warnings.push(msg);
                }
            }
            sink.emit(Event::Phase {
                phase: Phase::Sizing,
                done: 1,
                total: Some(1),
            });
        }
        cancel.check()?;

        // --- content probe -------------------------------------------------
        //
        // Labels say whose a volume is; only the contents say what it is. A
        // volume that is not probed stays unprovable — never assumed empty,
        // because assuming empty is how you delete a database.
        let mut contents: BTreeMap<String, ContentReport> = BTreeMap::new();
        if opts.probe_volumes {
            let access = VolumeAccess::detect(daemon.runtime, daemon.data_root.as_deref());
            if access.is_available() && !opts.force_container_probe {
                // Fast path: the data root is readable from the host, so no
                // container is involved at all.
                for (i, v) in volumes.iter().enumerate() {
                    if let Some(r) = access.probe_shallow(&v.name) {
                        contents.insert(v.name.clone(), r);
                    }
                    if i % 32 == 0 {
                        cancel.check()?;
                        sink.emit(Event::Phase {
                            phase: Phase::Probing,
                            done: i as u32,
                            total: Some(volumes.len() as u32),
                        });
                    }
                }
            } else if let Some(prober) = self.probe {
                // VM-backed runtime — Docker Desktop, Colima, Podman machine.
                // The bytes exist only inside the guest, so the only way to
                // read them is to mount them into a container.
                //
                // Only volumes with no referrer are worth the trouble: anything
                // a container holds is Protected regardless of contents, and on
                // a typical machine that cuts hundreds of candidates to dozens.
                let candidates: Vec<String> = volumes
                    .iter()
                    .filter(|v| !mounted.contains(&v.name) && !v.in_use)
                    .map(|v| v.name.clone())
                    .collect();

                if candidates.is_empty() {
                    // Nothing to look at; not a failure.
                } else if prober.probe_image().is_none() {
                    let msg = "no local image is available to read volume contents on this runtime;                                `docker pull alpine` once and re-run"
                        .to_string();
                    sink.emit(Event::Warning {
                        code: "probe_image_missing".into(),
                        message: msg.clone(),
                        resource: None,
                    });
                    warnings.push(msg);
                } else {
                    // Batched: a container per volume would mean hundreds of
                    // spawns per scan.
                    const BATCH: usize = 25;
                    let total = candidates.len() as u32;
                    let mut done = 0u32;
                    for chunk in candidates.chunks(BATCH) {
                        cancel.check()?;
                        match prober.probe_volumes(chunk) {
                            Ok(map) => {
                                for (name, raw) in map {
                                    contents.insert(name, crate::probe::from_raw(&raw));
                                }
                            }
                            Err(e) => {
                                let msg = format!("a volume probe failed: {e}");
                                sink.emit(Event::Warning {
                                    code: "probe_failed".into(),
                                    message: msg.clone(),
                                    resource: None,
                                });
                                warnings.push(msg);
                                break;
                            }
                        }
                        done += chunk.len() as u32;
                        sink.emit(Event::Phase {
                            phase: Phase::Probing,
                            done,
                            total: Some(total),
                        });
                    }
                }
            } else {
                let msg = format!(
                    "volume contents are unreadable on this runtime ({:?}) and no probe was supplied, \
                     so no volume can be proven safe to delete",
                    daemon.runtime
                );
                sink.emit(Event::Warning {
                    code: "probe_unavailable".into(),
                    message: msg.clone(),
                    resource: None,
                });
                warnings.push(msg);
            }
        }
        cancel.check()?;
        // --- attribution -------------------------------------------------
        sink.emit(Event::Phase {
            phase: Phase::Attributing,
            done: 0,
            total: None,
        });

        let mut catalog = Catalog::harvest(&containers);
        if !opts.project_roots.is_empty() {
            catalog.add_disk_projects(&opts.project_roots);
        }

        // Write down what we saw BEFORE anything is judged, let alone deleted.
        // A container's labels and mounts are the only place an anonymous
        // volume's provenance lives, so recording has to come first.
        let now_unix = now_unix();
        let mut project_absences: BTreeMap<String, u32> = BTreeMap::new();
        let mut remembered: BTreeMap<String, crate::index::RememberedOwner> = BTreeMap::new();
        if let Some(index) = self.index {
            let all: Vec<ResourceSummary> = containers
                .iter()
                .chain(images.iter())
                .chain(volumes.iter())
                .chain(networks.iter())
                .cloned()
                .collect();
            if let Err(e) = index
                .begin_scan(&daemon.id, now_unix)
                .and_then(|_| index.record_scan(&daemon.id, &all, now_unix))
            {
                warnings.push(format!("the index could not be updated: {e}"));
            }

            let observed: Vec<(String, String, bool)> = catalog
                .projects()
                .filter_map(|p| {
                    p.root
                        .as_ref()
                        .map(|r| (r.to_string_lossy().into_owned(), p.name.clone(), r.is_dir()))
                })
                .collect();
            if let Err(e) = index.record_projects(&daemon.id, &observed, now_unix) {
                warnings.push(format!("project history could not be updated: {e}"));
            }
            for (path, _, _) in &observed {
                if let Ok(Some(h)) = index.project_history(&daemon.id, path) {
                    project_absences.insert(path.clone(), h.absent_scans);
                }
            }
            remembered = index.all_historical_owners(&daemon.id).unwrap_or_default();
        }

        for p in catalog.projects() {
            sink.emit(Event::ProjectFound {
                project: crate::model::ProjectSummary {
                    id: p.id(),
                    name: p.name.clone(),
                    provider: p.provider,
                    root: p.root.clone(),
                    liveness: crate::providers::liveness_of(p.root.as_ref()),
                },
            });
        }

        let mut totals = Totals {
            containers: containers.len() as u32,
            images: images.len() as u32,
            volumes: volumes.len() as u32,
            networks: networks.len() as u32,
            build_cache_records: usage.build_cache_records,
            build_cache_bytes: usage.build_cache_reclaimable,
            ..Default::default()
        };
        totals.volume_bytes = volumes.iter().filter_map(|v| v.size).sum();
        totals.image_bytes = images.iter().filter_map(|i| i.size).sum();

        let mut resources =
            Vec::with_capacity(containers.len() + images.len() + volumes.len() + networks.len());

        for mut r in containers
            .into_iter()
            .chain(images)
            .chain(volumes)
            .chain(networks)
        {
            // A volume mounted by ANY container — running or long dead — is
            // referenced. The daemon's RefCount only counts live ones.
            if r.kind == ResourceKind::Volume && mounted.contains(&r.name) {
                r.in_use = true;
            }

            let mut claims = claims_for(&r, &catalog);

            // Replace the single-observation guess with what the index
            // actually counted. Without this, one missing directory would be
            // enough to call something orphaned.
            for c in claims.iter_mut() {
                if let (Liveness::Absent { since_unix, .. }, Some(root)) =
                    (&c.liveness, c.root.as_ref())
                {
                    let key = root.to_string_lossy().into_owned();
                    if let Some(n) = project_absences.get(&key) {
                        c.liveness = Liveness::Absent {
                            since_unix: *since_unix,
                            scans: *n,
                        };
                    }
                }
            }

            // A volume nothing can currently account for may still be
            // remembered from a container that has since been removed. This is
            // the index earning its keep.
            if claims.is_empty() && r.kind == ResourceKind::Volume {
                if let Some((name, path)) = remembered.get(&r.name) {
                    let root = path.as_ref().map(std::path::PathBuf::from);
                    let liveness = crate::providers::liveness_of(root.as_ref());
                    claims.push(crate::model::Claim {
                        project: crate::model::ProjectId(
                            path.clone()
                                .unwrap_or_else(|| name.clone().unwrap_or_default()),
                        ),
                        project_name: name.clone().unwrap_or_else(|| "?".into()),
                        provider: crate::model::ProviderKind::Heuristic,
                        // Historical, not current: an index edge is a memory,
                        // and a memory is Weak evidence by construction. Weak
                        // can never justify a deletion.
                        confidence: Confidence::Weak,
                        root,
                        liveness,
                        evidence: vec![crate::model::Evidence::new(
                            crate::model::EvidenceSource::Index,
                            format!(
                                "remembered from a container that has since been removed{}",
                                name.as_ref()
                                    .map(|n| format!(" (project {n})"))
                                    .unwrap_or_default()
                            ),
                        )],
                    });
                }
            }
            let best = best_claim(&claims).cloned();

            // A volume declared by a compose file whose project is on disk is
            // referenced, whether or not anything is running. Without this,
            // a project you simply have not started today looks abandoned.
            let declared_live = r.kind == ResourceKind::Volume
                && declared_by_live_project(&r.name, &catalog).is_some();
            if declared_live {
                r.in_use = true;
            }
            let orphan = !declared_live && is_orphan_candidate(&claims);
            if !orphan && !declared_live {
                if let Some(n) = crate::providers::is_orphan_pending(&claims) {
                    // Visible, but not actionable: say so rather than either
                    // hiding it or acting on one observation.
                    warnings_once(
                        &mut warnings,
                        format!(
                            "{} looks orphaned but has only been missing across {n} scan(s) — \
                             run again to confirm",
                            r.name
                        ),
                    );
                }
            }
            let unattributed = best
                .as_ref()
                .map(|c| c.confidence < Confidence::Strong)
                .unwrap_or(true);

            sink.emit(Event::ResourceFound {
                resource: r.clone(),
            });

            let content = if r.kind == ResourceKind::Volume {
                contents.get(&r.name).cloned()
            } else {
                None
            };

            resources.push(Attributed {
                content,
                resource: r,
                owner: best.as_ref().map(|c| c.project_name.clone()),
                confidence: best.as_ref().map(|c| c.confidence),
                liveness: best.as_ref().map(|c| c.liveness.clone()),
                claims,
                orphan_candidate: orphan,
                unattributed,
            });
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        sink.emit(Event::ScanFinished {
            totals: totals.clone(),
            duration_ms,
            stale,
        });
        sink.flush();

        Ok(ScanReport {
            daemon,
            context: context.to_string(),
            totals,
            resources,
            projects_known: catalog.len(),
            duration_ms,
            stale,
            warnings,
        })
    }
}

/// Deduplicate endpoints that turn out to be the same engine.
///
/// `default` and `orbstack` on the reference machine are one daemon, because
/// `/var/run/docker.sock` is a symlink. Keying by context name would double
/// count 136 GB, so identity is always the daemon's own `/info` ID.
pub fn dedupe_by_daemon<T>(
    items: Vec<(String, DaemonIdentity, T)>,
) -> Vec<(String, DaemonIdentity, T)> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for (name, id, payload) in items {
        if seen.contains_key(id.id.as_str()) {
            continue;
        }
        seen.insert(id.id.0.clone(), out.len());
        out.push((name, id, payload));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NullSink;
    use crate::model::Bytes;
    use crate::model::{ContainerState, DaemonId, RuntimeFlavor};
    use crate::providers::{COMPOSE_PROJECT, COMPOSE_WORKING_DIR};

    /// A hand-built client. Implements `DockerClient` only — never
    /// `DockerMutate` — so no test can delete anything even by accident.
    struct FakeDocker {
        containers: Vec<ResourceSummary>,
        volumes: Vec<ResourceSummary>,
    }

    impl DockerClient for FakeDocker {
        fn identity(&self) -> Result<DaemonIdentity> {
            Ok(DaemonIdentity {
                id: DaemonId("TEST:DAEMON".into()),
                api_version: "1.54".into(),
                server_version: "29.4.0".into(),
                runtime: RuntimeFlavor::OrbStack,
                data_root: None,
                local: true,
                swarm_active: false,
            })
        }
        fn list_containers(&self) -> Result<Vec<ResourceSummary>> {
            Ok(self.containers.clone())
        }
        fn list_images(&self) -> Result<Vec<ResourceSummary>> {
            Ok(vec![])
        }
        fn list_volumes(&self) -> Result<Vec<ResourceSummary>> {
            Ok(self.volumes.clone())
        }
        fn list_networks(&self) -> Result<Vec<ResourceSummary>> {
            Ok(vec![])
        }
        fn data_usage(&self) -> Result<DataUsage> {
            Ok(DataUsage {
                volume_sizes: BTreeMap::new(),
                build_cache_records: 3,
                build_cache_reclaimable: Bytes(20_520_000_000),
            })
        }
    }

    fn container(name: &str, labels: &[(&str, &str)], mounts: &[&str]) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Container, name, name);
        for (k, v) in labels {
            r.labels.insert((*k).to_string(), (*v).to_string());
        }
        r.mounts = mounts.iter().map(|s| s.to_string()).collect();
        r.state = Some(ContainerState::Exited);
        r
    }

    fn volume(name: &str, labels: &[(&str, &str)]) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Volume, name, name);
        for (k, v) in labels {
            r.labels.insert((*k).to_string(), (*v).to_string());
        }
        r
    }

    #[test]
    fn a_stopped_container_still_marks_its_volume_in_use() {
        // The daemon's RefCount would report 0 here. Deleting on that basis is
        // exactly the bug this graph exists to prevent.
        let anon = "a".repeat(64);
        let client = FakeDocker {
            containers: vec![container("fen-s3-1", &[], &[&anon])],
            volumes: vec![volume(&anon, &[])],
        };
        let report = Scanner::new(&client)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = report
            .of_kind(ResourceKind::Volume)
            .find(|a| a.resource.name == anon)
            .unwrap();
        assert!(v.resource.in_use, "a mount from any container state counts");
    }

    #[test]
    fn unreferenced_anonymous_volume_is_unattributed_not_orphaned() {
        // 50 volumes on the reference machine are in exactly this state: no
        // labels, no container, no recoverable provenance. The honest answer is
        // "I don't know what this was", never an orphan verdict.
        let anon = "b".repeat(64);
        let client = FakeDocker {
            containers: vec![],
            volumes: vec![volume(&anon, &[])],
        };
        let report = Scanner::new(&client)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = report.of_kind(ResourceKind::Volume).next().unwrap();
        assert!(v.unattributed);
        assert!(!v.orphan_candidate);
        assert!(v.owner.is_none());
    }

    #[test]
    fn one_scan_is_not_enough_to_call_a_volume_orphaned() {
        // This is `nbk_mysql` on the first ever run: the label is
        // authoritative and the directory is gone, but a single observation
        // cannot distinguish that from a disk that was unplugged this morning.
        let client = FakeDocker {
            containers: vec![container(
                "nbk-wordpress-1",
                &[
                    (COMPOSE_PROJECT, "nbk"),
                    (COMPOSE_WORKING_DIR, "/tmp/pj-definitely-absent-nbk"),
                ],
                &[],
            )],
            volumes: vec![volume("nbk_mysql", &[(COMPOSE_PROJECT, "nbk")])],
        };
        let report = Scanner::new(&client)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = report
            .of_kind(ResourceKind::Volume)
            .find(|a| a.resource.name == "nbk_mysql")
            .unwrap();
        assert_eq!(v.owner.as_deref(), Some("nbk"));
        assert!(
            !v.orphan_candidate,
            "a single missing observation must never be enough"
        );
    }

    #[test]
    fn a_second_scan_confirms_the_orphan() {
        // Run it twice against the same index and the absence is corroborated,
        // which is what licenses the verdict.
        let client = FakeDocker {
            containers: vec![container(
                "nbk-wordpress-1",
                &[
                    (COMPOSE_PROJECT, "nbk"),
                    (COMPOSE_WORKING_DIR, "/tmp/pj-definitely-absent-nbk"),
                ],
                &[],
            )],
            volumes: vec![volume("nbk_mysql", &[(COMPOSE_PROJECT, "nbk")])],
        };
        let index = crate::index::Index::in_memory().unwrap();

        let first = Scanner::with_index_only(&client, &index)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();
        assert!(
            !first
                .of_kind(ResourceKind::Volume)
                .any(|a| a.orphan_candidate),
            "not on the first run"
        );

        let second = Scanner::with_index_only(&client, &index)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();
        let v = second
            .of_kind(ResourceKind::Volume)
            .find(|a| a.resource.name == "nbk_mysql")
            .unwrap();
        assert!(
            v.orphan_candidate,
            "two consecutive absences is corroboration"
        );
    }

    #[test]
    fn a_volume_is_attributed_from_a_container_that_has_since_gone() {
        // The index earning its keep. First scan sees the container that
        // mounts the anonymous volume; by the second the container is gone,
        // and Docker has no way left to connect the two.
        let anon = "c".repeat(64);
        let index = crate::index::Index::in_memory().unwrap();

        let with_container = FakeDocker {
            containers: vec![container(
                "oak-s3-1",
                &[(COMPOSE_PROJECT, "oak"), (COMPOSE_WORKING_DIR, "/")],
                &[&anon],
            )],
            volumes: vec![volume(&anon, &[])],
        };
        Scanner::with_index_only(&with_container, &index)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let container_gone = FakeDocker {
            containers: vec![],
            volumes: vec![volume(&anon, &[])],
        };
        let after = Scanner::with_index_only(&container_gone, &index)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = after.of_kind(ResourceKind::Volume).next().unwrap();
        assert_eq!(
            v.owner.as_deref(),
            Some("oak"),
            "the index should still know whose this was"
        );
        // But a memory is Weak evidence, and Weak can never license a deletion.
        assert_eq!(v.confidence, Some(Confidence::Weak));
        assert!(v.unattributed, "Weak does not reach Strong");
    }

    #[test]
    fn an_unmounted_root_does_not_mass_orphan_its_projects() {
        // The parent directory is missing too, which is what an unmounted
        // external disk or a not-yet-cloned checkout looks like. Absence of the
        // project dir is NOT sufficient; this must come back unverifiable.
        let client = FakeDocker {
            containers: vec![container(
                "onvolume-1",
                &[
                    (COMPOSE_PROJECT, "onvolume"),
                    (COMPOSE_WORKING_DIR, "/Volumes/NotMounted/onvolume"),
                ],
                &[],
            )],
            volumes: vec![volume("onvolume_mysql", &[(COMPOSE_PROJECT, "onvolume")])],
        };
        let report = Scanner::new(&client)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = report
            .of_kind(ResourceKind::Volume)
            .find(|a| a.resource.name == "onvolume_mysql")
            .unwrap();
        assert!(
            !v.orphan_candidate,
            "an unreadable parent means unverifiable, never absent"
        );
        assert!(matches!(v.liveness, Some(Liveness::Unverifiable { .. })));
    }

    #[test]
    fn a_volume_whose_project_exists_is_never_an_orphan() {
        // `/` always exists, standing in for a live project root.
        let client = FakeDocker {
            containers: vec![container(
                "live-1",
                &[(COMPOSE_PROJECT, "live"), (COMPOSE_WORKING_DIR, "/")],
                &[],
            )],
            volumes: vec![volume("live_mysql", &[(COMPOSE_PROJECT, "live")])],
        };
        let report = Scanner::new(&client)
            .scan(
                "test",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = report
            .of_kind(ResourceKind::Volume)
            .find(|a| a.resource.name == "live_mysql")
            .unwrap();
        assert!(!v.orphan_candidate);
        assert!(!v.unattributed);
    }

    #[test]
    fn dedupe_collapses_two_contexts_onto_one_engine() {
        let id = |s: &str| DaemonIdentity {
            id: DaemonId(s.into()),
            api_version: "1.54".into(),
            server_version: "29".into(),
            runtime: RuntimeFlavor::OrbStack,
            data_root: None,
            local: true,
            swarm_active: false,
        };
        let items = vec![
            ("orbstack".to_string(), id("ENGINE-A"), ()),
            ("default".to_string(), id("ENGINE-A"), ()),
            ("other".to_string(), id("ENGINE-B"), ()),
        ];
        let out = dedupe_by_daemon(items);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "orbstack", "the first context seen wins");
        assert_eq!(out[1].0, "other");
    }

    #[test]
    fn cancellation_is_honoured_mid_scan() {
        let client = FakeDocker {
            containers: vec![],
            volumes: vec![],
        };
        let cancel = Cancel::new();
        cancel.cancel();
        let err = Scanner::new(&client)
            .scan("test", &ScanOptions::default(), Arc::new(NullSink), &cancel)
            .unwrap_err();
        assert!(matches!(err, crate::Error::Cancelled));
    }
}
