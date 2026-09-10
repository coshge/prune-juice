//! Planning, and the proof objects that gate deletion.
//!
//! The safety argument is encoded in the type system rather than asserted in a
//! comment:
//!
//! * [`SafeToDelete`] can only be constructed by [`Planner::plan`]. Its fields
//!   are private and there is no public constructor, so no other code path can
//!   fabricate permission to delete something.
//! * [`Fresh`] can only be produced by [`SafeToDelete::revalidate`], and
//!   `execute` consumes it. A witness therefore cannot outlive the check that
//!   produced it.
//!
//! The result is that "we revalidated immediately before deleting" is not a
//! discipline anyone has to remember — it is the only way the code compiles.

pub mod evidence;
pub mod graph;
pub mod tier;

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::model::{Bytes, DaemonId, ResourceId, ResourceKind};
use crate::providers::best_claim;
use crate::scan::ScanReport;

use evidence::ReadSet;
use graph::RefGraph;
use tier::{classify, Reversibility, Tier, Verdict};

/// Permission to delete exactly one resource, tied to the evidence that
/// justified it and the daemon it was proven against.
///
/// Not `Clone`, not `Copy`: a witness is consumed by the act it authorises.
#[derive(Debug, Serialize, Deserialize)]
pub struct SafeToDelete {
    resource: ResourceId,
    kind: ResourceKind,
    name: String,
    tier: Tier,
    evidence_hash: String,
    daemon: DaemonId,
    proven_at: i64,
}

impl SafeToDelete {
    pub fn resource(&self) -> &ResourceId {
        &self.resource
    }
    pub fn kind(&self) -> ResourceKind {
        self.kind
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn tier(&self) -> Tier {
        self.tier
    }
    pub fn evidence_hash(&self) -> &str {
        &self.evidence_hash
    }

    /// Re-derive the evidence against a freshly taken world and confirm it is
    /// byte-for-byte what we planned on.
    ///
    /// Must be called immediately before the delete, per item — not once per
    /// batch. The window this closes is the one where `docker compose up` lands
    /// between planning and applying.
    pub fn revalidate(self, now: &ScanReport, now_unix: i64) -> Result<Fresh, Staleness> {
        if now.daemon.id != self.daemon {
            return Err(Staleness::DifferentDaemon {
                planned: self.daemon.0.clone(),
                found: now.daemon.id.0.clone(),
            });
        }

        let Some(a) = now
            .resources
            .iter()
            .find(|a| a.resource.id == self.resource && a.resource.kind == self.kind)
        else {
            return Err(Staleness::Vanished {
                name: self.name.clone(),
            });
        };

        let graph = RefGraph::build(now);
        let mut rs = ReadSet::new();
        let verdict = classify(a, &graph, now_unix, &mut rs);

        if verdict.tier != self.tier {
            return Err(Staleness::TierChanged {
                name: self.name.clone(),
                from: self.tier,
                to: verdict.tier,
            });
        }
        let fresh_hash = rs.hash_hex();
        if fresh_hash != self.evidence_hash {
            return Err(Staleness::EvidenceChanged {
                name: self.name.clone(),
                was: self.evidence_hash.clone(),
                now: fresh_hash,
            });
        }

        Ok(Fresh(self))
    }
}

/// A witness that has just been revalidated. The only thing `execute` accepts.
#[derive(Debug)]
pub struct Fresh(SafeToDelete);

impl Fresh {
    pub fn get(&self) -> &SafeToDelete {
        &self.0
    }
    /// Consume the witness. Deliberately crate-visible: only the executor may
    /// cash one in.
    pub(crate) fn into_inner(self) -> SafeToDelete {
        self.0
    }
}

/// Why a witness is no longer good.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum Staleness {
    /// The plan was made against a different engine entirely.
    DifferentDaemon { planned: String, found: String },
    /// Something else removed it first. Not an error, just nothing to do.
    Vanished { name: String },
    /// It is no longer as safe as it was.
    TierChanged { name: String, from: Tier, to: Tier },
    /// A fact the decision rested on has moved.
    EvidenceChanged {
        name: String,
        was: String,
        now: String,
    },
}

impl std::fmt::Display for Staleness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Staleness::DifferentDaemon { planned, found } => {
                write!(
                    f,
                    "plan was made against daemon {planned}, but this is {found}"
                )
            }
            Staleness::Vanished { name } => write!(f, "{name} is already gone"),
            Staleness::TierChanged { name, from, to } => write!(
                f,
                "{name} moved from {} to {} since planning",
                from.as_str(),
                to.as_str()
            ),
            Staleness::EvidenceChanged { name, .. } => {
                write!(f, "{name} changed since planning — re-scan before applying")
            }
        }
    }
}

/// One resource, its verdict, and (when deletable) the permission to act.
#[derive(Debug, Serialize, Deserialize)]
pub struct PlanItem {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub name: String,
    pub size: Option<Bytes>,
    pub owner: Option<String>,
    /// Carried so `--only-label` can be enforced at apply time rather than
    /// merely documented.
    pub labels: BTreeMap<String, String>,
    /// Database engine detected by the probe, recorded in the vault manifest so
    /// a restore knows what it is holding.
    pub engine: Option<crate::probe::Engine>,
    pub verdict: Verdict,
    /// The facts the verdict rested on. Hashed for staleness detection; not
    /// meant for humans.
    pub evidence: ReadSet,
    /// Citable provenance, one line per fact, already tagged with its source —
    /// `[label] com.docker.compose.project = nbk`. This is what a person reads;
    /// `evidence` is what the machine compares.
    pub provenance: Vec<String>,
    /// Present only for tiers that may be acted on. `Protected` never has one.
    witness: Option<SafeToDelete>,
}

impl PlanItem {
    pub fn witness(&self) -> Option<&SafeToDelete> {
        self.witness.as_ref()
    }
    pub fn take_witness(&mut self) -> Option<SafeToDelete> {
        self.witness.take()
    }
    pub fn tier(&self) -> Tier {
        self.verdict.tier
    }
    pub fn reversibility(&self) -> &Reversibility {
        &self.verdict.reversibility
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub daemon: DaemonId,
    pub created_unix: i64,
    pub items: Vec<PlanItem>,
    /// Build cache is not a per-resource thing in our model; the daemon exposes
    /// it only in aggregate.
    pub build_cache_records: u32,
    pub build_cache_reclaimable: Bytes,
}

impl Plan {
    pub fn of_tier(&self, t: Tier) -> impl Iterator<Item = &PlanItem> {
        self.items.iter().filter(move |i| i.verdict.tier == t)
    }

    /// Bytes in a tier. Sizes we do not have are counted as zero and the caller
    /// is expected to say the figure is a floor, never to guess.
    pub fn bytes_of_tier(&self, t: Tier) -> Bytes {
        self.of_tier(t).filter_map(|i| i.size).sum()
    }

    /// Everything Tier 1 would reclaim, including the build cache.
    pub fn free_bytes(&self) -> Bytes {
        self.bytes_of_tier(Tier::Free) + self.build_cache_reclaimable
    }

    /// Bytes in Tier 1 that could not be undone. The Tier 1 Theorem says this
    /// is always zero; it is computed rather than assumed so the claim is
    /// checked on real data every run.
    pub fn irreversible_free_bytes(&self) -> Bytes {
        self.of_tier(Tier::Free)
            .filter(|i| i.verdict.reversibility.is_gone())
            .filter_map(|i| i.size)
            .sum()
    }
}

pub struct Planner;

impl Planner {
    /// Pure: no IO, no clock, no daemon calls. Everything it needs is in the
    /// report, which is what makes the invariants property-testable.
    pub fn plan(report: &ScanReport, now_unix: i64) -> Plan {
        let graph = RefGraph::build(report);
        let mut items = Vec::with_capacity(report.resources.len());

        for a in &report.resources {
            let mut rs = ReadSet::new();
            let verdict = classify(a, &graph, now_unix, &mut rs);

            // A witness exists only for tiers that may be acted on, and only
            // the planner can mint one.
            let witness = match verdict.tier {
                Tier::Free | Tier::Orphan | Tier::Stale => Some(SafeToDelete {
                    resource: a.resource.id.clone(),
                    kind: a.resource.kind,
                    name: a.resource.name.clone(),
                    tier: verdict.tier,
                    evidence_hash: rs.hash_hex(),
                    daemon: report.daemon.id.clone(),
                    proven_at: now_unix,
                }),
                Tier::Protected | Tier::Unattributed => None,
            };

            items.push(PlanItem {
                id: a.resource.id.clone(),
                kind: a.resource.kind,
                name: a.resource.name.clone(),
                size: a.resource.size,
                owner: a.owner.clone(),
                labels: a.resource.labels.clone(),
                engine: a.content.as_ref().and_then(|c| match &c.class {
                    crate::probe::ContentClass::Database(e) => Some(*e),
                    _ => None,
                }),
                provenance: best_claim(&a.claims)
                    .map(|c| {
                        c.evidence
                            .iter()
                            .map(|e| format!("[{}] {}", e.source.tag(), e.detail))
                            .collect()
                    })
                    .unwrap_or_default(),
                verdict,
                evidence: rs,
                witness,
            });
        }

        Plan {
            daemon: report.daemon.id.clone(),
            created_unix: now_unix,
            items,
            build_cache_records: report.totals.build_cache_records,
            build_cache_reclaimable: report.totals.build_cache_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DaemonIdentity;
    use crate::model::{ResourceSummary, RuntimeFlavor, Totals};
    use crate::scan::Attributed;

    const NOW: i64 = 1_800_000_000;
    const OLD: i64 = NOW - 90 * 24 * 3600;

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

    fn report_with(resources: Vec<Attributed>, daemon: &str) -> ScanReport {
        ScanReport {
            daemon: DaemonIdentity {
                id: DaemonId(daemon.into()),
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

    fn network(name: &str) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Network, name, name);
        r.created_unix = Some(OLD);
        r
    }

    #[test]
    fn protected_items_get_no_witness() {
        let rep = report_with(vec![attributed(network("bridge"))], "D");
        let plan = Planner::plan(&rep, NOW);
        assert_eq!(plan.items[0].tier(), Tier::Protected);
        assert!(
            plan.items[0].witness().is_none(),
            "there must be no permission object for something we will never touch"
        );
    }

    #[test]
    fn free_items_get_a_witness() {
        let rep = report_with(vec![attributed(network("proj_default"))], "D");
        let plan = Planner::plan(&rep, NOW);
        assert_eq!(plan.items[0].tier(), Tier::Free);
        assert!(plan.items[0].witness().is_some());
    }

    #[test]
    fn tier_one_is_never_irreversible() {
        // The Tier 1 Theorem, checked rather than asserted.
        let rep = report_with(
            vec![
                attributed(network("a_default")),
                attributed(network("b_default")),
            ],
            "D",
        );
        let plan = Planner::plan(&rep, NOW);
        assert_eq!(plan.irreversible_free_bytes(), Bytes::ZERO);
    }

    #[test]
    fn revalidation_passes_against_an_unchanged_world() {
        let rep = report_with(vec![attributed(network("n"))], "D");
        let mut plan = Planner::plan(&rep, NOW);
        let w = plan.items[0].take_witness().unwrap();
        assert!(w.revalidate(&rep, NOW).is_ok());
    }

    #[test]
    fn revalidation_refuses_a_different_daemon() {
        // The plan was made against one engine; we are now pointed at another.
        let rep = report_with(vec![attributed(network("n"))], "D");
        let mut plan = Planner::plan(&rep, NOW);
        let w = plan.items[0].take_witness().unwrap();

        let other = report_with(vec![attributed(network("n"))], "OTHER");
        assert!(matches!(
            w.revalidate(&other, NOW),
            Err(Staleness::DifferentDaemon { .. })
        ));
    }

    #[test]
    fn revalidation_refuses_when_the_world_moved() {
        // Planned while unreferenced; by apply time a container has attached.
        let rep = report_with(vec![attributed(network("n"))], "D");
        let mut plan = Planner::plan(&rep, NOW);
        let w = plan.items[0].take_witness().unwrap();

        let mut busy = network("n");
        busy.in_use = true;
        let after = report_with(vec![attributed(busy)], "D");

        let err = w.revalidate(&after, NOW).unwrap_err();
        assert!(
            matches!(
                err,
                Staleness::TierChanged { .. } | Staleness::EvidenceChanged { .. }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn revalidation_reports_a_vanished_resource_without_error_noise() {
        let rep = report_with(vec![attributed(network("n"))], "D");
        let mut plan = Planner::plan(&rep, NOW);
        let w = plan.items[0].take_witness().unwrap();

        let empty = report_with(vec![], "D");
        assert!(matches!(
            w.revalidate(&empty, NOW),
            Err(Staleness::Vanished { .. })
        ));
    }

    #[test]
    fn planning_is_deterministic() {
        let rep = report_with(vec![attributed(network("n"))], "D");
        let a = Planner::plan(&rep, NOW);
        let b = Planner::plan(&rep, NOW);
        assert_eq!(
            a.items[0].evidence.hash(),
            b.items[0].evidence.hash(),
            "the same world must yield the same evidence"
        );
    }

    #[test]
    fn free_bytes_include_the_build_cache() {
        let rep = report_with(vec![attributed(network("n"))], "D");
        let plan = Planner::plan(&rep, NOW);
        assert_eq!(plan.free_bytes(), Bytes(16_300_000_000));
    }
}
