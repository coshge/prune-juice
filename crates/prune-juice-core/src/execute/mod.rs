//! Applying a plan.
//!
//! Three rules govern everything here:
//!
//! * **Dry-run is the default.** `Apply` has to be asked for.
//! * **Revalidate per item, immediately before its own delete** — not once per
//!   batch. The window this closes is measured in milliseconds.
//! * **Never pass a force flag.** `docker rmi -f` and `volume rm --force`
//!   disable the daemon's own in-use check, which is the cheapest and last line
//!   of defence we have. A refusal from the daemon is the system working.

use serde::{Deserialize, Serialize};

use crate::disk::{reconcile, BackingStore, Reclamation};
use crate::docker::{DockerMutate, DockerProbe};
use crate::error::Result;
use crate::event::{Cancel, Event, EventSink};
use crate::model::{Bytes, DaemonId, ResourceKind};
use crate::plan::tier::Tier;
use crate::plan::{Plan, Staleness};
use crate::scan::ScanReport;
use crate::vault::{Vault, VaultEntry};

use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Decide everything, touch nothing. The default, and the oracle the apply
    /// path is tested against.
    DryRun,
    Apply,
}

#[derive(Clone, Debug)]
pub struct ExecuteOptions {
    pub mode: Mode,
    /// Which tiers to act on. Tier 1 alone is the no-confirmation default;
    /// anything else must be an explicit, reviewed choice.
    pub tiers: Vec<Tier>,
    /// Refuse to touch anything not carrying this label. Shipped as a real
    /// feature because it is genuinely useful, and used as the integration-test
    /// harness so a real-daemon test cannot escape its own sandbox.
    pub only_label: Option<(String, String)>,
    /// Act only on these exact resource names.
    ///
    /// A fence like `only_label`, for a user who reviewed a list and ticked
    /// specific rows. Anything not named is unreachable by the run, not merely
    /// skipped.
    pub only_names: Option<std::collections::BTreeSet<String>>,
    /// Preserve a verified copy before any irreversible deletion.
    ///
    /// On by default, and turning it off is what `--no-vault` is for. With it
    /// off, an irreversible item is **refused** rather than deleted unpreserved:
    /// declining to take a backup is not the same as consenting to lose data.
    pub vault: bool,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            mode: Mode::DryRun,
            tiers: vec![Tier::Free],
            only_label: None,
            only_names: None,
            vault: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "detail")]
pub enum ItemOutcome {
    /// Dry run: this is what would have happened.
    WouldDelete,
    Deleted,
    /// Already gone by the time we got there. Not a failure.
    AlreadyGone,
    /// The witness no longer held. Skipped deliberately.
    Skipped(Staleness),
    /// The daemon refused. The last line of defence working as intended.
    Refused(String),
    Failed(String),
    /// Preserved to the vault and then removed. Carries the entry id, which is
    /// what `restore` needs.
    Preserved(String),
    /// Could not be preserved, so it was left alone. The difference between a
    /// backup and the appearance of one.
    PreserveFailed(String),
}

impl ItemOutcome {
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            ItemOutcome::Refused(_) | ItemOutcome::Failed(_) | ItemOutcome::PreserveFailed(_)
        )
    }

    pub fn removed(&self) -> bool {
        matches!(self, ItemOutcome::Deleted | ItemOutcome::Preserved(_))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptItem {
    pub kind: ResourceKind,
    pub name: String,
    pub tier: Tier,
    pub size: Option<Bytes>,
    pub evidence_hash: String,
    pub outcome: ItemOutcome,
}

/// What actually happened. Persisted, and the basis of `undo`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub daemon: DaemonId,
    pub started_unix: i64,
    pub simulated: bool,
    pub items: Vec<ReceiptItem>,
    pub build_cache_reclaimed: Bytes,
    /// What the daemon told us we freed. Host-measured reclamation is a
    /// separate, and different, number.
    pub docker_reported: Bytes,
    /// What the host actually gave back, where that could be measured.
    /// `None` means unmeasurable — never quietly filled in with the Docker
    /// figure, because conflating the two is the whole mistake.
    pub reclamation: Option<Reclamation>,
}

impl Receipt {
    pub fn deleted(&self) -> usize {
        self.items.iter().filter(|i| i.outcome.removed()).count()
    }

    /// Entries written to the vault by this run, in order.
    pub fn preserved(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter_map(|i| match &i.outcome {
                ItemOutcome::Preserved(id) => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }
    pub fn skipped(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.outcome, ItemOutcome::Skipped(_)))
            .count()
    }
    pub fn problems(&self) -> usize {
        self.items.iter().filter(|i| i.outcome.is_problem()).count()
    }
}

pub struct Executor<'a> {
    mutate: Option<&'a dyn DockerMutate>,
    /// Needed to read a volume out before deleting it.
    prober: Option<&'a dyn DockerProbe>,
    vault: Option<&'a Vault>,
    /// Where the daemon's bytes physically live, so real host reclamation can
    /// be measured rather than inferred from Docker's logical figures.
    disk: Option<&'a BackingStore>,
}

impl<'a> Executor<'a> {
    /// A dry-run executor has no mutating client at all — it is not merely
    /// discouraged from deleting, it is unable to.
    pub fn dry_run() -> Self {
        Self {
            mutate: None,
            prober: None,
            vault: None,
            disk: None,
        }
    }

    pub fn applying(mutate: &'a dyn DockerMutate) -> Self {
        Self {
            mutate: Some(mutate),
            prober: None,
            vault: None,
            disk: None,
        }
    }

    /// Give the executor what it needs to preserve a volume before removing it.
    /// Without this, an irreversible item can only ever be refused.
    pub fn with_vault(mut self, prober: &'a dyn DockerProbe, vault: &'a Vault) -> Self {
        self.prober = Some(prober);
        self.vault = Some(vault);
        self
    }

    /// Measure real host reclamation around the run.
    pub fn with_disk(mut self, disk: &'a BackingStore) -> Self {
        self.disk = Some(disk);
        self
    }

    /// Apply `plan`, revalidating every item against `now` first.
    ///
    /// `now` must be a freshly taken report — the caller re-scans between
    /// planning and applying, and this is where that freshness is cashed in.
    pub fn run(
        &self,
        plan: Plan,
        now: &ScanReport,
        now_unix: i64,
        opts: &ExecuteOptions,
        sink: Arc<dyn EventSink>,
        cancel: &Cancel,
    ) -> Result<Receipt> {
        let simulated = opts.mode == Mode::DryRun || self.mutate.is_none();
        let mut items = Vec::new();
        let mut freed = Bytes::ZERO;

        // Measure the host *before* touching anything. A dry run measures too,
        // so the reporting path is exercised identically.
        let before = self.disk.map(|d| {
            let u = d.measure(now_unix);
            sink.emit(Event::HostMeasured {
                phase: "before".into(),
                physical: u.physical,
                fs_free: u.fs_free,
            });
            u
        });

        let mut plan = plan;
        for item in plan.items.iter_mut() {
            if !opts.tiers.contains(&item.verdict.tier) {
                continue;
            }
            // A hard fence, not a filter. Anything without the required label
            // is not merely skipped from the plan — it can never be reached by
            // this run at all. Real-daemon integration tests rely on this to
            // stay inside their own sandbox.
            if let Some((k, v)) = &opts.only_label {
                if item.labels.get(k).map(String::as_str) != Some(v.as_str()) {
                    continue;
                }
            }
            if let Some(names) = &opts.only_names {
                if !names.contains(&item.name) {
                    continue;
                }
            }
            cancel.check()?;

            let Some(witness) = item.take_witness() else {
                continue;
            };

            let hash = witness.evidence_hash().to_string();
            let record = |outcome: ItemOutcome| ReceiptItem {
                kind: item.kind,
                name: item.name.clone(),
                tier: item.verdict.tier,
                size: item.size,
                evidence_hash: hash.clone(),
                outcome,
            };

            // The witness is consumed here. There is no way to reach the delete
            // below without one, and no way to obtain one without this check.
            let fresh = match witness.revalidate(now, now_unix) {
                Ok(f) => f,
                Err(Staleness::Vanished { name }) => {
                    items.push(record(ItemOutcome::AlreadyGone));
                    sink.emit(Event::Warning {
                        code: "already_gone".into(),
                        message: format!("{name} was removed by something else"),
                        resource: None,
                    });
                    continue;
                }
                Err(s) => {
                    sink.emit(Event::Warning {
                        code: "stale_plan".into(),
                        message: s.to_string(),
                        resource: None,
                    });
                    items.push(record(ItemOutcome::Skipped(s)));
                    continue;
                }
            };

            if simulated {
                items.push(record(ItemOutcome::WouldDelete));
                freed = freed + item.size.unwrap_or(Bytes::ZERO);
                continue;
            }

            let w = fresh.into_inner();
            let mutate = self.mutate.expect("checked by `simulated` above");

            // Anything irreversible gets preserved first, or is not touched.
            //
            // The order here is the whole safety property: dump, fsync, re-read
            // and confirm the digest and entry count, and only then delete. A
            // failure at any step leaves the volume exactly where it was.
            let mut preserved: Option<VaultEntry> = None;
            if item.verdict.reversibility.is_gone() && item.kind == ResourceKind::Volume {
                if !opts.vault {
                    // Declining to take a backup is not consent to lose data.
                    items.push(record(ItemOutcome::Refused(
                        "irreversible, and --no-vault was given, so it was left alone".into(),
                    )));
                    continue;
                }
                let (Some(prober), Some(vault)) = (self.prober, self.vault) else {
                    items.push(record(ItemOutcome::Refused(
                        "irreversible, and no vault is configured to preserve it".into(),
                    )));
                    continue;
                };
                let engine = item.engine;
                match vault.store(
                    prober,
                    &item.name,
                    &item.labels,
                    engine,
                    item.size,
                    now_unix,
                ) {
                    Ok(entry) => {
                        sink.emit(Event::Warning {
                            code: "preserved".into(),
                            message: format!(
                                "{} preserved to the vault as {} ({})",
                                item.name,
                                entry.id,
                                entry.archive_bytes.human()
                            ),
                            resource: None,
                        });
                        preserved = Some(entry);
                    }
                    Err(e) => {
                        items.push(record(ItemOutcome::PreserveFailed(e.to_string())));
                        continue;
                    }
                }
            }

            let res = match w.kind() {
                ResourceKind::Volume => mutate.remove_volume(w.name()),
                ResourceKind::Image => mutate.remove_image(w.resource().as_str()),
                ResourceKind::Container => mutate.remove_container(w.resource().as_str()),
                ResourceKind::Network => mutate.remove_network(w.resource().as_str()),
                // Build cache is aggregate-only; it never becomes a plan item.
                ResourceKind::BuildCache => Ok(()),
            };

            match res {
                Ok(()) => {
                    freed = freed + item.size.unwrap_or(Bytes::ZERO);
                    match &preserved {
                        Some(e) => items.push(record(ItemOutcome::Preserved(e.id.clone()))),
                        None => items.push(record(ItemOutcome::Deleted)),
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    // "in use" from the daemon is not a bug on our side; it is
                    // the backstop catching a race we could not see.
                    let outcome = if msg.to_ascii_lowercase().contains("in use") {
                        ItemOutcome::Refused(msg)
                    } else {
                        ItemOutcome::Failed(msg)
                    };
                    items.push(record(outcome));
                }
            }
        }

        // Build cache last: it is aggregate, reversible, and cannot affect any
        // other resource's tier.
        //
        // Skipped entirely under `--only-label`: build cache records carry no
        // labels, so there is no way to honour the fence, and silently pruning
        // it anyway would break the promise the flag makes.
        let mut cache_freed = Bytes::ZERO;
        if opts.only_label.is_none()
            && opts.only_names.is_none()
            && opts.tiers.contains(&Tier::Free)
            && plan.build_cache_reclaimable > Bytes::ZERO
        {
            if simulated {
                cache_freed = plan.build_cache_reclaimable;
            } else if let Some(m) = self.mutate {
                match m.prune_build_cache(super::plan::tier::MIN_AGE_SECS as u64) {
                    Ok(b) => cache_freed = b,
                    Err(e) => sink.emit(Event::Warning {
                        code: "build_cache_prune_failed".into(),
                        message: e.to_string(),
                        resource: None,
                    }),
                }
            }
        }

        // Settle, then reconcile.
        //
        // Deleting inside the guest does not shrink a sparse host image until
        // the guest issues discard and the runtime punches holes, so the number
        // needs a moment to catch up. Poll until it stops moving rather than
        // reading it once and reporting a lie.
        let reclamation = match (self.disk, before) {
            (Some(store), Some(before)) if !simulated => {
                let after = settle(store, now_unix, &sink);
                let r = reconcile(&before, &after, freed + cache_freed, store);
                sink.emit(Event::HostReclaim {
                    docker_reported: r.docker_reported,
                    host_measured: r.host_measured,
                    confidence: format!("{:?}", r.confidence).to_lowercase(),
                });
                Some(r)
            }
            (Some(store), Some(before)) => {
                // Dry run: nothing moved, so report the shape without claiming
                // a measurement.
                Some(reconcile(&before, &before, freed + cache_freed, store))
            }
            _ => None,
        };

        sink.flush();
        Ok(Receipt {
            reclamation,
            daemon: plan.daemon.clone(),
            started_unix: now_unix,
            simulated,
            items,
            build_cache_reclaimed: cache_freed,
            docker_reported: freed + cache_freed,
        })
    }
}

/// Poll the host measurement until it stops changing.
///
/// Two consecutive identical readings, or `MAX_SETTLE` elapsed. Capped because
/// some runtimes never shrink on their own and waiting forever would look like
/// a hang.
fn settle(
    store: &BackingStore,
    now_unix: i64,
    sink: &Arc<dyn EventSink>,
) -> crate::disk::HostUsage {
    use std::time::{Duration, Instant};
    const MAX_SETTLE: Duration = Duration::from_secs(15);
    const STEP: Duration = Duration::from_millis(500);

    let start = Instant::now();
    let mut last = store.measure(now_unix);
    let mut stable = 0;

    while start.elapsed() < MAX_SETTLE {
        std::thread::sleep(STEP);
        let next = store.measure(now_unix);
        if next.physical == last.physical && next.fs_free == last.fs_free {
            stable += 1;
            if stable >= 2 {
                return next;
            }
        } else {
            stable = 0;
        }
        last = next;
    }
    sink.emit(Event::Warning {
        code: "host_measurement_unsettled".into(),
        message: "the host figure was still moving after 15s; it is a snapshot, not a total".into(),
        resource: None,
    });
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DaemonIdentity;
    use crate::event::NullSink;
    use crate::model::{ResourceSummary, RuntimeFlavor, Totals};
    use crate::plan::Planner;
    use crate::scan::{Attributed, ScanReport};
    use std::sync::Mutex;

    const NOW: i64 = 1_800_000_000;
    const OLD: i64 = NOW - 90 * 24 * 3600;

    /// Records what it was asked to remove. Deleting nothing real.
    #[derive(Default)]
    struct SpyMutate {
        calls: Mutex<Vec<String>>,
        fail_with: Option<String>,
    }

    impl DockerMutate for SpyMutate {
        fn remove_volume(&self, name: &str) -> Result<()> {
            self.note(format!("volume:{name}"))
        }
        fn remove_image(&self, id: &str) -> Result<()> {
            self.note(format!("image:{id}"))
        }
        fn remove_container(&self, id: &str) -> Result<()> {
            self.note(format!("container:{id}"))
        }
        fn remove_network(&self, id: &str) -> Result<()> {
            self.note(format!("network:{id}"))
        }
        fn restore_volume(
            &self,
            name: &str,
            _labels: &std::collections::BTreeMap<String, String>,
            _tar: Vec<u8>,
        ) -> Result<()> {
            self.note(format!("restore:{name}"))
        }
        fn prune_build_cache(&self, _keep: u64) -> Result<Bytes> {
            self.calls.lock().unwrap().push("build_cache".into());
            Ok(Bytes(16_300_000_000))
        }
    }

    impl SpyMutate {
        fn note(&self, s: String) -> Result<()> {
            self.calls.lock().unwrap().push(s);
            match &self.fail_with {
                Some(m) => Err(crate::Error::Api(m.clone())),
                None => Ok(()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn attributed(r: ResourceSummary) -> Attributed {
        Attributed {
            resource: r,
            claims: vec![],
            owner: None,
            confidence: None,
            liveness: None,
            orphan_candidate: false,
            unattributed: false,
            content: None,
        }
    }

    fn network(name: &str) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Network, name, name);
        r.created_unix = Some(OLD);
        r.size = Some(Bytes(1000));
        r
    }

    fn report(resources: Vec<Attributed>) -> ScanReport {
        ScanReport {
            daemon: DaemonIdentity {
                id: DaemonId("D".into()),
                api_version: "1.54".into(),
                server_version: "29".into(),
                runtime: RuntimeFlavor::OrbStack,
                data_root: None,
                local: true,
                swarm_active: false,
            },
            context: "t".into(),
            totals: Totals {
                build_cache_records: 329,
                build_cache_bytes: Bytes(16_300_000_000),
                ..Default::default()
            },
            resources,
            projects_known: 0,
            duration_ms: 0,
            stale: false,
            warnings: vec![],
        }
    }

    #[test]
    fn dry_run_issues_no_mutating_call_at_all() {
        let rep = report(vec![attributed(network("proj_default"))]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions::default(), // Mode::DryRun
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert!(receipt.simulated);
        assert!(
            spy.calls().is_empty(),
            "dry run must not touch the daemon: {:?}",
            spy.calls()
        );
        assert_eq!(receipt.items[0].outcome, ItemOutcome::WouldDelete);
    }

    #[test]
    fn dry_run_executor_has_no_mutating_client_to_misuse() {
        let rep = report(vec![attributed(network("proj_default"))]);
        let plan = Planner::plan(&rep, NOW);
        let receipt = Executor::dry_run()
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply, // asked to apply, but cannot
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();
        assert!(
            receipt.simulated,
            "no client means simulation, never a delete"
        );
    }

    #[test]
    fn apply_deletes_free_items_and_prunes_the_cache() {
        let rep = report(vec![attributed(network("proj_default"))]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert!(!receipt.simulated);
        assert_eq!(receipt.deleted(), 1);
        let calls = spy.calls();
        assert!(calls.iter().any(|c| c.starts_with("network:")));
        assert!(calls.contains(&"build_cache".to_string()));
        assert_eq!(receipt.build_cache_reclaimed, Bytes(16_300_000_000));
    }

    #[test]
    fn protected_items_are_never_acted_on() {
        let rep = report(vec![attributed(network("bridge"))]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 0);
        assert!(!spy.calls().iter().any(|c| c.starts_with("network:")));
    }

    #[test]
    fn an_item_that_changed_since_planning_is_skipped_not_deleted() {
        let before = report(vec![attributed(network("n"))]);
        let plan = Planner::plan(&before, NOW);

        // By apply time something has attached to it.
        let mut busy = network("n");
        busy.in_use = true;
        let after = report(vec![attributed(busy)]);

        let spy = SpyMutate::default();
        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &after,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 0);
        assert_eq!(receipt.skipped(), 1);
        assert!(
            !spy.calls().iter().any(|c| c.starts_with("network:")),
            "a stale witness must never reach the daemon"
        );
    }

    #[test]
    fn a_daemon_refusal_is_recorded_not_retried() {
        let rep = report(vec![attributed(network("proj_default"))]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate {
            fail_with: Some("network proj_default is in use".into()),
            ..Default::default()
        };

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 0);
        assert_eq!(receipt.problems(), 1);
        assert!(matches!(receipt.items[0].outcome, ItemOutcome::Refused(_)));
        assert_eq!(
            spy.calls()
                .iter()
                .filter(|c| c.starts_with("network:"))
                .count(),
            1,
            "one attempt, no retry loop"
        );
    }

    #[test]
    fn only_label_is_a_fence_not_a_filter() {
        // Two networks, one labelled. Only the labelled one may be touched, and
        // the build cache must be left alone because it cannot carry a label.
        let mut tagged = network("tagged_default");
        tagged
            .labels
            .insert("dev.prunejuice.test".into(), "abc".into());
        let rep = report(vec![
            attributed(tagged),
            attributed(network("untagged_default")),
        ]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    only_label: Some(("dev.prunejuice.test".into(), "abc".into())),
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 1);
        let calls = spy.calls();
        assert!(calls.iter().any(|c| c.contains("tagged_default")));
        assert!(
            !calls.iter().any(|c| c.contains("untagged_default")),
            "an unlabelled resource must be unreachable, not merely deprioritised"
        );
        assert!(
            !calls.contains(&"build_cache".to_string()),
            "build cache carries no labels, so the fence cannot be honoured for it"
        );
    }

    /// Produces a real tar stream, or fails, on demand.
    struct VaultProber {
        fail: bool,
    }

    impl DockerProbe for VaultProber {
        fn probe_image(&self) -> Option<String> {
            Some("busybox".into())
        }
        fn probe_volumes(
            &self,
            _v: &[String],
        ) -> Result<std::collections::BTreeMap<String, crate::docker::RawProbe>> {
            Ok(Default::default())
        }
        fn dump_volume(&self, _name: &str, out: &mut dyn std::io::Write) -> Result<u64> {
            if self.fail {
                return Err(crate::Error::Api("volume vanished mid-dump".into()));
            }
            let mut b = tar::Builder::new(Vec::new());
            let mut h = tar::Header::new_gnu();
            h.set_path("data").unwrap();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, &b"abcd"[..]).unwrap();
            let data = b.into_inner().unwrap();
            out.write_all(&data).map_err(crate::Error::Io)?;
            Ok(data.len() as u64)
        }
    }

    fn tmp_vault(tag: &str) -> (Vault, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!(
            "pj-exec-vault-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (Vault::at(d.clone()), d)
    }

    /// An unreferenced volume the probe says holds a database: irreversible,
    /// and therefore the vault's whole reason for existing.
    fn precious_volume(name: &str) -> Attributed {
        let mut r = ResourceSummary::new(ResourceKind::Volume, name, name);
        r.created_unix = Some(OLD);
        r.size = Some(Bytes(4096));
        let mut a = attributed(r);
        a.orphan_candidate = true;
        a.owner = Some("gone".into());
        a.content = Some(crate::probe::ContentReport {
            class: crate::probe::ContentClass::Database(crate::probe::Engine::MariaDb),
            method: crate::probe::ProbeMethod::Container,
            entries: vec!["ibdata1".into()],
            file_count: 1,
            truncated: false,
            bytes: Bytes(4096),
            newest_mtime: Some(OLD),
        });
        a
    }

    fn orphan_opts() -> ExecuteOptions {
        ExecuteOptions {
            mode: Mode::Apply,
            tiers: vec![Tier::Orphan],
            ..Default::default()
        }
    }

    #[test]
    fn an_irreversible_volume_is_preserved_before_it_is_removed() {
        let (vault, dir) = tmp_vault("ok");
        let rep = report(vec![precious_volume("nbk_mysql")]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();
        let prober = VaultProber { fail: false };

        let receipt = Executor::applying(&spy)
            .with_vault(&prober, &vault)
            .run(
                plan,
                &rep,
                NOW,
                &orphan_opts(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 1);
        assert_eq!(receipt.problems(), 0);
        assert_eq!(receipt.preserved().len(), 1, "{:?}", receipt.items);

        // The copy must actually be on disk and verify.
        let entries = vault.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].volume, "nbk_mysql");
        assert_eq!(entries[0].engine, Some(crate::probe::Engine::MariaDb));
        assert!(vault.verify(&entries[0]).is_ok());
        assert!(spy.calls().iter().any(|c| c == "volume:nbk_mysql"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_dump_means_the_volume_is_not_deleted() {
        // The difference between a backup and the appearance of one. If the
        // copy cannot be made, the original stays.
        let (vault, dir) = tmp_vault("fail");
        let rep = report(vec![precious_volume("precious")]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();
        let prober = VaultProber { fail: true };

        let receipt = Executor::applying(&spy)
            .with_vault(&prober, &vault)
            .run(
                plan,
                &rep,
                NOW,
                &orphan_opts(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 0);
        assert_eq!(receipt.problems(), 1);
        assert!(matches!(
            receipt.items[0].outcome,
            ItemOutcome::PreserveFailed(_)
        ));
        assert!(
            !spy.calls().iter().any(|c| c.starts_with("volume:")),
            "the daemon must never be asked to delete something we failed to preserve"
        );
        assert!(vault.entries().unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_vault_configured_refuses_rather_than_deleting_unpreserved() {
        let rep = report(vec![precious_volume("unprotected")]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &orphan_opts(),
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 0);
        assert!(matches!(receipt.items[0].outcome, ItemOutcome::Refused(_)));
        assert!(!spy.calls().iter().any(|c| c.starts_with("volume:")));
    }

    #[test]
    fn opting_out_of_the_vault_is_not_consent_to_lose_data() {
        let (vault, dir) = tmp_vault("optout");
        let rep = report(vec![precious_volume("optout")]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();
        let prober = VaultProber { fail: false };

        let receipt = Executor::applying(&spy)
            .with_vault(&prober, &vault)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    vault: false,
                    ..orphan_opts()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 0);
        assert!(matches!(receipt.items[0].outcome, ItemOutcome::Refused(_)));
        assert!(!spy.calls().iter().any(|c| c.starts_with("volume:")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_reversible_item_needs_no_vault_at_all() {
        // Only irreversible items pay the cost. A network is recreated by the
        // next compose up.
        let rep = report(vec![attributed(network("proj_default"))]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();
        assert_eq!(receipt.deleted(), 1);
        assert!(receipt.preserved().is_empty());
    }

    #[test]
    fn only_names_is_a_fence_over_individual_rows() {
        // What the review screen needs: the user ticked one row, so only that
        // row may be reached — and the build cache, which no name can cover,
        // is left alone.
        let rep = report(vec![
            attributed(network("picked_default")),
            attributed(network("ignored_default")),
        ]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();

        let receipt = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    only_names: Some(["picked_default".to_string()].into_iter().collect()),
                    ..Default::default()
                },
                Arc::new(NullSink),
                &Cancel::new(),
            )
            .unwrap();

        assert_eq!(receipt.deleted(), 1);
        let calls = spy.calls();
        assert!(calls.iter().any(|c| c.contains("picked_default")));
        assert!(
            !calls.iter().any(|c| c.contains("ignored_default")),
            "an unticked row must be unreachable"
        );
        assert!(!calls.contains(&"build_cache".to_string()));
    }

    #[test]
    fn cancellation_stops_before_the_next_delete() {
        let rep = report(vec![
            attributed(network("a_default")),
            attributed(network("b_default")),
        ]);
        let plan = Planner::plan(&rep, NOW);
        let spy = SpyMutate::default();
        let cancel = Cancel::new();
        cancel.cancel();

        let err = Executor::applying(&spy)
            .run(
                plan,
                &rep,
                NOW,
                &ExecuteOptions {
                    mode: Mode::Apply,
                    ..Default::default()
                },
                Arc::new(NullSink),
                &cancel,
            )
            .unwrap_err();

        assert!(matches!(err, crate::Error::Cancelled));
        assert!(spy.calls().is_empty());
    }
}
