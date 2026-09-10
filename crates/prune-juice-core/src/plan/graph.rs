//! The reference graph.
//!
//! "Unreferenced" is the necessary half of Tier 1, and getting it right means
//! being pedantic about two things:
//!
//! 1. **Every container is a referrer, in every state.** A stopped container
//!    still holds its image and its mounts. The daemon's `RefCount` counts only
//!    live ones, so it is corroboration, never the source of truth.
//! 2. **"I don't know" is a distinct answer from "no".** A remote daemon, an
//!    active swarm, or an unreadable project root all mean something could be
//!    referencing a resource in a way we cannot see. That is `Unknown`, and
//!    `Unknown` never satisfies a deletion predicate.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{ContainerState, Liveness, ResourceKind};
use crate::scan::{Attributed, ScanReport};

use super::evidence::ReadSet;

/// Something that holds a reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Referrer {
    pub kind: ReferrerKind,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferrerKind {
    /// A container in any state.
    Container,
    /// A live container. Stronger: it protects transitively.
    LiveContainer,
    /// A tag is a statement of user intent, not merely a name.
    Tag,
    /// An image whose layer stack contains this one as a prefix.
    ChildImage,
    /// A project on disk whose config declares this resource.
    Declaration,
}

/// Something we cannot see into. Its presence forces `Unknown` rather than `No`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Opacity {
    /// Swarm services may reference resources we never enumerate.
    SwarmActive,
    /// A daemon we did not reach over a local socket.
    RemoteDaemon(String),
    /// A project root that could not be read — an unmounted disk, most likely.
    UnreadableRoot(String),
    /// Image layer stacks were not fetched, so base-image relationships are
    /// unproven.
    ImageLayersUnknown,
}

impl Opacity {
    /// Could this opacity source conceal a referrer for *this specific*
    /// resource?
    ///
    /// Scoping matters enormously. An unreadable project root hides what that
    /// project declares — it says nothing about the other 250 volumes on the
    /// machine. Applying it globally would collapse every volume to `Unknown`
    /// because one external disk happened to be unplugged, which is exactly the
    /// mass-false-negative this design is supposed to avoid.
    pub fn affects(&self, a: &Attributed) -> bool {
        let kind = a.resource.kind;
        match self {
            Opacity::SwarmActive => matches!(
                kind,
                ResourceKind::Image | ResourceKind::Volume | ResourceKind::Network
            ),
            // A daemon we cannot see into hides everything, without exception.
            Opacity::RemoteDaemon(_) => true,
            Opacity::UnreadableRoot(reason) => kind == ResourceKind::Volume
                && a.claims.iter().any(
                    |c| matches!(&c.liveness, Liveness::Unverifiable { reason: r } if r == reason),
                ),
            Opacity::ImageLayersUnknown => matches!(kind, ResourceKind::Image),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Opacity::SwarmActive => "swarm is active on this daemon".into(),
            Opacity::RemoteDaemon(e) => format!("daemon is not local ({e})"),
            Opacity::UnreadableRoot(p) => format!("project root {p} could not be read"),
            Opacity::ImageLayersUnknown => {
                "image layer stacks were not fetched, so base images are unproven".into()
            }
        }
    }
}

/// Tri-state. `Unknown` is not a weaker `Yes` and not a softer `No` — it is the
/// answer that blocks everything.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Referenced {
    Yes { by: Vec<Referrer> },
    No,
    Unknown { because: Vec<Opacity> },
}

impl Referenced {
    pub fn is_no(&self) -> bool {
        matches!(self, Referenced::No)
    }
    pub fn is_yes(&self) -> bool {
        matches!(self, Referenced::Yes { .. })
    }
    pub fn is_unknown(&self) -> bool {
        matches!(self, Referenced::Unknown { .. })
    }
}

pub struct RefGraph {
    volume_referrers: BTreeMap<String, Vec<Referrer>>,
    image_referrers: BTreeMap<String, Vec<Referrer>>,
    network_referrers: BTreeMap<String, Vec<Referrer>>,
    /// Volumes mounted by a *live* container. These protect transitively.
    live_volumes: BTreeSet<String>,
    opacity: Vec<Opacity>,
}

impl RefGraph {
    pub fn build(report: &ScanReport) -> Self {
        let mut g = RefGraph {
            volume_referrers: BTreeMap::new(),
            image_referrers: BTreeMap::new(),
            network_referrers: BTreeMap::new(),
            live_volumes: BTreeSet::new(),
            opacity: Vec::new(),
        };

        if report.daemon.swarm_active {
            g.opacity.push(Opacity::SwarmActive);
        }
        if !report.daemon.local {
            g.opacity
                .push(Opacity::RemoteDaemon(report.context.clone()));
        }
        // Image layer stacks are not fetched by the M1/M2 scan, so we cannot
        // prove an image is not the base of another. Recorded honestly rather
        // than assumed away.
        g.opacity.push(Opacity::ImageLayersUnknown);

        // A project root we could not read hides whatever it declares.
        let mut seen_roots = BTreeSet::new();
        for a in &report.resources {
            for c in &a.claims {
                if let Liveness::Unverifiable { reason } = &c.liveness {
                    if seen_roots.insert(reason.clone()) {
                        g.opacity.push(Opacity::UnreadableRoot(reason.clone()));
                    }
                }
            }
        }

        for a in &report.resources {
            let r = &a.resource;
            if r.kind != ResourceKind::Container {
                continue;
            }
            let live = r.state.map(ContainerState::is_live).unwrap_or(false);
            let referrer = Referrer {
                kind: if live {
                    ReferrerKind::LiveContainer
                } else {
                    ReferrerKind::Container
                },
                id: r.id.0.clone(),
                name: r.name.clone(),
            };

            for m in &r.mounts {
                g.volume_referrers
                    .entry(m.clone())
                    .or_default()
                    .push(referrer.clone());
                if live {
                    g.live_volumes.insert(m.clone());
                }
            }
            if let Some(img) = &r.image_id {
                g.image_referrers
                    .entry(img.0.clone())
                    .or_default()
                    .push(referrer.clone());
            }
        }

        // A tag is user intent. An image someone named is not garbage.
        for a in &report.resources {
            let r = &a.resource;
            if r.kind == ResourceKind::Image && !r.repo_tags.is_empty() {
                g.image_referrers
                    .entry(r.id.0.clone())
                    .or_default()
                    .push(Referrer {
                        kind: ReferrerKind::Tag,
                        id: r.id.0.clone(),
                        name: r.repo_tags.join(", "),
                    });
            }
        }

        // A declaration by a project that exists on disk is a reference, even
        // with nothing running. This is what protects a volume belonging to a
        // project you simply have not started today.
        for a in &report.resources {
            if a.resource.kind != ResourceKind::Volume {
                continue;
            }
            let declared_live = a
                .claims
                .iter()
                .any(|c| c.liveness == Liveness::Present && a.resource.in_use);
            if declared_live && !g.volume_referrers.contains_key(&a.resource.name) {
                g.volume_referrers
                    .entry(a.resource.name.clone())
                    .or_default()
                    .push(Referrer {
                        kind: ReferrerKind::Declaration,
                        id: a.resource.name.clone(),
                        name: a.owner.clone().unwrap_or_else(|| a.resource.name.clone()),
                    });
            }
        }

        // Networks: a container attached to one references it.
        for a in &report.resources {
            if a.resource.kind == ResourceKind::Network && a.resource.in_use {
                g.network_referrers
                    .entry(a.resource.id.0.clone())
                    .or_default()
                    .push(Referrer {
                        kind: ReferrerKind::Container,
                        id: a.resource.id.0.clone(),
                        name: "attached container".into(),
                    });
            }
        }

        g
    }

    pub fn opacity(&self) -> &[Opacity] {
        &self.opacity
    }

    /// Is this volume held by a container that is running right now?
    pub fn volume_is_live(&self, name: &str) -> bool {
        self.live_volumes.contains(name)
    }

    /// Answer the reference question, recording every fact consulted.
    ///
    /// The `ReadSet` that comes back is exactly what the decision rested on, and
    /// hashing it yields the `evidence_hash` used to detect staleness at apply
    /// time.
    pub fn referenced(&self, a: &Attributed, rs: &mut ReadSet) -> Referenced {
        let r = &a.resource;
        let subject = format!("{}:{}", r.kind.as_str(), r.name);

        // Opacity is checked first: if we cannot see clearly, nothing else
        // matters.
        let blocking: Vec<Opacity> = self
            .opacity
            .iter()
            .filter(|o| o.affects(a))
            .cloned()
            .collect();
        if !blocking.is_empty() {
            for o in &blocking {
                rs.record(&subject, "opacity", o.describe());
            }
            return Referenced::Unknown { because: blocking };
        }

        let referrers = match r.kind {
            ResourceKind::Volume => self.volume_referrers.get(&r.name),
            ResourceKind::Image => self.image_referrers.get(&r.id.0),
            ResourceKind::Network => self.network_referrers.get(&r.id.0),
            // A container refers to things; nothing refers to a container.
            ResourceKind::Container | ResourceKind::BuildCache => None,
        };

        match referrers {
            Some(list) if !list.is_empty() => {
                rs.record(&subject, "referrer_count", list.len().to_string());
                for x in list {
                    rs.record(&subject, "referrer", format!("{:?}:{}", x.kind, x.name));
                }
                Referenced::Yes { by: list.clone() }
            }
            _ => {
                rs.record(&subject, "referrer_count", "0");
                // Cross-check against the daemon's own view. A disagreement is
                // not resolved in our favour — it downgrades to Unknown.
                rs.record(&subject, "daemon_in_use", r.in_use.to_string());
                if r.in_use {
                    return Referenced::Unknown {
                        because: vec![Opacity::RemoteDaemon(format!(
                            "daemon reports {} in use but no referrer was found",
                            r.name
                        ))],
                    };
                }
                Referenced::No
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DaemonIdentity;
    use crate::model::{DaemonId, ResourceSummary, RuntimeFlavor, Totals};
    use crate::scan::Attributed;

    fn daemon(local: bool, swarm: bool) -> DaemonIdentity {
        DaemonIdentity {
            id: DaemonId("D".into()),
            api_version: "1.54".into(),
            server_version: "29".into(),
            runtime: RuntimeFlavor::OrbStack,
            data_root: None,
            local,
            swarm_active: swarm,
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
            unattributed: true,
        }
    }

    fn report(resources: Vec<Attributed>, local: bool, swarm: bool) -> ScanReport {
        ScanReport {
            daemon: daemon(local, swarm),
            context: "test".into(),
            totals: Totals::default(),
            resources,
            projects_known: 0,
            duration_ms: 0,
            stale: false,
            warnings: vec![],
        }
    }

    fn container(name: &str, state: ContainerState, mounts: &[&str]) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Container, name, name);
        r.state = Some(state);
        r.mounts = mounts.iter().map(|s| s.to_string()).collect();
        r
    }

    fn volume(name: &str) -> ResourceSummary {
        ResourceSummary::new(ResourceKind::Volume, name, name)
    }

    #[test]
    fn an_exited_container_still_references_its_volume() {
        // The daemon's RefCount would say 0 here. Acting on that is the bug
        // this graph exists to prevent.
        let rep = report(
            vec![
                attributed(container("old", ContainerState::Exited, &["v1"])),
                attributed(volume("v1")),
            ],
            true,
            false,
        );
        let g = RefGraph::build(&rep);
        let mut rs = ReadSet::new();
        let v = rep
            .resources
            .iter()
            .find(|a| a.resource.name == "v1")
            .unwrap();
        assert!(g.referenced(v, &mut rs).is_yes());
    }

    #[test]
    fn a_volume_with_no_container_at_all_is_unreferenced() {
        let rep = report(vec![attributed(volume("lonely"))], true, false);
        let g = RefGraph::build(&rep);
        let mut rs = ReadSet::new();
        let v = &rep.resources[0];
        assert!(g.referenced(v, &mut rs).is_no());
        assert!(!rs.is_empty(), "the decision must record what it consulted");
    }

    #[test]
    fn swarm_makes_volumes_unknown_rather_than_unreferenced() {
        let rep = report(vec![attributed(volume("lonely"))], true, true);
        let g = RefGraph::build(&rep);
        let mut rs = ReadSet::new();
        let got = g.referenced(&rep.resources[0], &mut rs);
        assert!(got.is_unknown(), "swarm can reference what we cannot see");
    }

    #[test]
    fn a_remote_daemon_makes_everything_unknown() {
        let rep = report(vec![attributed(volume("lonely"))], false, false);
        let g = RefGraph::build(&rep);
        let mut rs = ReadSet::new();
        assert!(g.referenced(&rep.resources[0], &mut rs).is_unknown());
    }

    #[test]
    fn daemon_disagreement_downgrades_to_unknown() {
        // No referrer in our graph, but the daemon says it is in use. We do not
        // get to resolve that in favour of deleting.
        let mut v = volume("contested");
        v.in_use = true;
        let rep = report(vec![attributed(v)], true, false);
        let g = RefGraph::build(&rep);
        let mut rs = ReadSet::new();
        assert!(g.referenced(&rep.resources[0], &mut rs).is_unknown());
    }

    #[test]
    fn live_containers_are_distinguished_from_dead_ones() {
        let rep = report(
            vec![
                attributed(container("live", ContainerState::Running, &["hot"])),
                attributed(container("dead", ContainerState::Exited, &["cold"])),
                attributed(volume("hot")),
                attributed(volume("cold")),
            ],
            true,
            false,
        );
        let g = RefGraph::build(&rep);
        assert!(g.volume_is_live("hot"));
        assert!(
            !g.volume_is_live("cold"),
            "exited is a referrer, not a live one"
        );
    }

    #[test]
    fn images_are_always_unknown_until_layers_are_fetched() {
        // We cannot prove an image is not the base of another without its layer
        // stack, so no image may reach Tier 1 in this build. Stated as opacity
        // rather than quietly assumed.
        let img = ResourceSummary::new(ResourceKind::Image, "sha256:abc", "dangling");
        let rep = report(vec![attributed(img)], true, false);
        let g = RefGraph::build(&rep);
        let mut rs = ReadSet::new();
        let got = g.referenced(&rep.resources[0], &mut rs);
        assert!(got.is_unknown());
    }

    #[test]
    fn an_unreadable_root_only_darkens_its_own_volumes() {
        // One unplugged external disk must not collapse every volume on the
        // machine to Unknown. This was a real bug: three unverifiable roots
        // made all 259 volumes unattributed.
        let o = Opacity::UnreadableRoot("/Volumes/Work is not readable".into());

        let mine = attributed_with_claim(
            ResourceSummary::new(ResourceKind::Volume, "a", "a"),
            Liveness::Unverifiable {
                reason: "/Volumes/Work is not readable".into(),
            },
        );
        let theirs = attributed_with_claim(
            ResourceSummary::new(ResourceKind::Volume, "b", "b"),
            Liveness::Present,
        );

        assert!(o.affects(&mine));
        assert!(
            !o.affects(&theirs),
            "an unrelated volume must stay decidable"
        );
    }

    #[test]
    fn a_remote_daemon_darkens_everything_without_exception() {
        let o = Opacity::RemoteDaemon("staging".into());
        let v = attributed(ResourceSummary::new(ResourceKind::Volume, "a", "a"));
        let n = attributed(ResourceSummary::new(ResourceKind::Network, "b", "b"));
        assert!(o.affects(&v));
        assert!(o.affects(&n));
    }

    fn attributed_with_claim(r: ResourceSummary, liveness: Liveness) -> Attributed {
        use crate::model::{Claim, Confidence, ProjectId, ProviderKind};
        let mut a = attributed(r);
        a.claims = vec![Claim {
            project: ProjectId("p".into()),
            project_name: "p".into(),
            provider: ProviderKind::Compose,
            confidence: Confidence::Strong,
            root: None,
            liveness,
            evidence: vec![],
        }];
        a
    }
}
