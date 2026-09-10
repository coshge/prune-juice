//! Prune Juice — Docker disk reclamation with provenance.
//!
//! This crate holds all the logic and is deliberately UI-agnostic: it never
//! prints, never checks for a TTY, and never assumes a terminal. Consumers (the
//! TUI, the NDJSON writer, a future FFI shim) render the `Event` stream however
//! they like.
//!
//! The lints below make that a compile error rather than a matter of discipline.

#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod docker;
pub mod error;
pub mod event;
pub mod execute;
pub mod json;
pub mod model;
pub mod plan;
pub mod probe;
pub mod providers;
pub mod scan;

pub use error::{Error, Result};
pub use event::{Cancel, Event, EventSink};
