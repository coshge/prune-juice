//! The NDJSON wire contract.
//!
//! One `Envelope` per line, flushed per event. Not a single document — the
//! whole point is that a consumer renders progressively.
//!
//! Versioning rules (also in docs/protocol.md, enforced by a CI schema diff):
//!   1. `v` bumps ONLY on a breaking change: a removed field, a renamed field,
//!      a changed type, or a changed meaning.
//!   2. Adding an `Event` variant is NOT breaking. Consumers MUST ignore
//!      unknown `event` values.
//!   3. Adding an optional field is NOT breaking. Consumers MUST ignore
//!      unknown fields.

use serde::{Deserialize, Serialize};

use crate::event::Event;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol version. See the rules above.
    pub v: u32,
    /// Monotonic, starts at 0, gap-free. Consumers assert this to detect a
    /// dropped line.
    pub seq: u64,
    /// Unix milliseconds.
    pub ts: u64,
    #[serde(flatten)]
    pub event: Event,
}

impl Envelope {
    pub fn new(seq: u64, event: Event) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            seq,
            ts: now_millis(),
            event,
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Phase;

    #[test]
    fn envelope_flattens_the_event_tag() {
        let e = Envelope::new(
            7,
            Event::Phase {
                phase: Phase::Sizing,
                done: 2,
                total: Some(9),
            },
        );
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        // The event tag must sit alongside v/seq/ts, not nested under "event".
        assert_eq!(v["v"], 1);
        assert_eq!(v["seq"], 7);
        assert_eq!(v["event"], "phase");
        assert_eq!(v["phase"], "sizing");
        assert_eq!(v["done"], 2);
    }

    #[test]
    fn envelope_round_trips() {
        let e = Envelope::new(
            0,
            Event::ScanFinished {
                totals: Default::default(),
                duration_ms: 12,
                stale: false,
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.seq, 0);
        assert!(matches!(back.event, Event::ScanFinished { .. }));
    }
}
