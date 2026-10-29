//! Process-wide counters for the HTTP attempts of both S3 clients
//! (`object_store` and the `todo/` queue's `aws-sdk-s3`); `queue-maintenance`
//! logs them per phase.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The counters for every S3 store in this process.
pub static S3_HTTP: HttpStats = HttpStats::new();

/// Counters that only go up, except the requests in flight and their peak.
#[derive(Debug)]
pub struct HttpStats {
    requests: AtomicU64,
    ok: AtomicU64,
    not_found: AtomicU64,
    throttled: AtomicU64,
    client_errors: AtomicU64,
    server_errors: AtomicU64,
    transport_errors: AtomicU64,
    in_flight: AtomicU64,
    /// The most requests in flight at one time since the last
    /// [`checkpoint`](HttpStats::checkpoint).
    peak_in_flight: AtomicU64,
    wait_micros: AtomicU64,
}

/// How an HTTP attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpOutcome {
    /// The server sent a response with this status code.
    Status(u16),
    /// The request got no response: the connection failed, or it timed out.
    TransportError,
}

/// The values of the counters at a checkpoint, or the difference between two
/// checkpoints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HttpCounts {
    /// The HTTP attempts. Each retry is an attempt.
    pub requests: u64,
    /// The responses with a 1xx, 2xx, or 3xx status.
    pub ok: u64,
    /// The responses with a 404 status. A store probe that finds no object
    /// gets one.
    pub not_found: u64,
    /// The responses with a 429 or 503 status. S3 sends 503 `SlowDown` to
    /// throttle a client.
    pub throttled: u64,
    /// The responses with any other 4xx status.
    pub client_errors: u64,
    /// The responses with any other 5xx status.
    pub server_errors: u64,
    /// The attempts that got no response.
    pub transport_errors: u64,
    /// The most requests in flight at one time in the window that ends at this
    /// checkpoint.
    pub peak_in_flight: u64,
    /// The sum of the time from each request to its response headers. The time
    /// to read a response body is not in it.
    pub wait: Duration,
}

impl HttpStats {
    pub const fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
            ok: AtomicU64::new(0),
            not_found: AtomicU64::new(0),
            throttled: AtomicU64::new(0),
            client_errors: AtomicU64::new(0),
            server_errors: AtomicU64::new(0),
            transport_errors: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            peak_in_flight: AtomicU64::new(0),
            wait_micros: AtomicU64::new(0),
        }
    }

    /// Count the start of an HTTP attempt. Give the result to
    /// [`InFlight::finish`] when the response headers arrive.
    pub fn start(&self) -> InFlight<'_> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_in_flight.fetch_max(in_flight, Ordering::Relaxed);
        InFlight {
            stats: self,
            started: Instant::now(),
        }
    }

    /// Read the counters and start a new peak window at the requests in flight.
    pub fn checkpoint(&self) -> HttpCounts {
        // Clear the peak before reading `in_flight`: an attempt that starts in
        // between raises the new peak itself.
        let peak_in_flight = self.peak_in_flight.swap(0, Ordering::Relaxed);
        self.peak_in_flight
            .fetch_max(self.in_flight.load(Ordering::Relaxed), Ordering::Relaxed);
        HttpCounts {
            requests: self.requests.load(Ordering::Relaxed),
            ok: self.ok.load(Ordering::Relaxed),
            not_found: self.not_found.load(Ordering::Relaxed),
            throttled: self.throttled.load(Ordering::Relaxed),
            client_errors: self.client_errors.load(Ordering::Relaxed),
            server_errors: self.server_errors.load(Ordering::Relaxed),
            transport_errors: self.transport_errors.load(Ordering::Relaxed),
            peak_in_flight,
            wait: Duration::from_micros(self.wait_micros.load(Ordering::Relaxed)),
        }
    }
}

impl Default for HttpStats {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpCounts {
    /// The counts between `earlier` and this checkpoint, with this checkpoint's
    /// peak, since each checkpoint starts a new window.
    pub fn since(self, earlier: HttpCounts) -> HttpCounts {
        HttpCounts {
            requests: self.requests.saturating_sub(earlier.requests),
            ok: self.ok.saturating_sub(earlier.ok),
            not_found: self.not_found.saturating_sub(earlier.not_found),
            throttled: self.throttled.saturating_sub(earlier.throttled),
            client_errors: self.client_errors.saturating_sub(earlier.client_errors),
            server_errors: self.server_errors.saturating_sub(earlier.server_errors),
            transport_errors: self
                .transport_errors
                .saturating_sub(earlier.transport_errors),
            peak_in_flight: self.peak_in_flight,
            wait: self.wait.saturating_sub(earlier.wait),
        }
    }
}

/// An HTTP attempt in flight. Dropping it, finished or not, takes it out of the
/// in-flight count; without [`finish`](InFlight::finish) it records no outcome.
#[derive(Debug)]
#[must_use = "an attempt counts no outcome until it finishes"]
pub struct InFlight<'a> {
    stats: &'a HttpStats,
    started: Instant,
}

impl InFlight<'_> {
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Count how the attempt ended, and the time until it ended.
    pub fn finish(self, outcome: HttpOutcome) {
        let counter = match outcome {
            HttpOutcome::Status(100..=399) => &self.stats.ok,
            HttpOutcome::Status(404) => &self.stats.not_found,
            HttpOutcome::Status(429 | 503) => &self.stats.throttled,
            HttpOutcome::Status(400..=499) => &self.stats.client_errors,
            HttpOutcome::Status(_) => &self.stats.server_errors,
            HttpOutcome::TransportError => &self.stats.transport_errors,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.stats.wait_micros.fetch_add(micros, Ordering::Relaxed);
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_outcome_counts_in_its_class() {
        let stats = HttpStats::new();
        [
            HttpOutcome::Status(200),
            HttpOutcome::Status(304),
            HttpOutcome::Status(404),
            HttpOutcome::Status(429),
            HttpOutcome::Status(503),
            HttpOutcome::Status(403),
            HttpOutcome::Status(500),
            HttpOutcome::TransportError,
        ]
        .into_iter()
        .for_each(|outcome| stats.start().finish(outcome));
        let counts = stats.checkpoint();
        assert_eq!(
            HttpCounts {
                wait: Duration::ZERO,
                ..counts
            },
            HttpCounts {
                requests: 8,
                ok: 2,
                not_found: 1,
                throttled: 2,
                client_errors: 1,
                server_errors: 1,
                transport_errors: 1,
                peak_in_flight: 1,
                wait: Duration::ZERO,
            }
        );
    }

    /// A new window starts at the requests still in flight.
    #[test]
    fn a_checkpoint_starts_a_new_peak_window() {
        let stats = HttpStats::new();
        let first = stats.start();
        let second = stats.start();
        let third = stats.start();
        drop(third);
        second.finish(HttpOutcome::Status(200));
        let start = stats.checkpoint();
        assert_eq!(start.peak_in_flight, 3);

        let fourth = stats.start();
        first.finish(HttpOutcome::Status(200));
        fourth.finish(HttpOutcome::Status(200));
        let end = stats.checkpoint();
        let phase = end.since(start);
        assert_eq!(phase.peak_in_flight, 2, "`first` was still in flight");
        assert_eq!(phase.requests, 1);
        assert_eq!(phase.ok, 2);
        assert_eq!(stats.checkpoint().peak_in_flight, 0, "nothing is in flight");
    }
}
