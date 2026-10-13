//! How far the indexing under way has got, for whoever shows it: files
//! parsed out of how many, then linking. One process indexes one tree at a
//! time, so this is kept for the process.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering::Relaxed};

static PHASE: AtomicU8 = AtomicU8::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Parsing,
    Linking,
}

/// `total` files are about to be parsed.
pub fn parsing(total: usize) {
    DONE.store(0, Relaxed);
    TOTAL.store(total, Relaxed);
    PHASE.store(1, Relaxed);
}

/// One more file parsed.
pub fn tick() {
    DONE.fetch_add(1, Relaxed);
}

pub fn linking() {
    PHASE.store(2, Relaxed);
}

pub fn idle() {
    PHASE.store(0, Relaxed);
}

/// The phase, and files parsed out of how many.
pub fn now() -> (Phase, usize, usize) {
    let phase = match PHASE.load(Relaxed) {
        1 => Phase::Parsing,
        2 => Phase::Linking,
        _ => Phase::Idle,
    };
    (phase, DONE.load(Relaxed), TOTAL.load(Relaxed))
}
