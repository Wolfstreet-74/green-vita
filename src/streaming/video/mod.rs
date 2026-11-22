mod decoder;
mod memory;
pub(crate) mod metrics;
mod worker;

pub const STREAM_WIDTH: u32 = 1280;
pub const STREAM_HEIGHT: u32 = 720;
pub const HW_OUTPUT_WIDTH: u32 = 960;
pub const HW_OUTPUT_HEIGHT: u32 = 544;

/// Reference frames used by default. Keeping this at 1 saves decoder memory and time, and works
/// for streams whose encoder really references only the previous frame (idea from MattKC's
/// Vanilla project). Streams that need more are detected and the decoder is rebuilt.
pub const DEFAULT_REF_FRAMES: u32 = 1;
/// Largest picture the Vita AVC hardware decoder accepts.
pub const MAX_DECODE_WIDTH: u32 = 1920;
pub const MAX_DECODE_HEIGHT: u32 = 1088;
/// Cap for reference frames so a 1080p decoder still fits in the reserved CDRAM.
pub const MAX_REF_FRAMES: u32 = 6;

/// "Compatible video mode" from the settings: start with a 1920x1088 decoder and as many
/// reference frames as the stream declares instead of the lean 1280x720 / 1-ref setup.
static VIDEO_COMPAT_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_video_compat_mode(enabled: bool) {
    VIDEO_COMPAT_MODE.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn video_compat_mode() -> bool {
    VIDEO_COMPAT_MODE.load(std::sync::atomic::Ordering::Relaxed)
}

pub use memory::{free_memory_summary, reserve_decoder_cdram};
pub use metrics::video_performance_summary;
pub(crate) use worker::NO_PICTURE_ERROR;
pub use worker::VideoDecodeWorker;

use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

// Let a short render hitch absorb at most two 30 fps intervals. Together with the
// frame already pending for presentation, this caps the microbuffer at three frames.
const MAX_PENDING_TEXTURE_WAIT: Duration = Duration::from_millis(67);

#[derive(Clone, Copy)]
pub(crate) struct VideoTextureTarget {
    pub(crate) ptr: usize,
    pub(crate) pitch: u32,
    pub(crate) capacity: u32,
}

struct DirectVideoOutputState {
    targets: Option<[VideoTextureTarget; 2]>,
    displayed: Option<usize>,
    pending: Option<(usize, u64)>,
    next_generation: u64,
}

/// Synchronizes the decoder thread with the two SDL/GXM textures owned by the render thread.
/// Pointers are stored as integers so the platform-specific unsafe boundary stays in the code
/// that registers and consumes the textures.
pub(crate) struct DirectVideoOutput {
    state: Mutex<DirectVideoOutputState>,
    frame_displayed: Condvar,
    pub(crate) decoder_ready: AtomicBool,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl DirectVideoOutput {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            state: Mutex::new(DirectVideoOutputState {
                targets: None,
                displayed: None,
                pending: None,
                next_generation: 0,
            }),
            frame_displayed: Condvar::new(),
            decoder_ready: AtomicBool::new(false),
            width,
            height,
        }
    }

    pub(crate) fn set_targets(&self, targets: [VideoTextureTarget; 2]) {
        if let Ok(mut state) = self.state.lock() {
            state.targets = Some(targets);
            state.displayed = None;
            state.pending = None;
        }
    }

    pub(crate) fn clear_targets(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.targets = None;
            state.displayed = None;
            state.pending = None;
        }
        self.frame_displayed.notify_all();
    }

    pub(crate) fn mark_displayed(&self, index: usize, generation: u64) {
        let mut cleared_pending = false;
        if let Ok(mut state) = self.state.lock() {
            state.displayed = Some(index);
            if state.pending == Some((index, generation)) {
                state.pending = None;
                cleared_pending = true;
            }
        }
        if cleared_pending {
            self.frame_displayed.notify_one();
        }
    }

    pub(super) fn lock_decode_target(&self) -> Option<DirectVideoTargetGuard<'_>> {
        let mut state = self.state.lock().ok()?;
        if state.pending.is_some() {
            let (waited_state, _) = self
                .frame_displayed
                .wait_timeout_while(state, MAX_PENDING_TEXTURE_WAIT, |state| {
                    state.targets.is_some() && state.pending.is_some()
                })
                .ok()?;
            state = waited_state;
        }
        let targets = state.targets?;
        let index = state
            .pending
            .map(|(index, _)| index)
            .unwrap_or_else(|| state.displayed.map_or(0, |displayed| 1 - displayed));
        Some(DirectVideoTargetGuard {
            state,
            target: targets[index],
            index,
        })
    }
}

pub(super) struct DirectVideoTargetGuard<'a> {
    state: MutexGuard<'a, DirectVideoOutputState>,
    target: VideoTextureTarget,
    index: usize,
}

impl DirectVideoTargetGuard<'_> {
    pub(super) fn publish(mut self) -> (usize, u64) {
        self.state.next_generation = self.state.next_generation.wrapping_add(1);
        let generation = self.state.next_generation;
        self.state.pending = Some((self.index, generation));
        (self.index, generation)
    }
}

pub struct DecodedFrame {
    pub texture_index: usize,
    pub generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecoderConfig {
    pub decode_width: u32,
    pub decode_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub ref_frames: u32,
}

impl DecoderConfig {
    /// The decoder the stream starts with, depending on the compatible video mode setting.
    pub fn initial(output_width: u32, output_height: u32) -> Self {
        if video_compat_mode() {
            Self {
                decode_width: MAX_DECODE_WIDTH,
                decode_height: MAX_DECODE_HEIGHT,
                output_width,
                output_height,
                ref_frames: 4,
            }
        } else {
            Self {
                decode_width: STREAM_WIDTH,
                decode_height: STREAM_HEIGHT,
                output_width,
                output_height,
                ref_frames: DEFAULT_REF_FRAMES,
            }
        }
    }
}
