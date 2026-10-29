//! Process-wide counters for the JSON bodies the S3 stores decode through
//! `parse_json_blob` and `find_outcomes` (not plan manifests or `todo/` jobs);
//! `queue-maintenance` logs them per phase.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The counters for every S3 store in this process.
pub static S3_DECODE: DecodeStats = DecodeStats::new();

/// Counters for a set of decoded bodies. Each counter only goes up.
#[derive(Debug)]
pub struct DecodeStats {
    bodies: AtomicU64,
    stored_bytes: AtomicU64,
    decoded_bytes: AtomicU64,
    nanos: AtomicU64,
}

/// The values of the counters at a checkpoint, or the difference between two
/// checkpoints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeCounts {
    /// The bodies that the S3 backend decoded.
    pub bodies: u64,
    /// The bytes of the bodies as the store holds them, gzipped or plain.
    pub stored_bytes: u64,
    /// The bytes of JSON that the parser read. For a plain body, this is the
    /// same as its stored bytes.
    pub decoded_bytes: u64,
    /// The time to decompress and parse the bodies, in one pass; for a
    /// `find_outcomes` record, also the wait for its bytes.
    pub time: Duration,
}

impl DecodeStats {
    pub const fn new() -> Self {
        Self {
            bodies: AtomicU64::new(0),
            stored_bytes: AtomicU64::new(0),
            decoded_bytes: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        }
    }

    pub fn record(&self, stored_bytes: usize, decoded_bytes: u64, time: Duration) {
        self.bodies.fetch_add(1, Ordering::Relaxed);
        self.stored_bytes.fetch_add(
            u64::try_from(stored_bytes).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.decoded_bytes
            .fetch_add(decoded_bytes, Ordering::Relaxed);
        self.nanos.fetch_add(
            u64::try_from(time.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub fn checkpoint(&self) -> DecodeCounts {
        DecodeCounts {
            bodies: self.bodies.load(Ordering::Relaxed),
            stored_bytes: self.stored_bytes.load(Ordering::Relaxed),
            decoded_bytes: self.decoded_bytes.load(Ordering::Relaxed),
            time: Duration::from_nanos(self.nanos.load(Ordering::Relaxed)),
        }
    }
}

impl Default for DecodeStats {
    fn default() -> Self {
        Self::new()
    }
}

impl DecodeCounts {
    /// The counts between `earlier` and this checkpoint.
    pub fn since(self, earlier: DecodeCounts) -> DecodeCounts {
        DecodeCounts {
            bodies: self.bodies.saturating_sub(earlier.bodies),
            stored_bytes: self.stored_bytes.saturating_sub(earlier.stored_bytes),
            decoded_bytes: self.decoded_bytes.saturating_sub(earlier.decoded_bytes),
            time: self.time.saturating_sub(earlier.time),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_gives_the_bodies_between_two_checkpoints() {
        let stats = DecodeStats::new();
        stats.record(10, 40, Duration::from_millis(3));
        let start = stats.checkpoint();
        stats.record(20, 80, Duration::from_millis(5));
        stats.record(7, 7, Duration::from_millis(1));
        assert_eq!(
            stats.checkpoint().since(start),
            DecodeCounts {
                bodies: 2,
                stored_bytes: 27,
                decoded_bytes: 87,
                time: Duration::from_millis(6),
            }
        );
    }
}
