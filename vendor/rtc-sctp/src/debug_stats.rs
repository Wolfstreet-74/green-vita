//! GreenVita addition: live view of the SCTP association for the on-screen debug line.
//!
//! The data channels (controller inputs among them) run over this association; when inputs
//! arrive late, these values tell whether they wait here (pending), are not acknowledged
//! (in flight, round trip), or got stuck behind retransmission timeouts.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

pub static SRTT_MS: AtomicU32 = AtomicU32::new(0);
pub static RTO_MS: AtomicU32 = AtomicU32::new(0);
pub static CWND: AtomicU32 = AtomicU32::new(0);
pub static INFLIGHT_BYTES: AtomicU32 = AtomicU32::new(0);
pub static PENDING_BYTES: AtomicU32 = AtomicU32::new(0);
/// Highest pending bytes since the last [`take_pending_peak`].
pub static PENDING_PEAK: AtomicU32 = AtomicU32::new(0);
/// Highest in-flight bytes since the last [`take_inflight_peak`].
pub static INFLIGHT_PEAK: AtomicU32 = AtomicU32::new(0);
pub static T3_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
pub static FAST_RETRANS: AtomicU64 = AtomicU64::new(0);
pub static FORWARD_TSN_SENT: AtomicU64 = AtomicU64::new(0);
pub static STALE_DROPPED: AtomicU64 = AtomicU64::new(0);
/// 0 = not negotiated yet, 1 = peer supports FORWARD-TSN, 2 = it does not.
pub static FORWARD_TSN: AtomicU8 = AtomicU8::new(0);

pub fn take_pending_peak() -> u32 {
    PENDING_PEAK.swap(0, Ordering::Relaxed)
}

pub fn take_inflight_peak() -> u32 {
    INFLIGHT_PEAK.swap(0, Ordering::Relaxed)
}

pub(crate) fn publish(srtt: u64, rto: u64, cwnd: u32, inflight: usize, pending: usize) {
    SRTT_MS.store(srtt.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    RTO_MS.store(rto.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    CWND.store(cwnd, Ordering::Relaxed);
    let inflight = inflight.min(u32::MAX as usize) as u32;
    let pending = pending.min(u32::MAX as usize) as u32;
    INFLIGHT_BYTES.store(inflight, Ordering::Relaxed);
    PENDING_BYTES.store(pending, Ordering::Relaxed);
    PENDING_PEAK.fetch_max(pending, Ordering::Relaxed);
    INFLIGHT_PEAK.fetch_max(inflight, Ordering::Relaxed);
}
