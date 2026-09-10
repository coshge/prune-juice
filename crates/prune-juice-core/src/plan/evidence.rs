//! Read-set recording, and the evidence hash built from it.
//!
//! The central trick: **the evidence document IS the read-set.** Every fact the
//! classifier consults while deciding a resource's fate is recorded, and the
//! hash covers exactly those facts and nothing else.
//!
//! That gives two properties for free:
//!
//! * A fact we never read cannot invalidate the decision. Another project's
//!   container starting up does not spuriously cancel this deletion.
//! * A fact we did read cannot change silently. If it changes between planning
//!   and applying, the hash changes and the deletion is refused.
//!
//! It also satisfies the house `evidence_hash` convention without a second
//! mechanism.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One consulted fact. Rendered for humans, hashed for machines.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Fact {
    /// What the fact is about — a resource id, a path, a daemon.
    pub subject: String,
    /// Which property was read.
    pub key: String,
    /// What it said. Rendered as a stable string.
    pub value: String,
}

impl Fact {
    pub fn new(
        subject: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            subject: subject.into(),
            key: key.into(),
            value: value.into(),
        }
    }
}

/// The facts a decision rested on.
///
/// A `BTreeSet` rather than a `Vec`: the hash must not depend on the order the
/// classifier happened to read things in, and reading the same fact twice must
/// not change it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReadSet {
    facts: BTreeSet<Fact>,
}

impl ReadSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(
        &mut self,
        subject: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) {
        self.facts.insert(Fact::new(subject, key, value));
    }

    pub fn facts(&self) -> impl Iterator<Item = &Fact> {
        self.facts.iter()
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    pub fn merge(&mut self, other: &ReadSet) {
        for f in &other.facts {
            self.facts.insert(f.clone());
        }
    }

    /// Canonical digest of everything consulted.
    ///
    /// Length-prefixed so that `("ab", "c")` and `("a", "bc")` cannot collide.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"prune-juice/readset/v1");
        for f in &self.facts {
            for part in [&f.subject, &f.key, &f.value] {
                h.update((part.len() as u64).to_le_bytes());
                h.update(part.as_bytes());
            }
        }
        h.finalize().into()
    }

    pub fn hash_hex(&self) -> String {
        self.hash().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_order_independent() {
        let mut a = ReadSet::new();
        a.record("vol:x", "in_use", "false");
        a.record("vol:x", "size", "100");

        let mut b = ReadSet::new();
        b.record("vol:x", "size", "100");
        b.record("vol:x", "in_use", "false");

        assert_eq!(a.hash(), b.hash(), "read order must not affect the hash");
    }

    #[test]
    fn recording_the_same_fact_twice_is_idempotent() {
        let mut a = ReadSet::new();
        a.record("vol:x", "in_use", "false");
        let first = a.hash();
        a.record("vol:x", "in_use", "false");
        assert_eq!(a.len(), 1);
        assert_eq!(a.hash(), first);
    }

    #[test]
    fn a_changed_value_changes_the_hash() {
        // This is the whole point: if a fact we relied on moves, the plan is
        // stale and the deletion must be refused.
        let mut a = ReadSet::new();
        a.record("vol:x", "in_use", "false");
        let before = a.hash();

        let mut b = ReadSet::new();
        b.record("vol:x", "in_use", "true");
        assert_ne!(before, b.hash());
    }

    #[test]
    fn a_fact_we_never_read_cannot_invalidate_us() {
        let mut a = ReadSet::new();
        a.record("vol:x", "in_use", "false");
        let before = a.hash();

        // Something happened elsewhere in the world. We never consulted it, so
        // our decision stands.
        let mut world = ReadSet::new();
        world.record("vol:y", "in_use", "true");

        assert_eq!(a.hash(), before);
        assert_ne!(a.hash(), world.hash());
    }

    #[test]
    fn field_boundaries_cannot_be_confused() {
        let mut a = ReadSet::new();
        a.record("ab", "c", "d");
        let mut b = ReadSet::new();
        b.record("a", "bc", "d");
        assert_ne!(
            a.hash(),
            b.hash(),
            "length prefixing must prevent a concatenation collision"
        );
    }

    #[test]
    fn merge_is_a_union() {
        let mut a = ReadSet::new();
        a.record("x", "k", "1");
        let mut b = ReadSet::new();
        b.record("y", "k", "2");
        b.record("x", "k", "1");

        a.merge(&b);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn hex_is_64_chars() {
        let mut a = ReadSet::new();
        a.record("x", "k", "v");
        assert_eq!(a.hash_hex().len(), 64);
    }
}
