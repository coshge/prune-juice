//! The domain vocabulary.
//!
//! These are *our* types. Nothing from `bollard` appears here or anywhere
//! outside `crate::docker` — that seam is what lets the client be swapped and
//! what lets the replay client exist.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- bytes ----

/// A byte count. Newtype so it cannot be confused with any other u64, and so
/// decimal-vs-binary formatting happens in exactly one place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bytes(pub u64);

impl Bytes {
    pub const ZERO: Bytes = Bytes(0);

    pub fn get(self) -> u64 {
        self.0
    }

    /// Human formatting in **decimal** units, matching what the Docker API
    /// reports. Mixing these with `du`'s binary MiB/GiB is a ~5% phantom error,
    /// so both sides of any comparison must come through here.
    pub fn human(self) -> String {
        const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
        let mut v = self.0 as f64;
        let mut u = 0;
        while v >= 1000.0 && u < UNITS.len() - 1 {
            v /= 1000.0;
            u += 1;
        }
        if u == 0 {
            format!("{} {}", self.0, UNITS[0])
        } else if v >= 100.0 {
            format!("{v:.0} {}", UNITS[u])
        } else {
            format!("{v:.1} {}", UNITS[u])
        }
    }
}

impl std::ops::Add for Bytes {
    type Output = Bytes;
    fn add(self, rhs: Bytes) -> Bytes {
        Bytes(self.0.saturating_add(rhs.0))
    }
}

impl std::iter::Sum for Bytes {
    fn sum<I: Iterator<Item = Bytes>>(iter: I) -> Bytes {
        iter.fold(Bytes::ZERO, |a, b| a + b)
    }
}

// ------------------------------------------------------------------ ids ----

macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl From<String> for $name { fn from(s: String) -> Self { Self(s) } }
        impl From<&str> for $name { fn from(s: &str) -> Self { Self(s.to_string()) } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id! {
    /// The daemon's own `/info` `ID`. **Everything is keyed by this, never by
    /// context name** — two contexts can point at the same engine (a symlinked
    /// socket, a TCP alias), and keying by name double-counts.
    DaemonId
}
string_id! {
    /// Stable identity of a Docker resource within a daemon: a container or
    /// image ID, or a volume/network name.
    ResourceId
}
string_id! {
    /// A project, identified by its canonical absolute path where one is known,
    /// otherwise by provider-scoped name.
    ProjectId
}

// ------------------------------------------------------------- resources ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Container,
    Image,
    Volume,
    Network,
    BuildCache,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Container => "container",
            ResourceKind::Image => "image",
            ResourceKind::Volume => "volume",
            ResourceKind::Network => "network",
            ResourceKind::BuildCache => "build_cache",
        }
    }
}

/// Where a size figure came from. Surfaced so the UI can distinguish a cached
/// figure from a freshly measured one, and Docker-reported from host-measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeSource {
    Cache,
    DaemonDf,
    LocalDu,
}

/// One resource as the scan sees it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceSummary {
    pub kind: ResourceKind,
    pub id: ResourceId,
    pub name: String,
    /// RFC3339 as the daemon reports it, where available.
    pub created_at: Option<String>,
    /// Unix seconds, parsed from `created_at` where possible.
    pub created_unix: Option<i64>,
    pub labels: BTreeMap<String, String>,
    pub size: Option<Bytes>,
    /// True when a *running* container references this. Stopped containers do
    /// not count here, but they still count as referrers in the graph.
    pub in_use: bool,
    /// Container state, for containers only.
    pub state: Option<ContainerState>,
    /// Volume names this container mounts. Retained even for exited containers:
    /// this is the only surviving link from an anonymous volume to a project,
    /// and `docker container prune` destroys it permanently.
    pub mounts: Vec<String>,
    /// Image ID this container runs, for containers only.
    pub image_id: Option<ResourceId>,
    /// Writable-layer size, for containers. Non-zero means data lives outside
    /// any volume and would be lost.
    pub size_rw: Option<Bytes>,
    /// Content-addressed layer stack, for images. A proper prefix relationship
    /// identifies a base image — legacy `Parent` is empty under BuildKit.
    pub layers: Vec<String>,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
}

impl ResourceSummary {
    pub fn new(kind: ResourceKind, id: impl Into<ResourceId>, name: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
            name: name.into(),
            created_at: None,
            created_unix: None,
            labels: BTreeMap::new(),
            size: None,
            in_use: false,
            state: None,
            mounts: Vec::new(),
            image_id: None,
            size_rw: None,
            layers: Vec::new(),
            repo_tags: Vec::new(),
            repo_digests: Vec::new(),
        }
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(|s| s.as_str())
    }

    /// A label that is present but empty carries no information. The supabase
    /// CLI stamps `com.docker.compose.project.working_dir` with an empty string,
    /// so a "key exists" check is not sufficient anywhere in this codebase.
    pub fn label_nonempty(&self, key: &str) -> Option<&str> {
        self.label(key).map(str::trim).filter(|s| !s.is_empty())
    }

    /// True for a 64-hex volume name — Docker's anonymous volume shape.
    pub fn is_anonymous_volume(&self) -> bool {
        self.kind == ResourceKind::Volume
            && self.name.len() == 64
            && self.name.bytes().all(|b| b.is_ascii_hexdigit())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerState {
    Created,
    Running,
    Paused,
    Restarting,
    Exited,
    Dead,
    Removing,
    Unknown,
}

impl ContainerState {
    pub fn parse(s: &str) -> Self {
        match s {
            "created" => Self::Created,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "restarting" => Self::Restarting,
            "exited" => Self::Exited,
            "dead" => Self::Dead,
            "removing" => Self::Removing,
            _ => Self::Unknown,
        }
    }

    /// Live states hold hard references that protect everything downstream.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Paused | Self::Restarting)
    }
}

// -------------------------------------------------------------- projects ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Compose,
    Ddev,
    Devcontainer,
    Supabase,
    Heuristic,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Ddev => "ddev",
            Self::Devcontainer => "devcontainer",
            Self::Supabase => "supabase",
            Self::Heuristic => "heuristic",
        }
    }

    /// Tie-break when two providers claim the same resource at equal confidence.
    pub fn priority(self) -> u8 {
        match self {
            Self::Compose => 4,
            Self::Ddev => 3,
            Self::Devcontainer => 2,
            Self::Supabase => 1,
            Self::Heuristic => 0,
        }
    }
}

/// How sure we are that a resource belongs to a project.
///
/// This gates deletion: `Guess` may only raise a question, never justify
/// removing anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Pure name heuristic. The heuristic provider is capped here.
    Guess = 0,
    /// No label; a historical index edge, or a naming convention matching a
    /// known project of the right kind.
    Weak = 1,
    /// An authoritative label whose path exists, with no declaration to
    /// cross-check against.
    Strong = 2,
    /// An authoritative label naming an existing path, *and* that project's
    /// config declares this resource.
    Proven = 3,
}

/// Whether the project this resource claims to belong to is actually there.
///
/// `Unverifiable` is distinct from `Absent` and never licenses a deletion — an
/// unmounted external disk must not turn every project under it into an orphan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum Liveness {
    Present,
    Absent { since_unix: Option<i64>, scans: u32 },
    Unverifiable { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: ProjectId,
    pub name: String,
    pub provider: ProviderKind,
    pub root: Option<PathBuf>,
    pub liveness: Liveness,
}

/// A provider's assertion about who owns a resource, with the evidence that
/// justifies it. Every piece of evidence must render as one citable line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claim {
    pub project: ProjectId,
    pub project_name: String,
    pub provider: ProviderKind,
    pub confidence: Confidence,
    pub root: Option<PathBuf>,
    pub liveness: Liveness,
    pub evidence: Vec<Evidence>,
}

/// One human-readable fact with its source. If a signal cannot be rendered as
/// one of these, it does not get to influence a decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub source: EvidenceSource,
    pub detail: String,
}

impl Evidence {
    pub fn new(source: EvidenceSource, detail: impl Into<String>) -> Self {
        Self {
            source,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Label,
    Fs,
    Index,
    Probe,
    Daemon,
    Heuristic,
}

impl EvidenceSource {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Fs => "fs",
            Self::Index => "index",
            Self::Probe => "probe",
            Self::Daemon => "daemon",
            Self::Heuristic => "heuristic",
        }
    }
}

// --------------------------------------------------------------- runtime ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFlavor {
    DockerDesktopMac,
    DockerDesktopWindows,
    OrbStack,
    Colima,
    Podman,
    RancherDesktop,
    NativeLinux,
    Remote,
    Unknown,
}

// --------------------------------------------------------------- totals ----

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Totals {
    pub containers: u32,
    pub images: u32,
    pub volumes: u32,
    pub networks: u32,
    pub build_cache_records: u32,
    pub volume_bytes: Bytes,
    pub image_bytes: Bytes,
    pub build_cache_bytes: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_format_in_decimal_units() {
        assert_eq!(Bytes(999).human(), "999 B");
        assert_eq!(Bytes(1_000).human(), "1.0 kB");
        assert_eq!(Bytes(516_100_000).human(), "516 MB");
        assert_eq!(Bytes(20_520_000_000).human(), "20.5 GB");
    }

    #[test]
    fn empty_label_is_not_a_value() {
        // The supabase CLI stamps working_dir with "". Treating a present-but-
        // empty label as authoritative would attribute every supabase resource
        // to a project rooted at "".
        let mut r = ResourceSummary::new(ResourceKind::Container, "abc", "supabase_db_x");
        r.labels.insert(
            "com.docker.compose.project.working_dir".into(),
            String::new(),
        );
        assert!(r.label("com.docker.compose.project.working_dir").is_some());
        assert!(r
            .label_nonempty("com.docker.compose.project.working_dir")
            .is_none());
    }

    #[test]
    fn anonymous_volumes_are_64_hex() {
        let anon = ResourceSummary::new(
            ResourceKind::Volume,
            "x",
            "0fa0e3879a9eb80e16c2db964bf696099d67f0de7c4962ab4ce51769f7f7d155",
        );
        assert!(anon.is_anonymous_volume());

        let named = ResourceSummary::new(ResourceKind::Volume, "y", "oak_mysql");
        assert!(!named.is_anonymous_volume());

        // 64 chars but not hex.
        let notquite = ResourceSummary::new(ResourceKind::Volume, "z", "g".repeat(64));
        assert!(!notquite.is_anonymous_volume());
    }

    #[test]
    fn confidence_orders_correctly() {
        assert!(Confidence::Proven > Confidence::Strong);
        assert!(Confidence::Strong > Confidence::Weak);
        assert!(Confidence::Weak > Confidence::Guess);
    }

    #[test]
    fn only_live_states_hold_hard_references() {
        assert!(ContainerState::Running.is_live());
        assert!(ContainerState::Paused.is_live());
        assert!(ContainerState::Restarting.is_live());
        assert!(!ContainerState::Exited.is_live());
        assert!(!ContainerState::Created.is_live());
    }
}
