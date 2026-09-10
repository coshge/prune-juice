//! The scan pipeline.
//!
//! Read-only, always. There is no deletion code path anywhere in this
//! milestone's binary — destructive operations live behind [`DockerMutate`],
//! which nothing implements yet.
//!
//! [`DockerMutate`]: crate::docker::DockerMutate

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::docker::{DaemonIdentity, DataUsage, DockerClient, DockerProbe};
use crate::error::Result;
use crate::event::{Cancel, Event, EventSink, Phase};
use crate::index::Index;
use crate::model::{
    Bytes, Claim, Confidence, Liveness, ResourceKind, ResourceSummary, SizeSource, Totals,
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
    /// Give up after this long and report what has been gathered.
    ///
    /// Mandatory in spirit even though it is optional in type: a scan touches a
    /// filesystem it does not control, and `system df` is reported at minutes
    /// on some setups. Without a deadline "slow" and "hung" are the same thing
    /// to a user, and a GUI has no way to tell them apart either.
    pub deadline: Option<std::time::Duration>,
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
            deadline: Some(std::time::Duration::from_secs(120)),
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
    /// How this could be got back if it were removed. Drives the opt-in tiers:
    /// a thing with a stated price can be offered; a thing with none cannot.
    pub recovery: Option<crate::model::Recovery>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanReport {
    pub daemon: DaemonIdentity,
    pub context: String,
    pub totals: Totals,
    pub resources: Vec<Attributed>,
    pub projects_known: usize,
    pub duration_ms: u64,
    /// The current container -> volume edges were durably committed before
    /// classification. Only then may an otherwise disposable container stop
    /// being treated as the sole copy of that provenance.
    #[serde(default)]
    pub provenance_checkpointed: bool,
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

/// How an image could be got back.
///
/// Only images, for now: a volume's contents are not reproducible from a
/// checkout, and a container is recreated by compose as a matter of course.
///
/// A registry digest beats a build context, because a digest either resolves
/// or it does not, whereas a rebuild can fail in a dozen ways that only show
/// up when someone urgently needs it to work.
fn recovery_for(
    r: &ResourceSummary,
    catalog: &Catalog,
    roots: &[PathBuf],
) -> Option<crate::model::Recovery> {
    use crate::model::Recovery;
    if r.kind != ResourceKind::Image {
        return None;
    }

    // A local build first, and a registry digest only as a fallback.
    //
    // The obvious order is wrong: BuildKit stamps a RepoDigests entry on
    // locally built images too, so `fen-wordpress@sha256:…` looks exactly like
    // a pullable reference and is not — `docker pull` on it fails. Treating a
    // digest as proof of pullability marked all 89 project images as
    // "re-pullable, costs bandwidth" when in truth each needs a rebuild.
    //
    // Matching `<project>-<service>` back to a project directory is decisive:
    // `fen-wordpress` resolves to `fen`, `alpine` resolves to nothing.
    let tag = r.repo_tags.first().map(|t| t.as_str()).unwrap_or(&r.name);
    let repo = tag.split(':').next().unwrap_or(tag);
    // Only a bare `name-service` can be one of ours; anything with a registry
    // host in it came from a registry.
    let local_shaped = !repo.contains('/');

    if local_shaped {
        if let Some((candidate, service)) = repo.rsplit_once('-') {
            let dir = catalog
                .get(candidate)
                .and_then(|p| p.root.clone())
                .or_else(|| roots.iter().map(|r| r.join(candidate)).find(|p| p.is_dir()));

            if let Some(dir) = dir {
                return Some(if has_build_context(&dir) {
                    Recovery::Build {
                        command: format!("docker compose build {service}"),
                        dir,
                    }
                } else {
                    Recovery::Impossible {
                        why: format!("no Dockerfile found under {}", dir.display()),
                    }
                });
            }
            // Looks like one of ours, but the project is gone. This is nbk:
            // tagged, a gigabyte, and nothing will ever rebuild it.
            return Some(Recovery::Impossible {
                why: format!(
                    "no project directory for \"{candidate}\" — nothing left to build from"
                ),
            });
        }
    }

    match r.repo_digests.first() {
        Some(d) => Some(Recovery::Pull(d.clone())),
        None => Some(Recovery::Impossible {
            why: "no registry reference and no local build context".into(),
        }),
    }
}

/// Is there something here that could rebuild an image?
fn has_build_context(dir: &Path) -> bool {
    for rel in [
        "Dockerfile",
        "docker/Dockerfile",
        "docker/wordpress/Dockerfile",
        "Dockerfile.nginx",
    ] {
        if dir.join(rel).is_file() {
            return true;
        }
    }
    // A shallow walk, since these stacks nest their Dockerfiles a level or two
    // down and a full walk would cost more than the answer is worth.
    for depth1 in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = depth1.path();
        if !p.is_dir() {
            continue;
        }
        if p.join("Dockerfile").is_file() {
            return true;
        }
        for depth2 in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            if depth2.path().join("Dockerfile").is_file() {
                return true;
            }
        }
    }
    false
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
        // Closure rather than a flag, so every caller checks the live clock.
        let out_of_time = |elapsed: std::time::Duration| -> bool {
            opts.deadline.map(|d| elapsed >= d).unwrap_or(false)
        };

        let daemon = self.client.identity()?;
        sink.emit(Event::ScanStarted {
            daemon: daemon.id.clone(),
            context: context.to_string(),
            runtime: daemon.runtime,
            api_version: daemon.api_version.clone(),
        });
        cancel.check()?;

        // `df` is the slowest call the scan makes and nothing between here and
        // the sizing phase needs its answer, so it is started now and
        // collected later. A client that cannot overlap ignores this.
        if opts.with_sizes {
            self.client.start_data_usage();
        }

        // --- listing -----------------------------------------------------
        sink.emit(Event::Phase {
            phase: Phase::Listing,
            done: 0,
            total: Some(4),
        });

        let mut containers = self.client.list_containers()?;
        sink.emit(Event::Phase {
            phase: Phase::Listing,
            done: 1,
            total: Some(4),
        });
        cancel.check()?;

        let mut images = self.client.list_images()?;
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
                            // A volume owns every byte under it, so its
                            // exclusive size is its size.
                            v.exclusive_size = Some(*b);
                            sink.emit(Event::SizeUpdated {
                                id: v.id.clone(),
                                bytes: *b,
                                source: SizeSource::DaemonDf,
                            });
                        }
                    }
                    // Images: what the tool would reclaim by removing one is
                    // its size minus the layers another image also holds.
                    for i in &mut images {
                        let Some(shared) = u.image_shared_sizes.get(i.id.as_str()) else {
                            continue;
                        };
                        let total = i.size.unwrap_or(Bytes::ZERO);
                        i.exclusive_size = Some(Bytes(total.get().saturating_sub(shared.get())));
                    }
                    // Containers: the writable layer, measured. This is the
                    // only place it is ever known, and a container may only
                    // reach the safe tier on a measured zero.
                    let mut measured = 0usize;
                    for c in &mut containers {
                        if let Some(rw) = u.container_rw_sizes.get(c.id.as_str()) {
                            c.size_rw = Some(*rw);
                            measured += 1;
                        }
                    }
                    // Said out loud rather than absorbed. Without these
                    // figures no container can be proven data-free, so the
                    // safe tier quietly loses every container — which a user
                    // should hear about as a reason, not discover as a gap.
                    if measured < containers.len() {
                        let msg = format!(
                            "the daemon reported no writable-layer size for {} of {} \
                             containers, so those cannot be proven empty and are \
                             not offered as safe",
                            containers.len() - measured,
                            containers.len()
                        );
                        sink.emit(Event::Warning {
                            code: "container_sizes_unavailable".into(),
                            message: msg.clone(),
                            resource: None,
                        });
                        warnings.push(msg);
                    }
                    // Remembered for the next run that cannot afford the
                    // call. Written before anything is judged with it.
                    if let Some(index) = self.index {
                        if let Err(e) =
                            index.record_volume_sizes(&daemon.id, &u.volume_sizes, now_unix())
                        {
                            warnings.push(format!("volume sizes could not be remembered: {e}"));
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

        // No fresh figures — `--no-sizes`, or a `df` that failed. Fall back to
        // what a previous run measured, but only for a volume nothing is
        // mounting: an unreferenced volume's bytes cannot change, so the
        // remembered figure is still true, while an in-use volume is being
        // written to right now and last week's number for it would be an
        // invented measurement. Labelled `Cache` so the interface can say
        // "remembered" rather than implying it just looked.
        let sizes_measured = opts.with_sizes && !usage.volume_sizes.is_empty();
        if !sizes_measured {
            if let Some(index) = self.index {
                let remembered = index.volume_sizes(&daemon.id).unwrap_or_default();
                let mut used = 0usize;
                for v in &mut volumes {
                    if v.size.is_some() || mounted.contains(&v.name) {
                        continue;
                    }
                    if let Some((bytes, _measured_at)) = remembered.get(&v.name) {
                        v.size = Some(*bytes);
                        v.exclusive_size = Some(*bytes);
                        used += 1;
                        sink.emit(Event::SizeUpdated {
                            id: v.id.clone(),
                            bytes: *bytes,
                            source: SizeSource::Cache,
                        });
                    }
                }
                if used > 0 {
                    warnings.push(format!(
                        "{used} volume size(s) are remembered from an earlier run, not \
                         measured now; volumes in use have no figure at all"
                    ));
                }
            }
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
                    if out_of_time(started.elapsed()) {
                        warnings_once(
                            &mut warnings,
                            format!(
                                "gave up reading volume contents after {}s — {} of {} were read, \
                                 so fewer volumes can be proven safe",
                                started.elapsed().as_secs(),
                                i,
                                volumes.len()
                            ),
                        );
                        stale = true;
                        break;
                    }
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
                        if out_of_time(started.elapsed()) {
                            warnings_once(
                                &mut warnings,
                                format!(
                                    "gave up probing volumes after {}s; fewer can be proven safe",
                                    started.elapsed().as_secs()
                                ),
                            );
                            stale = true;
                            break;
                        }
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
        let mut recalled_paths: BTreeMap<String, String> = BTreeMap::new();
        let mut provenance_checkpointed = false;
        if let Some(index) = self.index {
            let all: Vec<ResourceSummary> = containers
                .iter()
                .chain(images.iter())
                .chain(volumes.iter())
                .chain(networks.iter())
                .cloned()
                .collect();
            match index
                .begin_scan(&daemon.id, now_unix)
                .and_then(|_| index.record_scan(&daemon.id, &all, now_unix))
            {
                Ok(()) => provenance_checkpointed = true,
                Err(e) => warnings.push(format!("the index could not be updated: {e}")),
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

            // Project name to path, derived from the same edges. Volumes are
            // keyed by their own name; an image only ever knows its project's
            // name, so it needs this second view to have a path recalled too.
            for (name, path) in remembered.values() {
                if let (Some(name), Some(path)) = (name, path) {
                    recalled_paths.entry(name.clone()).or_insert(path.clone());
                }
            }

            // Also pull absences for paths only the index remembers.
            //
            // Once the last container naming a project is gone, that project
            // drops out of the catalog too — so keying absences off the
            // catalog alone loses the count exactly when it is needed most.
            // Found by running --apply for real: afterwards the restored
            // volumes came back Unattributed because nothing left knew where
            // "nbk" had lived.
            for (_, path) in remembered.values() {
                let Some(path) = path else { continue };
                if project_absences.contains_key(path) {
                    continue;
                }
                if let Ok(Some(h)) = index.project_history(&daemon.id, path) {
                    project_absences.insert(path.clone(), h.absent_scans);
                }
            }
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
        // The daemon's own figure, never a sum of ours: layer stacks overlap,
        // and only the daemon knows which layers two images share.
        totals.image_unique_bytes = usage.image_layers_size;

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

            // A claim can name its project and still not know where it lived.
            // That happens the moment the last container carrying the path is
            // removed: the resource's own label says `project=nbk`, but no
            // label anywhere says where `nbk` is, so the claim cannot be
            // checked against a directory and falls to Weak.
            //
            // The index remembers. This applies to every kind, not just
            // volumes — nbk's two tagged images sat in Protected for exactly
            // this reason, a gigabyte that nothing will ever rebuild or want.
            for c in claims.iter_mut() {
                if c.root.is_some() {
                    continue;
                }
                let recalled = recalled_paths.get(&c.project_name).cloned().or_else(|| {
                    match remembered.get(&r.name) {
                        Some((_, Some(p))) => Some(p.clone()),
                        _ => None,
                    }
                });
                let Some(path) = recalled else { continue };

                let root = std::path::PathBuf::from(&path);
                // The filesystem decides whether this is an absence; the index
                // only counts how many scans it has lasted. Reading a stored
                // count as the verdict made every recalled project absent,
                // including the ones that are plainly there: `project_absences`
                // carries a row for every project with any history, and a
                // present one has `absent_scans = 0`. That produced "missing
                // across 0 scan(s) — run again to confirm" on every run, an
                // instruction no number of runs could satisfy, and it dropped
                // the `Present` claim whose whole job is to veto another
                // claim's orphan verdict.
                c.liveness = match crate::providers::liveness_of(Some(&root)) {
                    crate::model::Liveness::Absent { since_unix, scans } => {
                        crate::model::Liveness::Absent {
                            since_unix,
                            scans: project_absences.get(&path).copied().unwrap_or(scans),
                        }
                    }
                    settled => settled,
                };
                c.evidence.push(crate::model::Evidence::new(
                    crate::model::EvidenceSource::Index,
                    format!(
                        "project path {path} recalled from a container that has since been removed"
                    ),
                ));
                c.root = Some(root);
                // Ownership was already authoritative from the label; only the
                // location was missing.
                if c.confidence < Confidence::Strong {
                    c.confidence = Confidence::Strong;
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

            let recovery = recovery_for(&r, &catalog, &opts.project_roots);

            resources.push(Attributed {
                recovery,
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
            provenance_checkpointed,
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
                image_shared_sizes: BTreeMap::new(),
                image_layers_size: None,
                container_rw_sizes: BTreeMap::new(),
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
    fn a_remembered_size_is_used_only_where_it_cannot_have_changed() {
        // `--no-sizes` (and a failed `df`) leave the scan with no figures at
        // all. An unreferenced volume's bytes cannot change while nothing is
        // mounting it, so last run's measurement is still true — but a volume
        // in use is being written to right now, and reporting an old number
        // for it would be inventing a measurement.
        let index = crate::index::Index::in_memory().unwrap();
        let daemon = DaemonId("TEST:DAEMON".into());
        let mut measured = BTreeMap::new();
        measured.insert("idle_data".to_string(), Bytes(1_200_000_000));
        measured.insert("live_data".to_string(), Bytes(800_000_000));
        index
            .record_volume_sizes(&daemon, &measured, 1_000)
            .unwrap();

        let client = FakeDocker {
            containers: vec![container("app-1", &[], &["live_data"])],
            volumes: vec![volume("idle_data", &[]), volume("live_data", &[])],
        };
        let report = Scanner::with_index_only(&client, &index)
            .scan(
                "test",
                &ScanOptions {
                    with_sizes: false,
                    ..ScanOptions::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let size_of = |name: &str| {
            report
                .of_kind(ResourceKind::Volume)
                .find(|a| a.resource.name == name)
                .unwrap()
                .resource
                .size
        };
        assert_eq!(size_of("idle_data"), Some(Bytes(1_200_000_000)));
        assert_eq!(
            size_of("live_data"),
            None,
            "a mounted volume's remembered size is not a measurement of it now"
        );
        assert!(
            report.stale,
            "a report built on remembered figures is not a fresh one"
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("remembered")),
            "the report must say the figures were remembered: {:?}",
            report.warnings
        );
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
            first.provenance_checkpointed,
            "a successful index write must be visible to the planner"
        );
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
    fn the_index_supplies_a_path_the_labels_no_longer_carry() {
        // Removing the last container that named a project's directory leaves
        // its volumes labelled but unlocatable — the exact provenance loss the
        // index exists to prevent. Found by running --apply for real: the
        // volumes came back Unattributed afterwards.
        let index = crate::index::Index::in_memory().unwrap();
        let missing = "/tmp/pj-recalled-absent";

        let with_container = FakeDocker {
            containers: vec![container(
                "nbk-wordpress-1",
                &[(COMPOSE_PROJECT, "nbk"), (COMPOSE_WORKING_DIR, missing)],
                &["nbk_mysql"],
            )],
            volumes: vec![volume("nbk_mysql", &[(COMPOSE_PROJECT, "nbk")])],
        };
        // Two scans, so the absence is corroborated.
        for _ in 0..2 {
            Scanner::with_index_only(&with_container, &index)
                .scan(
                    "t",
                    &ScanOptions::default(),
                    Arc::new(NullSink),
                    &Cancel::new(),
                )
                .unwrap();
        }

        // Now the container is gone. Nothing left says where "nbk" lived.
        let container_gone = FakeDocker {
            containers: vec![],
            volumes: vec![volume("nbk_mysql", &[(COMPOSE_PROJECT, "nbk")])],
        };
        let after = Scanner::with_index_only(&container_gone, &index)
            .scan(
                "t",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = after.of_kind(ResourceKind::Volume).next().unwrap();
        assert_eq!(v.owner.as_deref(), Some("nbk"));
        assert_eq!(
            v.confidence,
            Some(Confidence::Strong),
            "the label owns it; the index only had to supply the path"
        );
        assert!(
            v.claims.iter().any(|c| c.root.is_some()),
            "the recalled path must be attached to the claim"
        );
        assert!(
            v.orphan_candidate,
            "and the verdict should now be reachable"
        );
    }

    #[test]
    fn a_recalled_path_that_still_exists_is_present_not_absent_across_zero_scans() {
        // `project_absences` holds a row for every project with any history,
        // and a present one reads `absent_scans = 0`. Taking that as the
        // verdict marked a recalled project absent while its directory sat
        // right there: on the reference machine ddev's global services warned
        // "missing across 0 scan(s) — run again to confirm" on every single
        // run, which no number of runs could ever satisfy. Worse, it threw
        // away the `Present` claim whose only job is to veto another claim's
        // orphan verdict.
        let index = crate::index::Index::in_memory().unwrap();
        // A directory that exists — the analogue of `~/.ddev`.
        let present = std::env::temp_dir().join("pj-recalled-present");
        std::fs::create_dir_all(&present).unwrap();
        let path = present.to_string_lossy().into_owned();

        let with_container = FakeDocker {
            containers: vec![container(
                "svc-1",
                &[(COMPOSE_PROJECT, "svc"), (COMPOSE_WORKING_DIR, &path)],
                &["svc_data"],
            )],
            volumes: vec![volume("svc_data", &[(COMPOSE_PROJECT, "svc")])],
        };
        Scanner::with_index_only(&with_container, &index)
            .scan(
                "t",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        // The container goes, so the path can only come from the index — but
        // the directory it names is still there.
        let container_gone = FakeDocker {
            containers: vec![],
            volumes: vec![volume("svc_data", &[(COMPOSE_PROJECT, "svc")])],
        };
        let after = Scanner::with_index_only(&container_gone, &index)
            .scan(
                "t",
                &ScanOptions::default(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        let v = after.of_kind(ResourceKind::Volume).next().unwrap();
        assert!(
            v.claims
                .iter()
                .any(|c| c.liveness == crate::model::Liveness::Present),
            "a directory that exists is Present: {:?}",
            v.claims.iter().map(|c| &c.liveness).collect::<Vec<_>>()
        );
        assert!(!v.orphan_candidate);
        assert!(
            !after.warnings.iter().any(|w| w.contains("run again")),
            "nothing to confirm, so nothing to nag about: {:?}",
            after.warnings
        );

        std::fs::remove_dir_all(&present).ok();
    }

    /// A prober that reports an image and then is never asked for anything.
    ///
    /// Enough to reach the batched probe loop, which is where the deadline is
    /// consulted. `probe_volumes` panics on purpose: an expired deadline must
    /// mean no volume is read at all, so a call here is the test failing.
    struct FakeProber;

    impl crate::docker::DockerProbe for FakeProber {
        fn probe_image(&self) -> Option<String> {
            Some("alpine:latest".into())
        }
        fn probe_volumes(
            &self,
            _volumes: &[String],
        ) -> Result<BTreeMap<String, crate::docker::RawProbe>> {
            unreachable!("the deadline had already expired; nothing should be read")
        }
        fn dump_volume(&self, _name: &str, _out: &mut dyn std::io::Write) -> Result<u64> {
            unreachable!("not part of a scan")
        }
    }

    #[test]
    fn a_deadline_degrades_the_scan_rather_than_hanging() {
        // "Slow" and "hung" look identical to a user, and a GUI cannot tell
        // them apart either. A deadline turns an unbounded wait into a partial
        // answer that says it is partial.
        //
        // `force_container_probe` is not incidental. Without it the path taken
        // depends on whether the machine running the test happens to have a
        // readable Docker data root — which is why this passed on the author's
        // machine and failed on a CI runner, where there is no data root, the
        // host loop is skipped and the deadline was never consulted.
        let client = FakeDocker {
            containers: vec![],
            volumes: vec![volume("v", &[])],
        };
        let report = Scanner::with_probe(&client, &FakeProber)
            .scan(
                "test",
                &ScanOptions {
                    deadline: Some(std::time::Duration::ZERO),
                    force_container_probe: true,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert!(report.stale, "a curtailed scan must admit it is incomplete");
        assert!(
            report.warnings.iter().any(|w| w.contains("gave up")),
            "and say so in words: {:?}",
            report.warnings
        );
        // And nothing it could not read may be treated as provably safe.
        let v = report.of_kind(ResourceKind::Volume).next().unwrap();
        assert!(v.content.is_none());
    }

    #[test]
    fn no_deadline_means_no_deadline() {
        let client = FakeDocker {
            containers: vec![],
            volumes: vec![volume("v", &[])],
        };
        let report = Scanner::new(&client)
            .scan(
                "test",
                &ScanOptions {
                    deadline: None,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();
        assert!(!report.warnings.iter().any(|w| w.contains("gave up")));
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
