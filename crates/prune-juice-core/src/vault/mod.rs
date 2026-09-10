//! The vault: a verified copy taken before anything irreversible happens.
//!
//! There is **no trash can for volumes**, and pretending otherwise would be the
//! most dishonest thing this tool could do. Docker has no volume rename; on
//! macOS and Windows the bytes live inside a VM the host cannot touch; and
//! moving `_data` aside on Linux corrupts daemon state. A verified logical copy
//! is the only truthful form of deferred deletion.
//!
//! Two rules govern everything here:
//!
//! * **Verify before delete, always.** Write to a temporary name, fsync,
//!   re-read and confirm the digest and the entry count, rename atomically, and
//!   only then let the caller delete. A mismatch refuses the deletion — the
//!   volume is kept and the run reports a problem.
//! * **Never start a writable container against a volume being preserved.**
//!   Booting Postgres against a real data directory triggers crash recovery and
//!   catalog writes, mutating the thing being protected. The dump reads through
//!   a container that is created and never started.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::disk::free_space;
use crate::docker::{DockerMutate, DockerProbe};
use crate::error::{Error, Result};
use crate::model::Bytes;
use crate::probe::Engine;

/// What was preserved, and everything needed to put it back.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VaultEntry {
    /// Filesystem-safe identifier: `<volume>-<unix>`.
    pub id: String,
    pub volume: String,
    /// Labels the volume carried, so a restore recreates it faithfully.
    pub labels: BTreeMap<String, String>,
    /// Engine detected at dump time, where one was.
    pub engine: Option<Engine>,
    /// Size of the compressed archive on disk.
    pub archive_bytes: Bytes,
    /// Digest of the compressed archive.
    pub sha256: String,
    /// Tar entries counted while writing. Re-counted on verify.
    pub entry_count: u64,
    pub created_unix: i64,
    /// When the archive was last confirmed intact. `None` means never — and an
    /// unverified backup does not license a deletion.
    pub verified_unix: Option<i64>,
}

impl VaultEntry {
    pub fn archive_name(&self) -> String {
        format!("{}.tar.gz", self.id)
    }
    pub fn manifest_name(&self) -> String {
        format!("{}.json", self.id)
    }
}

/// Where preserved copies live.
///
/// On the **host** filesystem, deliberately: a vault inside a Docker volume
/// could be eaten by a later `docker system prune`, which would make the safety
/// net part of the hazard.
pub struct Vault {
    root: PathBuf,
}

impl Vault {
    /// The platform's data directory. Not a cache directory — a cache is
    /// something the system may delete, and these are backups.
    pub fn open() -> Result<Self> {
        let base = if let Ok(x) = std::env::var("XDG_DATA_HOME") {
            PathBuf::from(x)
        } else if let Ok(home) = std::env::var("HOME") {
            if cfg!(target_os = "macos") {
                PathBuf::from(home).join("Library/Application Support")
            } else {
                PathBuf::from(home).join(".local/share")
            }
        } else {
            return Err(Error::Config("cannot locate a data directory".into()));
        };
        Ok(Self::at(base.join("prune-juice/vault")))
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(Error::Io)
    }

    /// Every entry currently held, newest first.
    pub fn entries(&self) -> Result<Vec<VaultEntry>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Ok(out);
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Ok(entry) = serde_json::from_str::<VaultEntry>(&text) {
                    out.push(entry);
                }
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.created_unix));
        Ok(out)
    }

    pub fn total_bytes(&self) -> Bytes {
        self.entries()
            .unwrap_or_default()
            .iter()
            .map(|e| e.archive_bytes)
            .sum()
    }

    /// Preserve a volume, then confirm the copy is intact.
    ///
    /// Returns an entry only when the archive has been verified. A caller may
    /// treat a returned entry — and nothing else — as permission to delete.
    pub fn store(
        &self,
        prober: &dyn DockerProbe,
        volume: &str,
        labels: &BTreeMap<String, String>,
        engine: Option<Engine>,
        expected_bytes: Option<Bytes>,
        now_unix: i64,
    ) -> Result<VaultEntry> {
        self.ensure()?;

        if let (Some(want), Some(free)) = (expected_bytes, free_space(&self.root)) {
            // Compressed archives are usually far smaller, but a database that
            // is mostly incompressible pages will not be. Refuse rather than
            // fill someone's disk halfway through a backup.
            let need = want.get().saturating_add(want.get() / 5);
            if free < need {
                return Err(Error::Config(format!(
                    "not enough room to preserve {volume}: need about {}, {} free at {}",
                    Bytes(need).human(),
                    Bytes(free).human(),
                    self.root.display()
                )));
            }
        }

        let id = format!("{}-{}", sanitise(volume), now_unix);
        let final_path = self.root.join(format!("{id}.tar.gz"));
        let part_path = self.root.join(format!("{id}.tar.gz.part"));
        if final_path.exists() {
            return Err(Error::Config(format!(
                "a vault entry named {id} already exists"
            )));
        }

        // Write the archive, then hash it *from disk*.
        //
        // An earlier version hashed during the write via a wrapper above the
        // gzip encoder. That was doubly wrong: it never called `update`, and
        // even fixed it would have digested the uncompressed stream while
        // `verify` reads the compressed file. Hashing what is actually on disk
        // is both simpler and the only thing that can catch a bad write.
        {
            let file = std::fs::File::create(&part_path).map_err(Error::Io)?;
            let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            if let Err(e) = prober.dump_volume(volume, &mut enc) {
                // Never leave a partial archive lying around looking like a
                // backup.
                drop(enc);
                let _ = std::fs::remove_file(&part_path);
                return Err(e);
            }
            let file = enc.finish().map_err(Error::Io)?;
            // fsync before we trust it. A rename that outruns the data leaves a
            // manifest pointing at nothing.
            file.sync_all().map_err(Error::Io)?;
        }

        let digest = match sha256_file(&part_path) {
            Ok(d) => d,
            Err(e) => {
                let _ = std::fs::remove_file(&part_path);
                return Err(e);
            }
        };

        let archive_bytes = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
        let entry_count = match count_entries(&part_path) {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&part_path);
                return Err(e);
            }
        };

        std::fs::rename(&part_path, &final_path).map_err(Error::Io)?;

        let mut entry = VaultEntry {
            id,
            volume: volume.to_string(),
            labels: labels.clone(),
            engine,
            archive_bytes: Bytes(archive_bytes),
            sha256: digest,
            entry_count,
            created_unix: now_unix,
            verified_unix: None,
        };

        // Verify from disk, not from memory. The point is to catch a bad write,
        // and comparing the hash we just computed against itself would catch
        // nothing.
        self.verify(&entry)?;
        entry.verified_unix = Some(now_unix);
        self.write_manifest(&entry)?;
        Ok(entry)
    }

    fn write_manifest(&self, entry: &VaultEntry) -> Result<()> {
        let path = self.root.join(entry.manifest_name());
        let text = serde_json::to_string_pretty(entry)?;
        std::fs::write(&path, text).map_err(Error::Io)
    }

    /// Re-read an archive and confirm it matches its manifest.
    pub fn verify(&self, entry: &VaultEntry) -> Result<()> {
        let path = self.root.join(entry.archive_name());
        let got = sha256_file(&path)?;
        if got != entry.sha256 {
            return Err(Error::Config(format!(
                "vault entry {} is corrupt: digest {} does not match {}",
                entry.id, got, entry.sha256
            )));
        }
        let n = count_entries(&path)?;
        if n != entry.entry_count {
            return Err(Error::Config(format!(
                "vault entry {} holds {n} entries, expected {}",
                entry.id, entry.entry_count
            )));
        }
        Ok(())
    }

    /// Put a volume back, exactly as it was.
    pub fn restore(&self, mutate: &dyn DockerMutate, entry: &VaultEntry) -> Result<()> {
        self.verify(entry)?;
        let path = self.root.join(entry.archive_name());
        let f = std::fs::File::open(&path).map_err(Error::Io)?;
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(f)
            .read_to_end(&mut tar)
            .map_err(Error::Io)?;
        mutate.restore_volume(&entry.volume, &entry.labels, tar)
    }

    /// Forget an entry. Explicit only — retention is "keep forever" by default,
    /// because a backup that quietly expires is not a backup.
    pub fn forget(&self, entry: &VaultEntry) -> Result<()> {
        std::fs::remove_file(self.root.join(entry.archive_name())).map_err(Error::Io)?;
        let _ = std::fs::remove_file(self.root.join(entry.manifest_name()));
        Ok(())
    }
}

/// Digest of a file exactly as it sits on disk.
fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).map_err(Error::Io)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Count tar entries inside a gzipped archive.
///
/// The second half of verification: a digest proves the bytes are unchanged,
/// and the entry count proves the archive is a plausible whole rather than a
/// well-formed prefix.
fn count_entries(path: &Path) -> Result<u64> {
    let f = std::fs::File::open(path).map_err(Error::Io)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut archive = tar::Archive::new(gz);
    let mut n = 0u64;
    for e in archive.entries().map_err(Error::Io)? {
        e.map_err(Error::Io)?;
        n += 1;
    }
    Ok(n)
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Volume names are daemon-supplied but end up in a filename.
fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_start_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::RawProbe;
    use std::io::Write;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pj-vault-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Produces a real tar stream, so the vault's verification is exercised
    /// against genuine archives rather than a stub.
    struct FakeProber {
        files: Vec<(String, Vec<u8>)>,
        fail: bool,
    }

    impl DockerProbe for FakeProber {
        fn probe_image(&self) -> Option<String> {
            Some("busybox".into())
        }
        fn probe_volumes(&self, _v: &[String]) -> Result<BTreeMap<String, RawProbe>> {
            Ok(BTreeMap::new())
        }
        fn dump_volume(&self, _name: &str, out: &mut dyn Write) -> Result<u64> {
            if self.fail {
                // Write something first, so the test proves a partial archive
                // is cleaned up rather than left looking like a backup.
                out.write_all(b"partial rubbish").map_err(Error::Io)?;
                return Err(Error::Api("daemon went away".into()));
            }
            let mut builder = tar::Builder::new(Vec::new());
            for (name, content) in &self.files {
                let mut h = tar::Header::new_gnu();
                h.set_path(name).unwrap();
                h.set_size(content.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                builder.append(&h, content.as_slice()).unwrap();
            }
            let data = builder.into_inner().unwrap();
            out.write_all(&data).map_err(Error::Io)?;
            Ok(data.len() as u64)
        }
    }

    fn prober(n: usize) -> FakeProber {
        FakeProber {
            files: (0..n)
                .map(|i| (format!("f{i}.dat"), vec![b'x'; 128]))
                .collect(),
            fail: false,
        }
    }

    #[test]
    fn a_stored_entry_is_verified_before_it_is_returned() {
        let d = tmp("store");
        let v = Vault::at(d.clone());
        let entry = v
            .store(&prober(3), "oak_mysql", &BTreeMap::new(), None, None, 1000)
            .unwrap();

        assert_eq!(entry.volume, "oak_mysql");
        assert_eq!(entry.entry_count, 3);
        assert_eq!(entry.sha256.len(), 64);
        assert!(
            entry.verified_unix.is_some(),
            "store must not hand back an unverified entry"
        );
        assert!(d.join(entry.archive_name()).is_file());
        assert!(d.join(entry.manifest_name()).is_file());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_failed_dump_leaves_no_partial_archive() {
        // The difference between a backup and the appearance of one.
        let d = tmp("fail");
        let v = Vault::at(d.clone());
        let err = v
            .store(
                &FakeProber {
                    files: vec![],
                    fail: true,
                },
                "risky",
                &BTreeMap::new(),
                None,
                None,
                1000,
            )
            .unwrap_err();
        assert!(matches!(err, Error::Api(_)));

        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed dump must clean up after itself: {leftovers:?}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn corruption_is_caught_by_verify() {
        let d = tmp("corrupt");
        let v = Vault::at(d.clone());
        let entry = v
            .store(&prober(2), "vol", &BTreeMap::new(), None, None, 1000)
            .unwrap();
        assert!(v.verify(&entry).is_ok());

        // Flip the archive's contents.
        let p = d.join(entry.archive_name());
        std::fs::write(&p, b"not a gzip at all").unwrap();
        assert!(
            v.verify(&entry).is_err(),
            "a rewritten archive must fail verification"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn truncation_is_caught_even_when_the_prefix_is_valid() {
        let d = tmp("truncate");
        let v = Vault::at(d.clone());
        let entry = v
            .store(&prober(6), "vol", &BTreeMap::new(), None, None, 1000)
            .unwrap();

        let p = d.join(entry.archive_name());
        let data = std::fs::read(&p).unwrap();
        std::fs::write(&p, &data[..data.len() / 2]).unwrap();
        assert!(v.verify(&entry).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn entries_are_listed_newest_first() {
        let d = tmp("list");
        let v = Vault::at(d.clone());
        v.store(&prober(1), "a", &BTreeMap::new(), None, None, 100)
            .unwrap();
        v.store(&prober(1), "b", &BTreeMap::new(), None, None, 300)
            .unwrap();
        v.store(&prober(1), "c", &BTreeMap::new(), None, None, 200)
            .unwrap();

        let got: Vec<String> = v.entries().unwrap().into_iter().map(|e| e.volume).collect();
        assert_eq!(got, vec!["b", "c", "a"]);
        assert!(v.total_bytes().get() > 0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn labels_survive_a_round_trip_through_the_manifest() {
        let d = tmp("labels");
        let v = Vault::at(d.clone());
        let mut labels = BTreeMap::new();
        labels.insert("com.docker.compose.project".to_string(), "oak".to_string());
        let entry = v
            .store(
                &prober(1),
                "oak_mysql",
                &labels,
                Some(Engine::MariaDb),
                None,
                1,
            )
            .unwrap();

        let read_back = v.entries().unwrap();
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].labels, labels);
        assert_eq!(read_back[0].engine, Some(Engine::MariaDb));
        assert_eq!(read_back[0].id, entry.id);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn forget_removes_both_archive_and_manifest() {
        let d = tmp("forget");
        let v = Vault::at(d.clone());
        let entry = v
            .store(&prober(1), "gone", &BTreeMap::new(), None, None, 1)
            .unwrap();
        v.forget(&entry).unwrap();
        assert!(v.entries().unwrap().is_empty());
        assert!(!d.join(entry.archive_name()).exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_hostile_volume_name_cannot_escape_the_vault() {
        assert_eq!(sanitise("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitise("oak_mysql"), "oak_mysql");
        assert!(!sanitise("/abs/path").contains('/'));
        assert!(!sanitise(".hidden").starts_with('.'));
    }

    #[test]
    fn the_vault_lives_outside_any_cache_directory() {
        // A cache is something the system may delete. These are backups.
        let v = Vault::open().unwrap();
        let p = v.root().to_string_lossy().to_lowercase();
        assert!(!p.contains("/caches/"), "{p}");
        assert!(!p.contains("/.cache/"), "{p}");
        assert!(p.contains("prune-juice"));
    }
}
