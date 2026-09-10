//! Ctrl-C, wired to the scan's own cancellation flag.
//!
//! Without this, `Error::Cancelled` and exit 130 were reachable only by
//! quitting the interface mid-scan: a Ctrl-C during a one-shot `--apply` killed
//! the process wherever it happened to be, and the receipt — the record of what
//! had already been deleted — was never written. The executor works one item at
//! a time so nothing could be torn in half, but "we deleted eleven things and
//! cannot tell you which" is not an acceptable answer.
//!
//! Two signals, one flag, and a second press that does not wait:
//!
//! * the first sets [`Cancel`], and the work stops at its next checkpoint and
//!   reports what it did;
//! * a second means the first did not take effect fast enough for the person
//!   pressing it, so the process leaves immediately with 130.
//!
//! The handler itself does nothing but an atomic swap and, at most, `_exit` —
//! both async-signal-safe. It never allocates, locks, or prints.
//!
//! No new dependency: `signal` and `_exit` are two symbols from the platform's
//! own C library, declared here. Pulling in `libc` to name them would add a
//! crate to a graph deliberately kept at a size a person can audit, and this
//! is the whole of what would be used from it.

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use prune_juice_core::Cancel;

/// The flag the handler sets. A raw pointer because a signal handler may not
/// touch anything that could allocate or lock, and null until `install` runs.
static FLAG: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(unix)]
mod sys {
    pub const SIGINT: i32 = 2;
    pub const SIGTERM: i32 = 15;

    pub type Handler = extern "C" fn(i32);

    extern "C" {
        /// `sighandler_t signal(int, sighandler_t)`. The return is a
        /// pointer-sized handler value we have no use for.
        pub fn signal(signum: i32, handler: Handler) -> usize;
        /// Terminate without running atexit handlers or flushing buffers.
        /// One of the few calls a signal handler may make.
        pub fn _exit(code: i32) -> !;
    }
}

#[cfg(unix)]
extern "C" fn on_interrupt(_signum: i32) {
    let ptr = FLAG.load(Ordering::SeqCst);
    if ptr.is_null() {
        // Nothing to cancel — behave like the default handler would.
        unsafe { sys::_exit(130) }
    }
    // SAFETY: the pointer came from `Cancel::leak_static`, so the allocation
    // outlives the process and the only operation performed on it is an
    // atomic swap.
    let already = unsafe { &*ptr }.swap(true, Ordering::SeqCst);
    if already {
        // Asked twice. The second press is not a request to be patient.
        unsafe { sys::_exit(130) }
    }
}

/// Ask for a graceful stop on the next Ctrl-C (or `SIGTERM`).
///
/// Call once, with the [`Cancel`] the run will actually consult. On platforms
/// without unix signals this does nothing and Ctrl-C keeps its default
/// behaviour, which is the same as it was before.
pub fn install(cancel: &Cancel) {
    #[cfg(unix)]
    {
        let flag: &'static AtomicBool = cancel.leak_static();
        FLAG.store(
            flag as *const AtomicBool as *mut AtomicBool,
            Ordering::SeqCst,
        );
        // SAFETY: installing a handler that only does atomic work. The return
        // value is the previous handler, which we do not chain to.
        unsafe {
            sys::signal(sys::SIGINT, on_interrupt);
            sys::signal(sys::SIGTERM, on_interrupt);
        }
    }
    #[cfg(not(unix))]
    let _ = cancel;
}

/// True once an interrupt has been seen, for a caller deciding how to word
/// what it prints.
pub fn interrupted() -> bool {
    let ptr = FLAG.load(Ordering::SeqCst);
    // SAFETY: as `on_interrupt` — the allocation is leaked and immortal.
    !ptr.is_null() && unsafe { &*ptr }.load(Ordering::SeqCst)
}
