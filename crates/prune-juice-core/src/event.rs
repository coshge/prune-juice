//! The streaming model.
//!
//! A callback trait, deliberately — not a `Stream`, not an `mpsc::Receiver`.
//! It is the lowest common denominator of the three consumers we care about:
//! the terminal UI, an NDJSON subprocess reader, and (if it ever happens) a
//! UniFFI callback interface. A stream or a channel can be built from a
//! callback in ten lines; a callback cannot be built from a stream across an
//! FFI boundary without an executor on the foreign side.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{
    Bytes, DaemonId, ProjectSummary, ResourceId, ResourceSummary, RuntimeFlavor, SizeSource, Totals,
};

/// Everything the engine reports while working.
///
/// FFI-compatible by construction: no generics, no lifetimes, no references,
/// no trait objects in fields, every variant named.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    ScanStarted {
        daemon: DaemonId,
        context: String,
        runtime: RuntimeFlavor,
        api_version: String,
    },
    Phase {
        phase: Phase,
        done: u32,
        total: Option<u32>,
    },
    ResourceFound {
        resource: ResourceSummary,
    },
    ProjectFound {
        project: ProjectSummary,
    },
    SizeUpdated {
        id: ResourceId,
        bytes: Bytes,
        source: SizeSource,
    },
    Warning {
        code: String,
        message: String,
        resource: Option<ResourceId>,
    },
    ScanFinished {
        totals: Totals,
        duration_ms: u64,
        stale: bool,
    },
    /// A host disk measurement, before or after a run.
    HostMeasured {
        phase: String,
        physical: Option<Bytes>,
        fs_free: Option<Bytes>,
    },
    /// What the host actually gave back, versus what Docker claimed.
    HostReclaim {
        docker_reported: Bytes,
        host_measured: Option<Bytes>,
        confidence: String,
    },
    /// One resource's verdict. Emitted after planning so a machine consumer —
    /// or an audit — can see exactly what was decided and why.
    Classified {
        kind: String,
        name: String,
        tier: String,
        because: String,
        size: Option<Bytes>,
        owner: Option<String>,
        provenance: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Contexts,
    Listing,
    Inspecting,
    Sizing,
    Probing,
    Attributing,
}

/// Where events go. Implementations must be cheap and non-blocking; the engine
/// calls `emit` from worker threads.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
    fn flush(&self) {}
}

/// Discards everything.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: Event) {}
}

/// Broadcasts to several sinks.
pub struct FanoutSink(pub Vec<Arc<dyn EventSink>>);

impl EventSink for FanoutSink {
    fn emit(&self, event: Event) {
        for sink in &self.0 {
            sink.emit(event.clone());
        }
    }
    fn flush(&self) {
        for sink in &self.0 {
            sink.flush();
        }
    }
}

/// Adapts the callback into a channel, for consumers that want to pull.
pub struct ChannelSink(pub mpsc::Sender<Event>);

impl EventSink for ChannelSink {
    fn emit(&self, event: Event) {
        // A closed receiver is not an error; the consumer stopped caring.
        let _ = self.0.send(event);
    }
}

/// Writes NDJSON, one `Envelope` per line, flushed per event so a subprocess
/// reader gets progressive output rather than one blob at exit.
pub struct JsonlSink<W: Write + Send> {
    writer: Mutex<W>,
    seq: AtomicU64,
}

impl<W: Write + Send> JsonlSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
            seq: AtomicU64::new(0),
        }
    }
}

impl<W: Write + Send> EventSink for JsonlSink<W> {
    fn emit(&self, event: Event) {
        let envelope = crate::json::Envelope::new(self.seq.fetch_add(1, Ordering::SeqCst), event);
        if let Ok(line) = serde_json::to_string(&envelope) {
            if let Ok(mut w) = self.writer.lock() {
                let _ = writeln!(w, "{line}");
                let _ = w.flush();
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.flush();
        }
    }
}

/// Cooperative cancellation.
///
/// Deliberately not `tokio_util::CancellationToken` — its `cancelled()` is
/// async, which cannot cross an FFI boundary. An `AtomicBool` polled between
/// awaits is more than fine at ~650 resources, and becomes a UniFFI object
/// with one method.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Call between units of work.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_starts_clear_and_latches() {
        let c = Cancel::new();
        assert!(!c.is_cancelled());
        assert!(c.check().is_ok());
        c.cancel();
        assert!(c.is_cancelled());
        assert!(matches!(c.check(), Err(Error::Cancelled)));
    }

    #[test]
    fn cancel_clone_shares_state() {
        let a = Cancel::new();
        let b = a.clone();
        a.cancel();
        assert!(b.is_cancelled(), "clones must share the same flag");
    }

    #[test]
    fn jsonl_sink_writes_one_line_per_event_with_monotonic_seq() {
        #[derive(Clone)]
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = JsonlSink::new(Shared(buf.clone()));
        for i in 0..3 {
            sink.emit(Event::Phase {
                phase: Phase::Listing,
                done: i,
                total: Some(3),
            });
        }

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["v"], 1);
            assert_eq!(v["seq"], i as u64);
            assert_eq!(v["event"], "phase");
        }
    }
}
