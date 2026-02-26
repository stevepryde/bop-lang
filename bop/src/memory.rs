//! Thread-local memory tracking for Bop script execution.
//!
//! Each simulation runs in its own thread. We use thread-local counters so
//! tracking is zero-cost (no Arc/Mutex) and perfectly isolated between
//! concurrent executions.
//!
//! Tracking happens at the Value layer: Clone tracks new allocations, Drop tracks
//! frees. The evaluator only needs to call `bop_memory_init()` at the start and
//! check `bop_memory_exceeded()` in `tick()`.

use std::cell::Cell;

thread_local! {
    static USED: Cell<usize> = const { Cell::new(0) };
    static LIMIT: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Reset the counter and set the limit for this simulation.
pub fn bop_memory_init(limit: usize) {
    USED.set(0);
    LIMIT.set(limit);
}

/// Track a new heap allocation. Does not check the limit.
/// Called by Value's Clone impl and constructor helpers.
pub fn bop_alloc(bytes: usize) {
    USED.with(|u| u.set(u.get().saturating_add(bytes)));
}

/// Track a deallocation. Called by Value's Drop impl.
pub fn bop_dealloc(bytes: usize) {
    USED.with(|u| u.set(u.get().saturating_sub(bytes)));
}

/// Returns true if current usage exceeds the limit.
/// Checked in `tick()` to catch allocations from clones.
pub fn bop_memory_exceeded() -> bool {
    USED.with(|u| LIMIT.with(|l| u.get() > l.get()))
}

/// Pre-flight check: would allocating `bytes` more exceed the limit?
/// Does NOT modify the counter. Use before creating large values
/// (string repeat, range) to avoid allocating memory we'll immediately reject.
pub fn bop_would_exceed(bytes: usize) -> bool {
    USED.with(|u| LIMIT.with(|l| u.get().saturating_add(bytes) > l.get()))
}
