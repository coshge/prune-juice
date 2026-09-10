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

use crate::docker::{DaemonIdentity, DataUsage, DockerClient};
use crate::error::Result;
use crate::event::{Cancel, Event, EventSink, Phase};
use crate::model::{
    Claim, Confidence, Liveness, ResourceKind, ResourceSummary, SizeSource, Totals,
};
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
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            project_roots: Vec::new(),
            with_sizes: true,
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

pub struct Scanner<'a> {
    client: &'a dyn DockerClient,
}

impl<'a> Scanner<'a> {
    pub fn new(client: &'a dyn DockerClient) -> Self {
        Self { client }
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

            let claims = claims_for(&r, &catalog);
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
            let unattributed = best
                .as_ref()
                .map(|c| c.confidence < Confidence::Strong)
                .unwrap_or(true);

            sink.emit(Event::ResourceFound {
                resource: r.clone(),
            });

            resources.push(Attributed {
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
    fn a_volume_whose_project_path_is_gone_is_an_orphan_candidate() {
        // This is `nbk_mysql`: the label is authoritative, but the directory
        // was renamed away years ago.
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
        assert!(v.orphan_candidate);
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
