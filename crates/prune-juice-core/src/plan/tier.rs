//! Tier classification.
//!
//! > **Tier 1 Theorem.** For every item in a Tier 1 batch, deleting it destroys
//! > no information that cannot be recovered from something that still exists
//! > after the deletion.
//!
//! That is the claim behind "one action, no confirmation", and it is a
//! conjunction of three independent properties, not just "unreferenced":
//!
//! 1. **Unreferenced** — proven, with `Unknown` blocking.
//! 2. **Reconstructible** — the bytes come back from something that survives,
//!    or were worthless.
//! 3. **Tier-stable** — deleting it changes no other resource's tier or
//!    provenance.
//!
//! Property 3 is the one that gets forgotten. Removing exited containers is
//! safe by 1 and 2, and silently converts attributable volumes into
//! unattributable ones. That is a safety event, not housekeeping.

use serde::{Deserialize, Serialize};

use crate::model::ResourceKind;
use crate::scan::Attributed;

use super::evidence::ReadSet;
use super::graph::{RefGraph, Referenced};

/// Nothing younger than this may enter Tier 1.
///
/// A blunt instrument, and the right one: it catches the volume created seconds
/// ago by a container that is still starting up, and the resource held by a
/// referrer we simply cannot see.
pub const MIN_AGE_SECS: i64 = 24 * 60 * 60;

/// A volume written this recently is in use by *something*, whatever the graph
/// says.
pub const RECENT_WRITE_SECS: i64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Never offered. Live, tagged, pinned or waived.
    Protected,
    /// One action, no confirmation. The theorem above holds.
    Free,
    /// A confident claim on a project that is gone. Per-item confirm.
    Orphan,
    /// A referrer exists but looks dormant. Strong confirm, preserve first.
    Stale,
    /// We do not know who owns this. Never offered for deletion.
    Unattributed,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Protected => "protected",
            Tier::Free => "free",
            Tier::Orphan => "orphan",
            Tier::Stale => "stale",
            Tier::Unattributed => "unattributed",
        }
    }

    /// Only Tier 1 may act without asking.
    pub fn needs_confirmation(self) -> bool {
        self != Tier::Free
    }
}

/// What "undo" can honestly mean for a resource.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Reversibility {
    /// The exact bytes come back — from a registry digest or a vault entry.
    Restorable(String),
    /// Derivable again. Costs time, never data.
    Rebuildable(String),
    /// Gone. No exceptions, no soft language.
    Gone,
}

impl Reversibility {
    pub fn is_gone(&self) -> bool {
        matches!(self, Reversibility::Gone)
    }
}

/// Why a resource landed in its tier. Every line is citable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verdict {
    pub tier: Tier,
    pub reversibility: Reversibility,
    /// The single sentence that most explains the tier.
    pub because: String,
    pub referenced: Referenced,
}

/// Classify one resource, recording every fact consulted.
///
/// `now_unix` is passed rather than read so the classifier stays pure and
/// testable — time is an input, not an ambient effect.
pub fn classify(a: &Attributed, graph: &RefGraph, now_unix: i64, rs: &mut ReadSet) -> Verdict {
    let r = &a.resource;
    let subject = format!("{}:{}", r.kind.as_str(), r.name);

    let referenced = graph.referenced(a, rs);
    let reversibility = reversibility_of(a, rs);

    // --- T0: anything a live container holds -----------------------------
    if let Referenced::Yes { by } = &referenced {
        let live = by
            .iter()
            .any(|x| x.kind == super::graph::ReferrerKind::LiveContainer);
        rs.record(&subject, "held_by_live_container", live.to_string());
        if live {
            return Verdict {
                tier: Tier::Protected,
                reversibility,
                because: "held by a running container".into(),
                referenced,
            };
        }
    }

    // A tagged image is a statement of intent, not garbage.
    if r.kind == ResourceKind::Image && !r.repo_tags.is_empty() {
        rs.record(&subject, "repo_tags", r.repo_tags.join(","));
        return Verdict {
            tier: Tier::Protected,
            reversibility,
            because: format!("tagged {}", r.repo_tags.join(", ")),
            referenced,
        };
    }

    // Docker's own networks are not ours to remove.
    if r.kind == ResourceKind::Network && matches!(r.name.as_str(), "bridge" | "host" | "none") {
        return Verdict {
            tier: Tier::Protected,
            reversibility,
            because: "a built-in Docker network".into(),
            referenced,
        };
    }

    // --- Unknown blocks everything ---------------------------------------
    if let Referenced::Unknown { because } = &referenced {
        let why = because
            .first()
            .map(|o| o.describe())
            .unwrap_or_else(|| "something could not be seen".into());
        rs.record(&subject, "referenced", "unknown");
        return Verdict {
            tier: Tier::Unattributed,
            reversibility,
            because: why,
            referenced,
        };
    }

    // --- Still referenced: orphan or stale, never free --------------------
    if referenced.is_yes() {
        if a.orphan_candidate {
            return Verdict {
                tier: Tier::Orphan,
                reversibility,
                because: format!(
                    "belongs to \"{}\", whose directory is gone",
                    a.owner.as_deref().unwrap_or("?")
                ),
                referenced,
            };
        }
        return Verdict {
            tier: Tier::Protected,
            reversibility,
            because: "still referenced".into(),
            referenced,
        };
    }

    // --- Unreferenced. Now the other two properties. -----------------------
    debug_assert!(referenced.is_no());

    if a.orphan_candidate {
        return Verdict {
            tier: Tier::Orphan,
            reversibility,
            because: format!(
                "belongs to \"{}\", whose directory is gone",
                a.owner.as_deref().unwrap_or("?")
            ),
            referenced,
        };
    }

    // Property 2: irreversible loss can never be a no-confirmation action.
    if reversibility.is_gone() {
        rs.record(&subject, "reversibility", "gone");
        return Verdict {
            tier: if a.unattributed {
                Tier::Unattributed
            } else {
                Tier::Stale
            },
            reversibility,
            because: "deleting this would be irreversible".into(),
            referenced,
        };
    }

    // Age gate.
    //
    // Record the *decision*, not the raw elapsed time. A read-set must contain
    // stable facts: `age_secs` ticks upward between the planning scan and the
    // pre-apply re-scan, so hashing it made every witness stale on arrival and
    // nothing could ever be applied. The creation timestamp is fixed, and
    // whether it cleared the gate is what the verdict actually rested on.
    if let Some(created) = r.created_unix {
        let age = now_unix - created;
        rs.record(&subject, "created_unix", created.to_string());
        rs.record(
            &subject,
            "age_gate",
            if age >= MIN_AGE_SECS {
                "passed"
            } else {
                "failed"
            },
        );
        if age < MIN_AGE_SECS {
            return Verdict {
                tier: Tier::Stale,
                reversibility,
                because: format!("only {age}s old — too new to be sure"),
                referenced,
            };
        }
    } else {
        // No creation time means we cannot apply the age gate at all, and an
        // ungated deletion is exactly what the gate exists to prevent.
        rs.record(&subject, "created_unix", "unknown");
        return Verdict {
            tier: Tier::Unattributed,
            reversibility,
            because: "no creation time, so the age gate cannot be applied".into(),
            referenced,
        };
    }

    // Property 3: tier-stability.
    if let Some(reason) = destabilises(a, rs) {
        return Verdict {
            tier: Tier::Stale,
            reversibility,
            because: reason,
            referenced,
        };
    }

    Verdict {
        tier: Tier::Free,
        reversibility,
        because: "unreferenced, reconstructible, and nothing else depends on it".into(),
        referenced,
    }
}

/// Would removing this resource degrade what we know about another?
///
/// Returns the reason it must not be Tier 1, or `None` if it is inert.
fn destabilises(a: &Attributed, rs: &mut ReadSet) -> Option<String> {
    let r = &a.resource;
    let subject = format!("{}:{}", r.kind.as_str(), r.name);

    if r.kind == ResourceKind::Container {
        // THE ordering constraint. A container's mount list is the only
        // surviving link from an anonymous volume to a project; `docker
        // container prune` destroys it permanently. Until that mapping is
        // committed to the index, removing the container is a provenance loss,
        // not housekeeping.
        rs.record(&subject, "mount_count", r.mounts.len().to_string());
        if !r.mounts.is_empty() {
            return Some(format!(
                "removing it would orphan {} volume(s) whose provenance it carries",
                r.mounts.len()
            ));
        }

        // A non-empty writable layer holds data that lives in no volume.
        if let Some(rw) = r.size_rw {
            rs.record(&subject, "size_rw", rw.get().to_string());
            if rw.get() > 50_000_000 {
                return Some(format!(
                    "its writable layer holds {} that exists nowhere else",
                    rw.human()
                ));
            }
        }
    }

    // A volume is never Tier 1 in this build. Without the content probe we
    // cannot tell an empty scratch volume from a Postgres data directory, and
    // `docker run postgres` with no -v produces exactly the unreferenced,
    // unlabelled shape that would otherwise sail through.
    if r.kind == ResourceKind::Volume {
        rs.record(&subject, "probe", "not-implemented");
        return Some("volume contents have not been inspected, so it cannot be proven safe".into());
    }

    None
}

fn reversibility_of(a: &Attributed, rs: &mut ReadSet) -> Reversibility {
    let r = &a.resource;
    let subject = format!("{}:{}", r.kind.as_str(), r.name);

    match r.kind {
        ResourceKind::BuildCache => {
            Reversibility::Rebuildable("re-derived on the next build".into())
        }
        ResourceKind::Network => Reversibility::Rebuildable("recreated on next compose up".into()),
        ResourceKind::Image => {
            if !r.repo_digests.is_empty() {
                rs.record(&subject, "repo_digests", r.repo_digests.len().to_string());
                Reversibility::Restorable(format!("docker pull {}", r.repo_digests[0]))
            } else {
                // Built locally, and we have not confirmed the build context
                // still exists. Assume the worst.
                Reversibility::Gone
            }
        }
        ResourceKind::Container => {
            let rw = r.size_rw.map(|b| b.get()).unwrap_or(0);
            rs.record(&subject, "size_rw", rw.to_string());
            if rw > 0 {
                Reversibility::Gone
            } else {
                Reversibility::Rebuildable("recreated on next compose up".into())
            }
        }
        // Until a verified vault copy exists, deleting a volume is final. There
        // is no trash can for volumes: Docker has no rename, and on macOS the
        // data lives inside a VM the host cannot reach.
        ResourceKind::Volume => Reversibility::Gone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DaemonIdentity;
    use crate::model::{Bytes, ContainerState, DaemonId, ResourceSummary, RuntimeFlavor, Totals};
    use crate::scan::ScanReport;

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
        }
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
            totals: Totals::default(),
            resources,
            projects_known: 0,
            duration_ms: 0,
            stale: false,
            warnings: vec![],
        }
    }

    fn classify_one(rep: &ScanReport, idx: usize) -> Verdict {
        let g = RefGraph::build(rep);
        let mut rs = ReadSet::new();
        classify(&rep.resources[idx], &g, NOW, &mut rs)
    }

    fn network(name: &str, created: i64) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Network, name, name);
        r.created_unix = Some(created);
        r
    }

    fn container(name: &str, state: ContainerState, mounts: &[&str]) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Container, name, name);
        r.state = Some(state);
        r.created_unix = Some(OLD);
        r.mounts = mounts.iter().map(|s| s.to_string()).collect();
        r
    }

    #[test]
    fn an_unused_old_network_is_free() {
        let rep = report(vec![attributed(network("proj_default", OLD))]);
        assert_eq!(classify_one(&rep, 0).tier, Tier::Free);
    }

    #[test]
    fn built_in_networks_are_protected() {
        for n in ["bridge", "host", "none"] {
            let rep = report(vec![attributed(network(n, OLD))]);
            assert_eq!(classify_one(&rep, 0).tier, Tier::Protected, "{n}");
        }
    }

    #[test]
    fn nothing_younger_than_a_day_is_free() {
        let rep = report(vec![attributed(network("fresh", NOW - 600))]);
        let v = classify_one(&rep, 0);
        assert_eq!(v.tier, Tier::Stale);
        assert!(v.because.contains("too new"));
    }

    #[test]
    fn a_resource_with_no_creation_time_cannot_pass_the_age_gate() {
        let mut n = network("timeless", OLD);
        n.created_unix = None;
        let rep = report(vec![attributed(n)]);
        assert_eq!(classify_one(&rep, 0).tier, Tier::Unattributed);
    }

    #[test]
    fn no_volume_is_ever_free_without_a_content_probe() {
        // `docker run postgres` with no -v yields exactly this shape:
        // unreferenced, unlabelled, and holding a real database.
        let mut v = ResourceSummary::new(ResourceKind::Volume, "x", "a".repeat(64));
        v.created_unix = Some(OLD);
        let rep = report(vec![attributed(v)]);
        let got = classify_one(&rep, 0);
        assert_ne!(got.tier, Tier::Free);
        assert_eq!(got.reversibility, Reversibility::Gone);
    }

    #[test]
    fn a_container_carrying_volume_provenance_is_not_free() {
        // Deleting this container destroys the only link between the anonymous
        // volume and its project. Safe by "unreferenced", unsafe by
        // tier-stability.
        let rep = report(vec![
            attributed(container("old", ContainerState::Exited, &["anon123"])),
            attributed(ResourceSummary::new(ResourceKind::Volume, "v", "anon123")),
        ]);
        let v = classify_one(&rep, 0);
        assert_eq!(v.tier, Tier::Stale);
        assert!(v.because.contains("orphan"), "{}", v.because);
    }

    #[test]
    fn an_inert_exited_container_is_free() {
        let rep = report(vec![attributed(container(
            "inert",
            ContainerState::Exited,
            &[],
        ))]);
        assert_eq!(classify_one(&rep, 0).tier, Tier::Free);
    }

    #[test]
    fn a_container_with_a_fat_writable_layer_is_not_free() {
        let mut c = container("fat", ContainerState::Exited, &[]);
        c.size_rw = Some(Bytes(200_000_000));
        let rep = report(vec![attributed(c)]);
        let v = classify_one(&rep, 0);
        assert_ne!(v.tier, Tier::Free);
        assert_eq!(v.reversibility, Reversibility::Gone);
    }

    #[test]
    fn a_live_container_protects_its_volume() {
        let rep = report(vec![
            attributed(container("live", ContainerState::Running, &["hot"])),
            attributed(ResourceSummary::new(ResourceKind::Volume, "hot", "hot")),
        ]);
        assert_eq!(classify_one(&rep, 1).tier, Tier::Protected);
    }

    #[test]
    fn images_are_never_free_while_layers_are_unproven() {
        let mut i = ResourceSummary::new(ResourceKind::Image, "sha256:x", "dangling");
        i.created_unix = Some(OLD);
        let rep = report(vec![attributed(i)]);
        assert_ne!(classify_one(&rep, 0).tier, Tier::Free);
    }

    #[test]
    fn a_tagged_image_is_protected() {
        let mut i = ResourceSummary::new(ResourceKind::Image, "sha256:y", "app:latest");
        i.created_unix = Some(OLD);
        i.repo_tags = vec!["app:latest".into()];
        let rep = report(vec![attributed(i)]);
        assert_eq!(classify_one(&rep, 0).tier, Tier::Protected);
    }

    #[test]
    fn classification_always_records_what_it_consulted() {
        let rep = report(vec![attributed(network("n", OLD))]);
        let g = RefGraph::build(&rep);
        let mut rs = ReadSet::new();
        classify(&rep.resources[0], &g, NOW, &mut rs);
        assert!(!rs.is_empty());
        // And the hash must be reproducible for the same inputs.
        let mut rs2 = ReadSet::new();
        classify(&rep.resources[0], &g, NOW, &mut rs2);
        assert_eq!(rs.hash(), rs2.hash());
    }

    #[test]
    fn evidence_is_stable_across_the_gap_between_planning_and_applying() {
        // A real bug this guards: recording raw elapsed seconds made every
        // witness stale by the time the pre-apply re-scan finished, so nothing
        // could ever be applied.
        let rep = report(vec![attributed(network("n", OLD))]);
        let g = RefGraph::build(&rep);

        let mut planned = ReadSet::new();
        classify(&rep.resources[0], &g, NOW, &mut planned);

        // Six seconds later — a realistic scan duration.
        let mut revalidated = ReadSet::new();
        classify(&rep.resources[0], &g, NOW + 6, &mut revalidated);

        assert_eq!(
            planned.hash(),
            revalidated.hash(),
            "the passage of time alone must not invalidate a plan"
        );
    }

    #[test]
    fn crossing_the_age_gate_does_change_the_evidence() {
        // The flip side: if a resource actually crosses the threshold, that is
        // a real change and the hash must move.
        let fresh = network("n", NOW - 10);
        let rep = report(vec![attributed(fresh)]);
        let g = RefGraph::build(&rep);

        let mut before = ReadSet::new();
        classify(&rep.resources[0], &g, NOW, &mut before);

        let mut after = ReadSet::new();
        classify(&rep.resources[0], &g, NOW + MIN_AGE_SECS, &mut after);

        assert_ne!(before.hash(), after.hash());
    }

    #[test]
    fn only_free_skips_confirmation() {
        assert!(!Tier::Free.needs_confirmation());
        for t in [
            Tier::Protected,
            Tier::Orphan,
            Tier::Stale,
            Tier::Unattributed,
        ] {
            assert!(t.needs_confirmation(), "{t:?}");
        }
    }
}
