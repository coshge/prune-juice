//! Waivers: "leave this alone, and here is why".
//!
//! Two house rules apply, and both exist because an unexplained exclusion is
//! worse than none:
//!
//! * **A waiver requires a human-authored reason.** The tool refuses to record
//!   one without. Six months later "why is this excluded" has to have an
//!   answer, and `--reason ok` is not one.
//! * **A waiver is granted on facts, and facts move.** Each one stores the
//!   evidence hash at the time it was written. When that changes, the waiver is
//!   surfaced as stale rather than honoured silently for ever — the situation
//!   it was granted for may no longer be the situation.
//!
//! Stored as JSON rather than SQLite: there is no index yet, and a half-built
//! database would be worse than a file that reviews cleanly in a diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::ResourceKind;

/// The shortest reason worth storing. Long enough to rule out "ok" and "keep",
/// short enough not to be a chore.
pub const MIN_REASON: usize = 12;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Waiver {
    /// `volume:oak_mysql`, `oak_mysql`, or a `prefix*`.
    pub selector: String,
    pub reason: String,
    pub created_unix: i64,
    /// The evidence hash when this was granted, where it was known. A change
    /// means the facts moved and the waiver should be re-affirmed.
    pub evidence_hash_at_creation: Option<String>,
}

impl Waiver {
    /// Does this waiver cover the named resource?
    pub fn covers(&self, kind: ResourceKind, name: &str) -> bool {
        let (want_kind, pattern) = match self.selector.split_once(':') {
            Some((k, rest)) => (Some(k), rest),
            None => (None, self.selector.as_str()),
        };
        if let Some(k) = want_kind {
            if k != kind.as_str() {
                return false;
            }
        }
        match pattern.strip_suffix('*') {
            Some(prefix) => !prefix.is_empty() && name.starts_with(prefix),
            None => name == pattern,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    waivers: Vec<Waiver>,
}

pub struct Waivers {
    path: PathBuf,
    entries: Vec<Waiver>,
}

impl Waivers {
    /// The user's config directory — a *config* location, not a cache: these
    /// are decisions the user made, and losing them silently would be rude.
    pub fn open() -> Result<Self> {
        let base = if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(x)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".config")
        } else {
            return Err(Error::Config("cannot locate a config directory".into()));
        };
        Ok(Self::at(base.join("prune-juice/waivers.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<File>(&t).ok())
            .map(|f| f.waivers)
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn all(&self) -> &[Waiver] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The waiver covering this resource, if any.
    pub fn covering(&self, kind: ResourceKind, name: &str) -> Option<&Waiver> {
        self.entries.iter().find(|w| w.covers(kind, name))
    }

    /// Record a waiver.
    ///
    /// Refuses an empty or perfunctory reason. This is deliberate friction: the
    /// cost of typing a sentence is paid once, and the cost of an unexplained
    /// exclusion is paid every time someone reads the list afterwards.
    pub fn add(
        &mut self,
        selector: &str,
        reason: &str,
        evidence_hash: Option<String>,
        now_unix: i64,
    ) -> Result<()> {
        let selector = selector.trim();
        let reason = reason.trim();
        if selector.is_empty() {
            return Err(Error::Config("a waiver needs a selector".into()));
        }
        if reason.chars().count() < MIN_REASON {
            return Err(Error::Config(format!(
                "a waiver needs a real reason ({MIN_REASON} characters or more) — \
                 six months from now it has to explain itself"
            )));
        }
        if selector == "*" {
            return Err(Error::Config(
                "a waiver of `*` would silence the whole tool; waive specific resources".into(),
            ));
        }

        self.entries.retain(|w| w.selector != selector);
        self.entries.push(Waiver {
            selector: selector.to_string(),
            reason: reason.to_string(),
            created_unix: now_unix,
            evidence_hash_at_creation: evidence_hash,
        });
        self.entries.sort_by(|a, b| a.selector.cmp(&b.selector));
        self.save()
    }

    pub fn remove(&mut self, selector: &str) -> Result<bool> {
        let before = self.entries.len();
        self.entries.retain(|w| w.selector != selector);
        let changed = self.entries.len() != before;
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        // Sorted keys and pretty output, so the file reviews cleanly in a diff.
        let text = serde_json::to_string_pretty(&File {
            waivers: self.entries.clone(),
        })?;
        std::fs::write(&self.path, text).map_err(Error::Io)
    }

    /// Waivers whose justifying evidence has since changed.
    ///
    /// Not removed automatically — that would be deciding on the user's behalf.
    /// Surfaced so they can look again.
    pub fn stale(&self, current: &BTreeMap<String, String>) -> Vec<&Waiver> {
        self.entries
            .iter()
            .filter(
                |w| match (&w.evidence_hash_at_creation, current.get(&w.selector)) {
                    (Some(then), Some(now)) => then != now,
                    _ => false,
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pj-waivers-{tag}-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn a_waiver_without_a_real_reason_is_refused() {
        let p = tmp("reason");
        let mut w = Waivers::at(p.clone());
        for bad in ["", "  ", "ok", "keep", "no", "later"] {
            assert!(
                w.add("volume:x", bad, None, 0).is_err(),
                "must refuse {bad:?}"
            );
        }
        assert!(w.is_empty());
        assert!(w
            .add(
                "volume:x",
                "client asked us to keep this until March",
                None,
                0
            )
            .is_ok());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_blanket_waiver_is_refused() {
        // `*` would silence the entire tool, which is not a waiver, it is
        // uninstalling it.
        let p = tmp("blanket");
        let mut w = Waivers::at(p.clone());
        assert!(w
            .add("*", "this is a perfectly long reason", None, 0)
            .is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn selectors_match_by_kind_name_and_prefix() {
        let mk = |sel: &str| Waiver {
            selector: sel.to_string(),
            reason: "a sufficiently long reason".into(),
            created_unix: 0,
            evidence_hash_at_creation: None,
        };

        // kind-qualified
        assert!(mk("volume:oak_mysql").covers(ResourceKind::Volume, "oak_mysql"));
        assert!(!mk("volume:oak_mysql").covers(ResourceKind::Image, "oak_mysql"));

        // bare name matches any kind
        assert!(mk("oak_mysql").covers(ResourceKind::Volume, "oak_mysql"));
        assert!(mk("oak_mysql").covers(ResourceKind::Container, "oak_mysql"));

        // prefix
        assert!(mk("volume:oak_*").covers(ResourceKind::Volume, "oak_mysql"));
        assert!(mk("volume:oak_*").covers(ResourceKind::Volume, "oak_s3"));
        assert!(!mk("volume:oak_*").covers(ResourceKind::Volume, "fen_mysql"));

        // a bare `*` never matches, even if one got into a file by hand
        assert!(!mk("*").covers(ResourceKind::Volume, "anything"));
        assert!(!mk("volume:*").covers(ResourceKind::Volume, "anything"));
    }

    #[test]
    fn waivers_round_trip_through_the_file() {
        let p = tmp("roundtrip");
        {
            let mut w = Waivers::at(p.clone());
            w.add(
                "volume:keepme",
                "holds the staging snapshot we still need",
                Some("abc123".into()),
                42,
            )
            .unwrap();
        }
        let w = Waivers::at(p.clone());
        assert_eq!(w.all().len(), 1);
        assert_eq!(w.all()[0].created_unix, 42);
        assert_eq!(
            w.all()[0].evidence_hash_at_creation.as_deref(),
            Some("abc123")
        );
        assert!(w.covering(ResourceKind::Volume, "keepme").is_some());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn adding_the_same_selector_twice_replaces_rather_than_duplicates() {
        let p = tmp("replace");
        let mut w = Waivers::at(p.clone());
        w.add("volume:x", "the first stated reason here", None, 1)
            .unwrap();
        w.add("volume:x", "a revised and clearer reason", None, 2)
            .unwrap();
        assert_eq!(w.all().len(), 1);
        assert_eq!(w.all()[0].reason, "a revised and clearer reason");
        assert_eq!(w.all()[0].created_unix, 2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn removing_reports_whether_it_did_anything() {
        let p = tmp("remove");
        let mut w = Waivers::at(p.clone());
        w.add("volume:x", "a sufficiently long reason", None, 0)
            .unwrap();
        assert!(w.remove("volume:x").unwrap());
        assert!(!w.remove("volume:x").unwrap(), "second removal is a no-op");
        assert!(w.is_empty());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_waiver_whose_evidence_moved_is_reported_stale_not_dropped() {
        // The facts a waiver was granted on can change. Honouring it silently
        // for ever would be a way to lose data on purpose.
        let p = tmp("stale");
        let mut w = Waivers::at(p.clone());
        w.add(
            "volume:x",
            "empty scratch volume, safe to keep around",
            Some("hash-at-grant".into()),
            0,
        )
        .unwrap();

        let mut now = BTreeMap::new();
        now.insert("volume:x".to_string(), "hash-at-grant".to_string());
        assert!(w.stale(&now).is_empty(), "unchanged evidence is not stale");

        now.insert("volume:x".to_string(), "something-else".to_string());
        let stale = w.stale(&now);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].selector, "volume:x");

        // Still honoured — surfacing is not the same as revoking.
        assert!(w.covering(ResourceKind::Volume, "x").is_some());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_missing_file_is_an_empty_set_not_an_error() {
        let w = Waivers::at(PathBuf::from("/nonexistent/pj/waivers.json"));
        assert!(w.is_empty());
        assert!(w.covering(ResourceKind::Volume, "x").is_none());
    }

    #[test]
    fn waivers_live_in_config_not_cache() {
        let w = Waivers::open().unwrap();
        let p = w.path().to_string_lossy().to_lowercase();
        assert!(!p.contains("cache"), "{p}");
        assert!(p.contains("prune-juice"));
    }
}
