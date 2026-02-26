//! Memory tracking for Bop script execution.
//!
//! With the `std` feature (default), uses thread-local counters so tracking is
//! zero-cost and perfectly isolated between concurrent executions.
//!
//! Without `std`, uses global statics. This is safe for single-threaded
//! environments (e.g., wasm) but is NOT thread-safe.
//!
//! Tracking happens at the Value layer: Clone tracks new allocations, Drop tracks
//! frees. The evaluator only needs to call `bop_memory_init()` at the start and
//! check `bop_memory_exceeded()` in `tick()`.

// ─── std: thread-local storage ──────────────────────────────────────────────

#[cfg(feature = "std")]
mod imp {
    use std::cell::Cell;

    thread_local! {
        static USED: Cell<usize> = const { Cell::new(0) };
        static LIMIT: Cell<usize> = const { Cell::new(usize::MAX) };
    }

    pub fn init(limit: usize) {
        USED.set(0);
        LIMIT.set(limit);
    }

    pub fn alloc(bytes: usize) {
        USED.with(|u| u.set(u.get().saturating_add(bytes)));
    }

    pub fn dealloc(bytes: usize) {
        USED.with(|u| u.set(u.get().saturating_sub(bytes)));
    }

    pub fn exceeded() -> bool {
        USED.with(|u| LIMIT.with(|l| u.get() > l.get()))
    }

    pub fn would_exceed(bytes: usize) -> bool {
        USED.with(|u| LIMIT.with(|l| u.get().saturating_add(bytes) > l.get()))
    }
}

// ─── no-std: global statics (single-threaded only) ──────────────────────────

#[cfg(not(feature = "std"))]
mod imp {
    use core::cell::Cell;

    // Safety: bop is single-threaded in no-std mode (e.g., wasm).
    // SyncCell lets us put Cell in a static.
    struct SyncCell(Cell<usize>);
    unsafe impl Sync for SyncCell {}

    static USED: SyncCell = SyncCell(Cell::new(0));
    static LIMIT: SyncCell = SyncCell(Cell::new(usize::MAX));

    pub fn init(limit: usize) {
        USED.0.set(0);
        LIMIT.0.set(limit);
    }

    pub fn alloc(bytes: usize) {
        USED.0.set(USED.0.get().saturating_add(bytes));
    }

    pub fn dealloc(bytes: usize) {
        USED.0.set(USED.0.get().saturating_sub(bytes));
    }

    pub fn exceeded() -> bool {
        USED.0.get() > LIMIT.0.get()
    }

    pub fn would_exceed(bytes: usize) -> bool {
        USED.0.get().saturating_add(bytes) > LIMIT.0.get()
    }
}

// ─── Public API (delegates to the active impl) ─────────────────────────────

/// Reset the counter and set the limit for this simulation.
pub fn bop_memory_init(limit: usize) {
    imp::init(limit);
}

/// Track a new heap allocation. Does not check the limit.
/// Called by Value's Clone impl and constructor helpers.
pub fn bop_alloc(bytes: usize) {
    imp::alloc(bytes);
}

/// Track a deallocation. Called by Value's Drop impl.
pub fn bop_dealloc(bytes: usize) {
    imp::dealloc(bytes);
}

/// Returns true if current usage exceeds the limit.
/// Checked in `tick()` to catch allocations from clones.
pub fn bop_memory_exceeded() -> bool {
    imp::exceeded()
}

/// Pre-flight check: would allocating `bytes` more exceed the limit?
/// Does NOT modify the counter. Use before creating large values
/// (string repeat, range) to avoid allocating memory we'll immediately reject.
pub fn bop_would_exceed(bytes: usize) -> bool {
    imp::would_exceed(bytes)
}
