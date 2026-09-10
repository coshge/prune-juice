//! The provenance index: the memory Docker throws away.
//!
//! Labels only live as long as the object carrying them. `docker container
//! prune` destroys the only surviving link between an anonymous volume and its
//! project, permanently — on the reference machine 50 volumes are already in
//! that state, unattributable for ever. Nothing can recover those. What an
//! index can do is make sure it never happens again.
//!
//! It also answers a question Docker cannot: **how long has this been like
//! this?** `CreatedAt` is the only timestamp a volume carries and it diverges
//! from real last-write by months. "Absent across six scans over 41 days" is a
//! fact only a tool that kept notes can state, and it is exactly what stands
//! between a considered orphan verdict and mass-orphaning a root because a
//! disk happened to be unplugged during one run.
//!
//! Keyed by daemon ID throughout, never context name: two contexts can be the
//! same engine, and evidence must not leak between genuinely different ones.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Error, Result};
use crate::model::{Bytes, DaemonId, ResourceKind, ResourceSummary};

const SCHEMA_VERSION: i64 = 2;

pub struct Index {
    conn: Connection,
}

/// A remembered `(project_name, project_path)` for a volume. Either half can be
/// missing: a container may name its project without recording a path.
pub type RememberedOwner = (Option<String>, Option<String>);

/// What the index remembers about a resource across runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    pub first_seen_unix: i64,
    pub last_seen_unix: i64,
    /// How many distinct scans have observed it.
    pub scans: u32,
}

/// What the index remembers about a project directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectHistory {
    pub first_seen_unix: i64,
    pub last_seen_unix: i64,
    /// Scans in which the directory was present.
    pub present_scans: u32,
    /// Consecutive scans in which it was missing. This is the number that
    /// licenses an orphan verdict, and one observation is never enough.
    pub absent_scans: u32,
    /// When it was first noticed missing.
    pub absent_since_unix: Option<i64>,
}

impl Index {
    /// Open the index in the platform data directory, creating it if needed.
    pub fn open() -> Result<Self> {
        let base = if let Ok(x) = std::env::var("XDG_DATA_HOME") {
            std::path::PathBuf::from(x)
        } else if let Ok(home) = std::env::var("HOME") {
            if cfg!(target_os = "macos") {
                std::path::PathBuf::from(home).join("Library/Application Support")
            } else {
                std::path::PathBuf::from(home).join(".local/share")
            }
        } else {
            return Err(Error::Config("cannot locate a data directory".into()));
        };
        let dir = base.join("prune-juice");
        std::fs::create_dir_all(&dir).map_err(Error::Io)?;
        Self::at(dir.join("index.db"))
    }

    pub fn at(path: std::path::PathBuf) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// In-memory, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL so a long scan does not block a second invocation, and a busy
        // timeout so two runs queue rather than one failing outright.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let idx = Self { conn };
        idx.migrate()?;
        Ok(idx)
    }

    fn migrate(&self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if current >= SCHEMA_VERSION {
            return Ok(());
        }

        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS daemon (
                id          TEXT PRIMARY KEY,
                first_seen  INTEGER NOT NULL,
                last_seen   INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS scan (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                daemon_id   TEXT NOT NULL,
                started_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS scan_daemon ON scan(daemon_id, started_at);

            CREATE TABLE IF NOT EXISTS resource (
                daemon_id   TEXT NOT NULL,
                kind        TEXT NOT NULL,
                name        TEXT NOT NULL,
                first_seen  INTEGER NOT NULL,
                last_seen   INTEGER NOT NULL,
                scans       INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (daemon_id, kind, name)
            );

            -- The memory Docker throws away. A container's mount list is the
            -- only link from an anonymous volume to a project, and removing the
            -- container destroys it. Recorded here first, so it survives.
            CREATE TABLE IF NOT EXISTS edge (
                daemon_id     TEXT NOT NULL,
                container     TEXT NOT NULL,
                volume        TEXT NOT NULL,
                project_name  TEXT,
                project_path  TEXT,
                first_seen    INTEGER NOT NULL,
                last_seen     INTEGER NOT NULL,
                PRIMARY KEY (daemon_id, container, volume)
            );
            CREATE INDEX IF NOT EXISTS edge_volume ON edge(daemon_id, volume);

            -- Measured volume sizes. `/system/df` is the slowest call the scan
            -- makes (~1.4 s on the reference machine, minutes on some setups),
            -- and an *unreferenced* volume's size cannot change while nothing
            -- is mounting it — so a run that skips the call still has a
            -- figure, clearly labelled as remembered rather than measured.
            CREATE TABLE IF NOT EXISTS volume_size (
                daemon_id    TEXT NOT NULL,
                name         TEXT NOT NULL,
                bytes        INTEGER NOT NULL,
                measured_at  INTEGER NOT NULL,
                PRIMARY KEY (daemon_id, name)
            );

            CREATE TABLE IF NOT EXISTS project (
                daemon_id      TEXT NOT NULL,
                path           TEXT NOT NULL,
                name           TEXT NOT NULL,
                first_seen     INTEGER NOT NULL,
                last_seen      INTEGER NOT NULL,
                present_scans  INTEGER NOT NULL DEFAULT 0,
                absent_scans   INTEGER NOT NULL DEFAULT 0,
                absent_since   INTEGER,
                PRIMARY KEY (daemon_id, path)
            );
            "#,
        )?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Begin a scan and return its id.
    pub fn begin_scan(&self, daemon: &DaemonId, now_unix: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO daemon(id, first_seen, last_seen) VALUES (?1, ?2, ?2)
             ON CONFLICT(id) DO UPDATE SET last_seen = ?2",
            params![daemon.as_str(), now_unix],
        )?;
        self.conn.execute(
            "INSERT INTO scan(daemon_id, started_at) VALUES (?1, ?2)",
            params![daemon.as_str(), now_unix],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record what a scan saw.
    ///
    /// **This must be committed before anything deletes a container.** A
    /// container's labels and mounts are the only place an anonymous volume's
    /// provenance lives; removing it first would mean deleting the evidence
    /// before writing it down.
    pub fn record_scan(
        &self,
        daemon: &DaemonId,
        resources: &[ResourceSummary],
        now_unix: i64,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        for r in resources {
            tx.execute(
                "INSERT INTO resource(daemon_id, kind, name, first_seen, last_seen, scans)
                 VALUES (?1, ?2, ?3, ?4, ?4, 1)
                 ON CONFLICT(daemon_id, kind, name) DO UPDATE SET
                    last_seen = ?4, scans = scans + 1",
                params![daemon.as_str(), r.kind.as_str(), r.name, now_unix],
            )?;
        }

        // Edges, harvested from containers in every state.
        for c in resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Container)
        {
            let project_name = c
                .label_nonempty(crate::providers::DDEV_SITE_NAME)
                .or_else(|| c.label_nonempty(crate::providers::COMPOSE_PROJECT));
            let project_path = c
                .label_nonempty(crate::providers::DDEV_APPROOT)
                .or_else(|| c.label_nonempty(crate::providers::DEVCONTAINER_FOLDER))
                .or_else(|| c.label_nonempty(crate::providers::COMPOSE_WORKING_DIR));

            for volume in &c.mounts {
                tx.execute(
                    "INSERT INTO edge(daemon_id, container, volume, project_name, project_path,
                                      first_seen, last_seen)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                     ON CONFLICT(daemon_id, container, volume) DO UPDATE SET
                        last_seen = ?6,
                        project_name = COALESCE(?4, project_name),
                        project_path = COALESCE(?5, project_path)",
                    params![
                        daemon.as_str(),
                        c.name,
                        volume,
                        project_name,
                        project_path,
                        now_unix
                    ],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Remember the sizes a `df` just measured.
    ///
    /// Written as soon as the figures arrive, before anything is judged with
    /// them, for the same reason [`Self::record_scan`] is.
    pub fn record_volume_sizes(
        &self,
        daemon: &DaemonId,
        sizes: &BTreeMap<String, Bytes>,
        now_unix: i64,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (name, bytes) in sizes {
            tx.execute(
                "INSERT INTO volume_size(daemon_id, name, bytes, measured_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(daemon_id, name) DO UPDATE SET
                    bytes = ?3, measured_at = ?4",
                params![daemon.as_str(), name, bytes.get() as i64, now_unix],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Sizes measured on a previous run, with when each was measured.
    ///
    /// A remembered size is only usable for a volume nothing is mounting: an
    /// in-use volume is being written to as we speak, and reporting last
    /// week's figure for it would be inventing a measurement. The caller
    /// enforces that; this call just hands over what is remembered.
    pub fn volume_sizes(&self, daemon: &DaemonId) -> Result<BTreeMap<String, (Bytes, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, bytes, measured_at FROM volume_size WHERE daemon_id = ?1")?;
        let rows = stmt.query_map(params![daemon.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    Bytes(r.get::<_, i64>(1)?.max(0) as u64),
                    r.get::<_, i64>(2)?,
                ),
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (name, v) = row?;
            out.insert(name, v);
        }
        Ok(out)
    }

    /// Record whether each known project directory was present this scan.
    ///
    /// `absent_scans` only advances on a *consecutive* miss, and any sighting
    /// resets it. That is what makes "absent across six scans" mean something.
    pub fn record_projects(
        &self,
        daemon: &DaemonId,
        projects: &[(String, String, bool)],
        now_unix: i64,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (path, name, present) in projects {
            if *present {
                tx.execute(
                    "INSERT INTO project(daemon_id, path, name, first_seen, last_seen,
                                         present_scans, absent_scans, absent_since)
                     VALUES (?1, ?2, ?3, ?4, ?4, 1, 0, NULL)
                     ON CONFLICT(daemon_id, path) DO UPDATE SET
                        last_seen = ?4,
                        present_scans = present_scans + 1,
                        absent_scans = 0,
                        absent_since = NULL",
                    params![daemon.as_str(), path, name, now_unix],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO project(daemon_id, path, name, first_seen, last_seen,
                                         present_scans, absent_scans, absent_since)
                     VALUES (?1, ?2, ?3, ?4, ?4, 0, 1, ?4)
                     ON CONFLICT(daemon_id, path) DO UPDATE SET
                        last_seen = ?4,
                        absent_scans = absent_scans + 1,
                        absent_since = COALESCE(absent_since, ?4)",
                    params![daemon.as_str(), path, name, now_unix],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn history(
        &self,
        daemon: &DaemonId,
        kind: ResourceKind,
        name: &str,
    ) -> Result<Option<History>> {
        let got = self
            .conn
            .query_row(
                "SELECT first_seen, last_seen, scans FROM resource
                 WHERE daemon_id = ?1 AND kind = ?2 AND name = ?3",
                params![daemon.as_str(), kind.as_str(), name],
                |r| {
                    Ok(History {
                        first_seen_unix: r.get(0)?,
                        last_seen_unix: r.get(1)?,
                        scans: r.get::<_, i64>(2)? as u32,
                    })
                },
            )
            .optional()?;
        Ok(got)
    }

    pub fn project_history(&self, daemon: &DaemonId, path: &str) -> Result<Option<ProjectHistory>> {
        let got = self
            .conn
            .query_row(
                "SELECT first_seen, last_seen, present_scans, absent_scans, absent_since
                 FROM project WHERE daemon_id = ?1 AND path = ?2",
                params![daemon.as_str(), path],
                |r| {
                    Ok(ProjectHistory {
                        first_seen_unix: r.get(0)?,
                        last_seen_unix: r.get(1)?,
                        present_scans: r.get::<_, i64>(2)? as u32,
                        absent_scans: r.get::<_, i64>(3)? as u32,
                        absent_since_unix: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(got)
    }

    /// Who a volume used to belong to, from a container that may be long gone.
    ///
    /// The whole reason the index exists. Returns `(project_name, project_path)`
    /// for the most recently observed edge.
    pub fn historical_owner(
        &self,
        daemon: &DaemonId,
        volume: &str,
    ) -> Result<Option<RememberedOwner>> {
        let got = self
            .conn
            .query_row(
                "SELECT project_name, project_path FROM edge
                 WHERE daemon_id = ?1 AND volume = ?2
                   AND (project_name IS NOT NULL OR project_path IS NOT NULL)
                 ORDER BY last_seen DESC LIMIT 1",
                params![daemon.as_str(), volume],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(got)
    }

    /// All remembered volume→owner mappings for a daemon.
    pub fn all_historical_owners(
        &self,
        daemon: &DaemonId,
    ) -> Result<BTreeMap<String, RememberedOwner>> {
        let mut stmt = self.conn.prepare(
            "SELECT volume, project_name, project_path FROM edge
             WHERE daemon_id = ?1
               AND (project_name IS NOT NULL OR project_path IS NOT NULL)
             ORDER BY last_seen ASC",
        )?;
        let rows = stmt.query_map(params![daemon.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ),
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (v, owner) = row?;
            // Ascending order means the newest observation wins.
            out.insert(v, owner);
        }
        Ok(out)
    }

    pub fn scan_count(&self, daemon: &DaemonId) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM scan WHERE daemon_id = ?1",
            params![daemon.as_str()],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    pub fn edge_count(&self, daemon: &DaemonId) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM edge WHERE daemon_id = ?1",
            params![daemon.as_str()],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceKind;

    fn daemon() -> DaemonId {
        DaemonId("ENGINE-A".into())
    }

    fn container(name: &str, project: &str, path: &str, mounts: &[&str]) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Container, name, name);
        r.labels
            .insert(crate::providers::COMPOSE_PROJECT.into(), project.into());
        r.labels
            .insert(crate::providers::COMPOSE_WORKING_DIR.into(), path.into());
        r.mounts = mounts.iter().map(|s| s.to_string()).collect();
        r
    }

    fn volume(name: &str) -> ResourceSummary {
        ResourceSummary::new(ResourceKind::Volume, name, name)
    }

    #[test]
    fn a_fresh_index_migrates_and_is_empty() {
        let idx = Index::in_memory().unwrap();
        assert_eq!(idx.scan_count(&daemon()).unwrap(), 0);
        assert_eq!(idx.edge_count(&daemon()).unwrap(), 0);
        assert!(idx
            .history(&daemon(), ResourceKind::Volume, "anything")
            .unwrap()
            .is_none());
    }

    #[test]
    fn migration_is_idempotent() {
        let idx = Index::in_memory().unwrap();
        idx.migrate().unwrap();
        idx.migrate().unwrap();
        assert_eq!(idx.scan_count(&daemon()).unwrap(), 0);
    }

    #[test]
    fn provenance_survives_the_container_that_carried_it() {
        // The whole point. A container names the project and mounts the volume;
        // once it is gone Docker has no way to connect the two, and the index
        // does.
        let idx = Index::in_memory().unwrap();
        let anon = "a".repeat(64);

        idx.begin_scan(&daemon(), 1000).unwrap();
        idx.record_scan(
            &daemon(),
            &[
                container("oak-wp-1", "oak", "/r/oak", &[&anon]),
                volume(&anon),
            ],
            1000,
        )
        .unwrap();

        // Later scan: the container is gone entirely.
        idx.begin_scan(&daemon(), 2000).unwrap();
        idx.record_scan(&daemon(), &[volume(&anon)], 2000).unwrap();

        let owner = idx.historical_owner(&daemon(), &anon).unwrap().unwrap();
        assert_eq!(owner.0.as_deref(), Some("oak"));
        assert_eq!(owner.1.as_deref(), Some("/r/oak"));
    }

    #[test]
    fn resource_history_counts_scans_not_rows() {
        let idx = Index::in_memory().unwrap();
        for t in [100, 200, 300] {
            idx.begin_scan(&daemon(), t).unwrap();
            idx.record_scan(&daemon(), &[volume("v")], t).unwrap();
        }
        let h = idx
            .history(&daemon(), ResourceKind::Volume, "v")
            .unwrap()
            .unwrap();
        assert_eq!(h.first_seen_unix, 100);
        assert_eq!(h.last_seen_unix, 300);
        assert_eq!(h.scans, 3);
    }

    #[test]
    fn absent_scans_accumulate_and_a_sighting_resets_them() {
        // This is the F3 mitigation: one missing observation must never be
        // enough, and an unplugged disk that comes back must clear the count.
        let idx = Index::in_memory().unwrap();
        let p = "/r/nbk".to_string();

        idx.record_projects(&daemon(), &[(p.clone(), "nbk".into(), true)], 100)
            .unwrap();
        assert_eq!(
            idx.project_history(&daemon(), &p)
                .unwrap()
                .unwrap()
                .absent_scans,
            0
        );

        for t in [200, 300, 400] {
            idx.record_projects(&daemon(), &[(p.clone(), "nbk".into(), false)], t)
                .unwrap();
        }
        let h = idx.project_history(&daemon(), &p).unwrap().unwrap();
        assert_eq!(h.absent_scans, 3);
        assert_eq!(h.absent_since_unix, Some(200));

        // The disk gets plugged back in.
        idx.record_projects(&daemon(), &[(p.clone(), "nbk".into(), true)], 500)
            .unwrap();
        let h = idx.project_history(&daemon(), &p).unwrap().unwrap();
        assert_eq!(h.absent_scans, 0, "a sighting must reset the count");
        assert!(h.absent_since_unix.is_none());
    }

    #[test]
    fn measured_volume_sizes_are_remembered_and_keyed_by_daemon() {
        let ix = Index::in_memory().unwrap();
        let mut sizes = BTreeMap::new();
        sizes.insert("oak_mysql".to_string(), Bytes(1_200_000_000));
        ix.record_volume_sizes(&daemon(), &sizes, 1000).unwrap();

        let back = ix.volume_sizes(&daemon()).unwrap();
        assert_eq!(back.get("oak_mysql"), Some(&(Bytes(1_200_000_000), 1000)));

        // A re-measurement replaces the figure rather than accumulating rows.
        sizes.insert("oak_mysql".to_string(), Bytes(900_000_000));
        ix.record_volume_sizes(&daemon(), &sizes, 2000).unwrap();
        let back = ix.volume_sizes(&daemon()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.get("oak_mysql"), Some(&(Bytes(900_000_000), 2000)));

        // Another engine's measurement is not this engine's.
        assert!(ix
            .volume_sizes(&DaemonId("OTHER".into()))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn evidence_does_not_leak_between_daemons() {
        // Two engines are two different worlds. Attributing one's volume from
        // the other's history would be worse than knowing nothing.
        let idx = Index::in_memory().unwrap();
        let a = DaemonId("ENGINE-A".into());
        let b = DaemonId("ENGINE-B".into());

        idx.begin_scan(&a, 100).unwrap();
        idx.record_scan(&a, &[container("c", "proj", "/r/proj", &["shared"])], 100)
            .unwrap();

        assert!(idx.historical_owner(&a, "shared").unwrap().is_some());
        assert!(
            idx.historical_owner(&b, "shared").unwrap().is_none(),
            "a different engine must see nothing"
        );
    }

    #[test]
    fn the_newest_observation_of_an_owner_wins() {
        // A volume can be re-used by a different project. The latest sighting
        // is the one that matters.
        let idx = Index::in_memory().unwrap();
        idx.record_scan(
            &daemon(),
            &[container("old", "first", "/r/first", &["v"])],
            100,
        )
        .unwrap();
        idx.record_scan(
            &daemon(),
            &[container("new", "second", "/r/second", &["v"])],
            200,
        )
        .unwrap();

        let all = idx.all_historical_owners(&daemon()).unwrap();
        assert_eq!(all.get("v").unwrap().0.as_deref(), Some("second"));
    }

    #[test]
    fn an_edge_is_updated_not_duplicated() {
        let idx = Index::in_memory().unwrap();
        for t in [100, 200, 300] {
            idx.record_scan(&daemon(), &[container("c", "p", "/r/p", &["v"])], t)
                .unwrap();
        }
        assert_eq!(idx.edge_count(&daemon()).unwrap(), 1);
    }

    #[test]
    fn a_container_without_labels_still_records_the_mount() {
        // No project to name, but the container→volume link is still worth
        // keeping: it proves the volume was in use by something.
        let idx = Index::in_memory().unwrap();
        let mut c = ResourceSummary::new(ResourceKind::Container, "bare", "bare");
        c.mounts = vec!["v".into()];
        idx.record_scan(&daemon(), &[c], 100).unwrap();

        assert_eq!(idx.edge_count(&daemon()).unwrap(), 1);
        // But it must not claim an owner it never saw.
        assert!(idx.historical_owner(&daemon(), "v").unwrap().is_none());
    }
}
