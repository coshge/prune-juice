//! Host disk measurement.
//!
//! Docker's numbers are *logical*. What a user cares about is whether the host
//! got its space back, and on every VM-backed runtime those are different
//! questions. Conflating them is the mistake most cleanup tools make: they
//! report 20 GB freed, Finder shows no change, and the user concludes the tool
//! is broken.
//!
//! Two traps, both verified on the reference machine:
//!
//! * **A sparse disk image lies about its size.** OrbStack's `data.img.raw`
//!   reports 494.4 GB apparent and 109.2 GB actually allocated — a 4.5× error
//!   if you read `st_size` instead of `st_blocks`.
//! * **Deleting inside the guest does not shrink the host file** until the
//!   guest issues discard and the runtime punches holes. Some runtimes do that
//!   automatically, some need prompting, and some cannot do it at all.
//!
//! So per-resource figures are labelled *Docker-reported* and the headline
//! reclamation is *host-measured*, and the two are never added together.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Bytes, RuntimeFlavor};

/// Where a daemon's bytes physically live.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BackingStore {
    /// A real host directory. Deleting reclaims immediately.
    NativeFilesystem { data_root: PathBuf },
    /// A sparse image file holding a guest filesystem.
    SparseDiskImage {
        path: PathBuf,
        /// True when the runtime is known to punch holes without being asked.
        auto_compacts: bool,
    },
    /// A WSL2 virtual disk. Only measurable from the Windows side.
    Wsl2Vhdx { note: String },
    /// A daemon we are not on the same machine as.
    Remote { endpoint: String },
    /// Nothing we recognise. Reported as unknown rather than guessed at.
    Unknown { reason: String },
}

/// A measurement of host disk, at a point in time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostUsage {
    /// `st_size` — what a naive listing shows. Meaningless for a sparse file,
    /// carried only so the gap can be explained to a confused user.
    pub apparent: Option<Bytes>,
    /// `st_blocks * 512` — bytes actually committed. This is the real number.
    pub physical: Option<Bytes>,
    /// Free space on the filesystem holding the store.
    pub fs_free: Option<Bytes>,
    pub measured_at: i64,
}

/// How much of a reclamation actually reached the host, and how sure we are.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reclamation {
    pub docker_reported: Bytes,
    /// Change in physically-allocated bytes. `None` when the store cannot be
    /// measured — which is said plainly rather than filled in with the
    /// Docker figure.
    pub host_measured: Option<Bytes>,
    pub confidence: Confidence,
    pub compaction: CompactionCapability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A native filesystem, or a sparse image whose delta looks clean.
    High,
    /// The two views disagree — usually something else was writing at the time.
    Medium,
    /// We could not measure the host at all.
    Unknown,
}

/// Whether the host file can be made to give its space back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum CompactionCapability {
    /// A native filesystem. Space returns as soon as the file is unlinked.
    NotNeeded,
    /// The runtime punches holes on its own; the number may just lag.
    Automatic(String),
    /// A command exists, but it is slow and sometimes needs Docker stopped, so
    /// it is never run without asking.
    Triggerable {
        how: String,
        warning: String,
    },
    /// Only the user can do it, from the runtime's own UI.
    ManualOnly {
        instructions: Vec<String>,
    },
    Unavailable(String),
}

/// Detect the backing store for a runtime.
///
/// Selection happens at **runtime**, not by `#[cfg]`: one macOS machine can
/// have both Docker Desktop and OrbStack installed, and the answer depends on
/// which daemon the context points at.
pub fn detect(runtime: RuntimeFlavor, data_root: Option<&str>, endpoint: &str) -> BackingStore {
    if endpoint.starts_with("tcp://") || endpoint.starts_with("ssh://") {
        return BackingStore::Remote {
            endpoint: endpoint.to_string(),
        };
    }

    // A Linux binary talking to Docker Desktop through WSL2 is looking at the
    // wrong filesystem entirely — the vhdx lives on the Windows side. Reporting
    // the WSL filesystem here would be confidently wrong.
    if is_wsl() {
        return BackingStore::Wsl2Vhdx {
            note:
                "the virtual disk is on the Windows side; run prune-juice.exe there to measure it"
                    .to_string(),
        };
    }

    let home = std::env::var("HOME").ok().map(PathBuf::from);

    match runtime {
        RuntimeFlavor::NativeLinux => match data_root {
            Some(dr) if Path::new(dr).is_dir() => BackingStore::NativeFilesystem {
                data_root: PathBuf::from(dr),
            },
            _ => BackingStore::Unknown {
                reason: "the daemon's data root is not readable from here".into(),
            },
        },

        RuntimeFlavor::OrbStack => {
            // OrbStack keeps its image in a group container, not under
            // ~/.orbstack — which holds only ~800 KB of config. Measuring the
            // latter would be wrong by five orders of magnitude.
            if let Some(h) = &home {
                let group = h.join("Library/Group Containers");
                if let Some(p) = find_first(&group, &["data/data.img.raw", "data/data.img"]) {
                    return BackingStore::SparseDiskImage {
                        path: p,
                        auto_compacts: true,
                    };
                }
            }
            BackingStore::Unknown {
                reason: "OrbStack's disk image was not found".into(),
            }
        }

        RuntimeFlavor::DockerDesktopMac => {
            if let Some(h) = &home {
                let base = h.join("Library/Containers/com.docker.docker/Data");
                for rel in [
                    "vms/0/data/Docker.raw",
                    "vms/0/Docker.raw",
                    "vms/0/data/Docker.qcow2",
                ] {
                    let p = base.join(rel);
                    if p.is_file() {
                        return BackingStore::SparseDiskImage {
                            path: p,
                            // Modern Docker Desktop on virtualization.framework
                            // usually reclaims via guest TRIM, but not always,
                            // and the escape hatch is worth surfacing.
                            auto_compacts: false,
                        };
                    }
                }
            }
            BackingStore::Unknown {
                reason: "Docker Desktop's disk image was not found".into(),
            }
        }

        RuntimeFlavor::Colima => {
            if let Some(h) = &home {
                if let Some(p) = find_first(&h.join(".colima/_lima"), &["diffdisk"]) {
                    return BackingStore::SparseDiskImage {
                        path: p,
                        auto_compacts: false,
                    };
                }
            }
            BackingStore::Unknown {
                reason: "Colima's disk image was not found".into(),
            }
        }

        RuntimeFlavor::Podman => {
            if let Some(h) = &home {
                let base = h.join(".local/share/containers/podman/machine");
                if let Some(p) = find_by_extension(&base, "raw") {
                    return BackingStore::SparseDiskImage {
                        path: p,
                        auto_compacts: false,
                    };
                }
            }
            BackingStore::Unknown {
                reason: "Podman machine's disk image was not found".into(),
            }
        }

        RuntimeFlavor::DockerDesktopWindows => BackingStore::Wsl2Vhdx {
            note: "measure and compact the vhdx from Windows".to_string(),
        },

        RuntimeFlavor::RancherDesktop | RuntimeFlavor::Unknown | RuntimeFlavor::Remote => {
            BackingStore::Unknown {
                reason: format!("no known disk layout for {runtime:?}"),
            }
        }
    }
}

impl BackingStore {
    /// Measure it, now.
    pub fn measure(&self, now_unix: i64) -> HostUsage {
        match self {
            BackingStore::NativeFilesystem { data_root } => HostUsage {
                apparent: None,
                physical: None,
                fs_free: free_space(data_root).map(Bytes),
                measured_at: now_unix,
            },
            BackingStore::SparseDiskImage { path, .. } => {
                let (apparent, physical) = file_sizes(path);
                HostUsage {
                    apparent,
                    physical,
                    fs_free: free_space(path).map(Bytes),
                    measured_at: now_unix,
                }
            }
            _ => HostUsage {
                apparent: None,
                physical: None,
                fs_free: None,
                measured_at: now_unix,
            },
        }
    }

    pub fn compaction(&self) -> CompactionCapability {
        match self {
            BackingStore::NativeFilesystem { .. } => CompactionCapability::NotNeeded,
            BackingStore::SparseDiskImage {
                auto_compacts: true,
                ..
            } => CompactionCapability::Automatic(
                "this runtime punches holes on its own; the figure may lag by a moment".into(),
            ),
            BackingStore::SparseDiskImage {
                auto_compacts: false,
                ..
            } => CompactionCapability::Triggerable {
                how: "docker run --privileged --pid=host docker/desktop-reclaim-space".into(),
                warning: "takes minutes, pulls a privileged image, and is never run for you".into(),
            },
            BackingStore::Wsl2Vhdx { .. } => CompactionCapability::ManualOnly {
                instructions: vec![
                    "wsl --shutdown".into(),
                    "Optimize-VHD -Path <vhdx> -Mode Full   (Hyper-V module)".into(),
                    "or: diskpart → select vdisk file=<vhdx> → compact vdisk".into(),
                ],
            },
            BackingStore::Remote { .. } => {
                CompactionCapability::Unavailable("the daemon is not on this machine".into())
            }
            BackingStore::Unknown { reason } => CompactionCapability::Unavailable(reason.clone()),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            BackingStore::NativeFilesystem { data_root } => {
                format!("native filesystem at {}", data_root.display())
            }
            BackingStore::SparseDiskImage {
                path,
                auto_compacts,
            } => format!(
                "sparse disk image at {} ({})",
                path.display(),
                if *auto_compacts {
                    "reclaims automatically"
                } else {
                    "may need compacting"
                }
            ),
            BackingStore::Wsl2Vhdx { note } => format!("WSL2 virtual disk — {note}"),
            BackingStore::Remote { endpoint } => format!("remote daemon at {endpoint}"),
            BackingStore::Unknown { reason } => format!("unknown — {reason}"),
        }
    }
}

/// Compare a before and after measurement against what Docker claimed.
///
/// Reports a shortfall rather than hiding it: under-delivering silently is how
/// a tool loses a user's trust for good.
pub fn reconcile(
    before: &HostUsage,
    after: &HostUsage,
    docker_reported: Bytes,
    store: &BackingStore,
) -> Reclamation {
    let host_measured = match (before.physical, after.physical) {
        (Some(b), Some(a)) => Some(Bytes(b.get().saturating_sub(a.get()))),
        _ => match (before.fs_free, after.fs_free) {
            // On a native filesystem there is no image file, so the change in
            // free space is the measurement.
            (Some(b), Some(a)) => Some(Bytes(a.get().saturating_sub(b.get()))),
            _ => None,
        },
    };

    let confidence = match (host_measured, store) {
        (None, _) => Confidence::Unknown,
        (Some(_), BackingStore::NativeFilesystem { .. }) => Confidence::High,
        (Some(m), _) => {
            // A sparse image that gave back roughly what Docker claimed is a
            // clean result. A wild divergence means something else was writing,
            // or the image has not been compacted yet.
            let claimed = docker_reported.get();
            if claimed == 0 {
                Confidence::High
            } else {
                let ratio = m.get() as f64 / claimed as f64;
                if (0.5..=1.5).contains(&ratio) {
                    Confidence::High
                } else {
                    Confidence::Medium
                }
            }
        }
    };

    Reclamation {
        docker_reported,
        host_measured,
        confidence,
        compaction: store.compaction(),
    }
}

impl Reclamation {
    /// True when the host clearly gave back much less than Docker claimed, so
    /// the user should be told about compaction rather than left puzzled.
    pub fn shortfall_worth_mentioning(&self) -> bool {
        match self.host_measured {
            Some(m) => {
                self.docker_reported.get() > 0
                    && m.get() * 2 < self.docker_reported.get()
                    && !matches!(self.compaction, CompactionCapability::NotNeeded)
            }
            None => false,
        }
    }
}

/// `(apparent, physical)` for a file.
///
/// `st_blocks` is the number that matters. Reading `st_size` on OrbStack's
/// image reports 494 GB where 109 GB is committed.
fn file_sizes(path: &Path) -> (Option<Bytes>, Option<Bytes>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(md) => (
                Some(Bytes(md.len())),
                Some(Bytes(md.blocks().saturating_mul(512))),
            ),
            Err(_) => (None, None),
        }
    }
    #[cfg(not(unix))]
    {
        match std::fs::metadata(path) {
            Ok(md) => (Some(Bytes(md.len())), None),
            Err(_) => (None, None),
        }
    }
}

/// Free bytes on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let avail_kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb.saturating_mul(1024))
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

/// First existing `dir/*/rel` for any `rel`.
fn find_first(dir: &Path, rels: &[&str]) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        for rel in rels {
            let p = e.path().join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// First file with the given extension, one level down.
fn find_by_extension(dir: &Path, ext: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        if let Ok(inner) = std::fs::read_dir(e.path()) {
            for f in inner.flatten() {
                let p = f.path();
                if p.extension().and_then(|s| s.to_str()) == Some(ext) {
                    return Some(p);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_endpoint_is_never_measured_locally() {
        let s = detect(RuntimeFlavor::OrbStack, None, "tcp://10.0.0.5:2375");
        assert!(matches!(s, BackingStore::Remote { .. }));
        assert!(matches!(
            s.compaction(),
            CompactionCapability::Unavailable(_)
        ));
        let u = s.measure(0);
        assert!(u.physical.is_none(), "a remote host cannot be measured");
    }

    #[test]
    fn a_native_filesystem_needs_no_compaction() {
        let d = std::env::temp_dir();
        let s = detect(
            RuntimeFlavor::NativeLinux,
            Some(d.to_str().unwrap()),
            "unix:///x.sock",
        );
        if let BackingStore::NativeFilesystem { .. } = s {
            assert_eq!(s.compaction(), CompactionCapability::NotNeeded);
            assert!(s.measure(0).fs_free.is_some());
        } else {
            // On a non-Linux host `detect` correctly declines; that is fine.
            assert!(matches!(
                s,
                BackingStore::Unknown { .. } | BackingStore::Wsl2Vhdx { .. }
            ));
        }
    }

    #[test]
    fn an_unfound_image_is_unknown_rather_than_guessed() {
        // With HOME pointed somewhere empty, detection must decline.
        let s = detect(RuntimeFlavor::DockerDesktopMac, None, "unix:///x.sock");
        match s {
            BackingStore::SparseDiskImage { path, .. } => {
                assert!(path.is_file(), "only claim a path that exists");
            }
            other => assert!(
                matches!(
                    other,
                    BackingStore::Unknown { .. } | BackingStore::Wsl2Vhdx { .. }
                ),
                "{other:?}"
            ),
        }
    }

    #[test]
    fn physical_and_apparent_are_measured_separately() {
        // The 4.5x trap: a sparse file's apparent size is not its footprint.
        let p = std::env::temp_dir().join(format!(
            "pj-sparse-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, b"small").unwrap();
        let (apparent, physical) = file_sizes(&p);
        assert_eq!(apparent, Some(Bytes(5)));
        assert!(physical.is_some());
        std::fs::remove_file(&p).ok();
    }

    fn usage(physical: Option<u64>, free: Option<u64>) -> HostUsage {
        HostUsage {
            apparent: None,
            physical: physical.map(Bytes),
            fs_free: free.map(Bytes),
            measured_at: 0,
        }
    }

    fn image_store() -> BackingStore {
        BackingStore::SparseDiskImage {
            path: PathBuf::from("/tmp/x.raw"),
            auto_compacts: false,
        }
    }

    #[test]
    fn a_clean_sparse_delta_reads_as_high_confidence() {
        let r = reconcile(
            &usage(Some(100_000_000_000), None),
            &usage(Some(90_000_000_000), None),
            Bytes(10_000_000_000),
            &image_store(),
        );
        assert_eq!(r.host_measured, Some(Bytes(10_000_000_000)));
        assert_eq!(r.confidence, Confidence::High);
        assert!(!r.shortfall_worth_mentioning());
    }

    #[test]
    fn a_host_that_gave_back_nothing_is_reported_not_hidden() {
        // Docker says 10 GB, the image did not shrink. The user must be told,
        // or they will conclude the tool lied.
        let r = reconcile(
            &usage(Some(100_000_000_000), None),
            &usage(Some(100_000_000_000), None),
            Bytes(10_000_000_000),
            &image_store(),
        );
        assert_eq!(r.host_measured, Some(Bytes::ZERO));
        assert_eq!(r.confidence, Confidence::Medium);
        assert!(
            r.shortfall_worth_mentioning(),
            "a total shortfall must prompt the compaction hint"
        );
    }

    #[test]
    fn an_unmeasurable_host_yields_none_not_the_docker_figure() {
        // Filling the gap with Docker's number would be the exact conflation
        // this module exists to avoid.
        let r = reconcile(
            &usage(None, None),
            &usage(None, None),
            Bytes(10_000_000_000),
            &BackingStore::Unknown {
                reason: "no idea".into(),
            },
        );
        assert!(r.host_measured.is_none());
        assert_eq!(r.confidence, Confidence::Unknown);
        assert!(!r.shortfall_worth_mentioning());
    }

    #[test]
    fn a_native_filesystem_is_measured_by_free_space_delta() {
        let store = BackingStore::NativeFilesystem {
            data_root: PathBuf::from("/tmp"),
        };
        let r = reconcile(
            &usage(None, Some(50_000_000_000)),
            &usage(None, Some(58_000_000_000)),
            Bytes(8_000_000_000),
            &store,
        );
        assert_eq!(r.host_measured, Some(Bytes(8_000_000_000)));
        assert_eq!(r.confidence, Confidence::High);
    }

    #[test]
    fn an_automatic_runtime_does_not_nag_about_compaction() {
        let store = BackingStore::SparseDiskImage {
            path: PathBuf::from("/tmp/x.raw"),
            auto_compacts: true,
        };
        assert!(matches!(
            store.compaction(),
            CompactionCapability::Automatic(_)
        ));
    }

    #[test]
    fn wsl_is_told_to_measure_from_windows() {
        let s = BackingStore::Wsl2Vhdx {
            note: "n".to_string(),
        };
        assert!(matches!(
            s.compaction(),
            CompactionCapability::ManualOnly { .. }
        ));
        assert!(s.measure(0).physical.is_none());
    }
}
