//! Delay-based receive-side bandwidth estimate, sent to the server as REMB.
//!
//! The video track negotiates `goog-remb`, but nothing used to send one, so the server only
//! learnt about congestion from packet loss in receiver reports. On 4G the network buffers
//! fill for many seconds before anything is lost: the picture (and with it the effect of every
//! button) ended up 10-15 s behind. Here the Vita measures how much the video is queued in the
//! network (arrival time against the RTP timestamp) and asks for a lower bitrate as soon as
//! that queue grows, before it reaches seconds.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const VIDEO_CLOCK_KHZ: f64 = 90.0;
/// How often the estimate is updated and sent.
pub(crate) const REMB_INTERVAL: Duration = Duration::from_millis(250);
/// Width of one bucket of the base-delay minimum, and how many are kept (clock drift).
const BASE_BUCKET: Duration = Duration::from_secs(1);
const BASE_BUCKETS: usize = 30;
/// Queuing delay floor above which the link is considered overused.
const OVERUSE_MS: f64 = 100.0;
/// Below this the link is considered clear and the estimate may grow.
const CLEAR_MS: f64 = 40.0;
const MIN_ESTIMATE_BPS: f64 = 500_000.0;
const START_ESTIMATE_BPS: f64 = 20_000_000.0;
/// Growth per interval while clear (about +8 %/s).
const INCREASE_FACTOR: f64 = 1.02;
/// Bitrate asked for after overuse, relative to what actually arrives.
const DECREASE_FACTOR: f64 = 0.85;
/// No growth for this long after a cut, so the queue can drain.
const HOLD_AFTER_DECREASE: Duration = Duration::from_millis(1000);
/// Data needed before any REMB goes out.
const WARMUP: Duration = Duration::from_secs(2);

pub(crate) struct BandwidthEstimator {
    start: Instant,
    last_ts: Option<u32>,
    /// RTP timestamp unwrapped, in 90 kHz ticks from the first packet.
    unwrapped_ts: i64,
    /// (bucket index, smallest relative delay in it).
    base_buckets: VecDeque<(u64, f64)>,
    interval_started: Instant,
    interval_bytes: usize,
    interval_min_rel: f64,
    interval_max_rel: f64,
    incoming_bps: Option<f64>,
    estimate_bps: f64,
    last_decrease: Option<Instant>,
    /// Latest queuing delay floor and peak (ms), for the debug line.
    pub(crate) queue_delay_ms: f64,
    pub(crate) queue_delay_peak_ms: f64,
    pub(crate) overuse_events: u64,
}

impl BandwidthEstimator {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            start: now,
            last_ts: None,
            unwrapped_ts: 0,
            base_buckets: VecDeque::with_capacity(BASE_BUCKETS + 1),
            interval_started: now,
            interval_bytes: 0,
            interval_min_rel: f64::INFINITY,
            interval_max_rel: f64::NEG_INFINITY,
            incoming_bps: None,
            estimate_bps: START_ESTIMATE_BPS,
            last_decrease: None,
            queue_delay_ms: 0.0,
            queue_delay_peak_ms: 0.0,
            overuse_events: 0,
        }
    }

    pub(crate) fn on_packet(&mut self, now: Instant, rtp_timestamp: u32, bytes: usize) {
        match self.last_ts {
            None => self.unwrapped_ts = 0,
            Some(last) => self.unwrapped_ts += i64::from(rtp_timestamp.wrapping_sub(last) as i32),
        }
        self.last_ts = Some(rtp_timestamp);

        let arrival_ms = now.duration_since(self.start).as_secs_f64() * 1000.0;
        let rel = arrival_ms - self.unwrapped_ts as f64 / VIDEO_CLOCK_KHZ;
        self.interval_bytes += bytes;
        self.interval_min_rel = self.interval_min_rel.min(rel);
        self.interval_max_rel = self.interval_max_rel.max(rel);

        let bucket = (now.duration_since(self.start).as_millis() / BASE_BUCKET.as_millis()) as u64;
        match self.base_buckets.back_mut() {
            Some((index, min)) if *index == bucket => *min = min.min(rel),
            _ => {
                self.base_buckets.push_back((bucket, rel));
                while self.base_buckets.len() > BASE_BUCKETS {
                    self.base_buckets.pop_front();
                }
            }
        }
    }

    /// Updates the estimate once per [`REMB_INTERVAL`]; returns the bitrate (bit/s) to send.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<f64> {
        let elapsed = now.duration_since(self.interval_started);
        if elapsed < REMB_INTERVAL {
            return None;
        }
        let bytes = std::mem::take(&mut self.interval_bytes);
        let min_rel = std::mem::replace(&mut self.interval_min_rel, f64::INFINITY);
        let max_rel = std::mem::replace(&mut self.interval_max_rel, f64::NEG_INFINITY);
        self.interval_started = now;

        if !min_rel.is_finite() {
            // Nothing arrived: a stalled link is the worst kind of queue, but without data
            // there is no rate to base a cut on. Keep the current estimate.
            return (now.duration_since(self.start) >= WARMUP).then_some(self.estimate_bps);
        }

        let rate = bytes as f64 * 8.0 / elapsed.as_secs_f64();
        let incoming = match self.incoming_bps {
            Some(previous) => previous * 0.7 + rate * 0.3,
            None => rate,
        };
        self.incoming_bps = Some(incoming);

        let base = self
            .base_buckets
            .iter()
            .map(|(_, min)| *min)
            .fold(f64::INFINITY, f64::min);
        self.queue_delay_ms = (min_rel - base).max(0.0);
        self.queue_delay_peak_ms = (max_rel - base).max(0.0);

        if now.duration_since(self.start) < WARMUP {
            return None;
        }

        if self.queue_delay_ms > OVERUSE_MS {
            let target = (incoming * DECREASE_FACTOR).max(MIN_ESTIMATE_BPS);
            let recently_cut = self
                .last_decrease
                .is_some_and(|at| now.duration_since(at) < HOLD_AFTER_DECREASE);
            if target < self.estimate_bps && !recently_cut {
                self.estimate_bps = target;
                self.last_decrease = Some(now);
                self.overuse_events += 1;
            }
        } else if self.queue_delay_ms < CLEAR_MS {
            let holding = self
                .last_decrease
                .is_some_and(|at| now.duration_since(at) < HOLD_AFTER_DECREASE);
            if !holding {
                // Do not run far ahead of what the server actually sends.
                let ceiling = (incoming * 1.5).max(incoming + 1_000_000.0);
                self.estimate_bps =
                    (self.estimate_bps * INCREASE_FACTOR).min(ceiling.max(self.estimate_bps));
            }
        }
        Some(self.estimate_bps)
    }

    pub(crate) fn incoming_kbps(&self) -> u64 {
        self.incoming_bps.map_or(0, |bps| (bps / 1000.0) as u64)
    }

    pub(crate) fn estimate_kbps(&self) -> u64 {
        (self.estimate_bps / 1000.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(
        est: &mut BandwidthEstimator,
        start: Instant,
        from_ms: u64,
        to_ms: u64,
        delay: impl Fn(u64) -> u64,
    ) -> Vec<f64> {
        let mut sent = Vec::new();
        for frame_ms in (from_ms..to_ms).step_by(16) {
            let arrival = start + Duration::from_millis(frame_ms + delay(frame_ms));
            let ts = (frame_ms * 90) as u32;
            for _ in 0..8 {
                est.on_packet(arrival, ts, 1200);
            }
            if let Some(bps) = est.poll(arrival) {
                sent.push(bps);
            }
        }
        sent
    }

    #[test]
    fn growing_queue_cuts_the_estimate() {
        let start = Instant::now();
        let mut est = BandwidthEstimator::new(start);
        let clear = feed(&mut est, start, 0, 5000, |_| 20);
        assert!(!clear.is_empty());
        assert!(est.queue_delay_ms < CLEAR_MS);
        // Queue builds up by 1 ms every 16 ms of video.
        feed(&mut est, start, 5000, 9000, |t| 20 + (t - 5000) / 16);
        assert!(est.overuse_events > 0);
        assert!(est.estimate_bps < 4_500_000.0, "{}", est.estimate_bps);
    }

    #[test]
    fn rtp_timestamp_wraps() {
        let start = Instant::now();
        let mut est = BandwidthEstimator::new(start);
        est.on_packet(start, u32::MAX - 899, 100);
        est.on_packet(start + Duration::from_millis(20), 600, 100);
        assert_eq!(est.unwrapped_ts, 1500);
    }
}
