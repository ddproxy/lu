use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

/// Which pipe a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One read from stdout or stderr, raw and undecoded, at most 64 KiB.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    /// The pipe this chunk was read from.
    pub stream: Stream,
    /// The bytes exactly as read, never decoded or trimmed.
    pub bytes: Vec<u8>,
}

/// Largest chunk the supervisor emits.
pub(crate) const MAX_CHUNK: usize = 64 * 1024;

/// Shared progress of the two pipe readers: the time of the last output, the
/// running byte total and whether the consumer has gone away.
pub(crate) struct OutputState {
    last_output: Mutex<Instant>,
    total: AtomicU64,
    pub(crate) limit: u64,
    pub(crate) discarding: AtomicBool,
}

impl OutputState {
    pub(crate) fn new(limit: u64) -> Self {
        Self {
            last_output: Mutex::new(Instant::now()),
            total: AtomicU64::new(0),
            limit,
            discarding: AtomicBool::new(false),
        }
    }

    /// Records `len` freshly read bytes and returns the new running total.
    pub(crate) fn record(&self, len: usize) -> u64 {
        let mut last = self
            .last_output
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *last = Instant::now();
        drop(last);
        let len = len as u64;
        self.total.fetch_add(len, Ordering::Relaxed) + len
    }

    /// The instant the most recent output arrived.
    pub(crate) fn last_output(&self) -> Instant {
        *self
            .last_output
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}
