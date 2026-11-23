use crate::diag_log;
use crate::streaming::video::{
    DecoderConfig, MAX_DECODE_HEIGHT, MAX_DECODE_WIDTH, MAX_REF_FRAMES, VideoDecodeWorker,
    video_compat_mode,
};
use bytes::Bytes;
use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::sps::SeqParameterSet;
use h264_reader::nal::{Nal, RefNal, UnitType};
use h264_reader::push::NalInterest;
use rtc::rtp::Packet;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::codec::opus::OpusPacket;
use rtc::rtp::packetizer::Depacketizer;
use rtc_media::io::sample_builder::SampleBuilder;
use std::sync::atomic::Ordering;
use std::time::Instant;

const MAX_PENDING_AUDIO_PACKETS: usize = 32;
const MAX_H264_ACCESS_UNIT_BYTES: usize = 2 * 1024 * 1024;
const VIDEO_RTP_CLOCK_RATE: u32 = 90_000;
const AUDIO_MAX_LATE_PACKETS: u16 = 32;
const LOW_FPS_DAMAGE_LIMIT: u8 = 8;
const HIGH_FPS_DAMAGE_LIMIT: u8 = 3;
/// How long an incomplete frame may wait for missing (resent) packets before it is dropped.
/// Long enough for one NACK round trip on 4G, short enough to stay playable.
const MAX_FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(150);
/// Upper bound on frames held while waiting, whatever the timing.
const MAX_BUFFERED_FRAMES: usize = 6;
/// A frame that took longer than this to complete probably waited for a resent packet.
const NORMAL_FRAME_ASSEMBLY: std::time::Duration = std::time::Duration::from_millis(60);

#[derive(Default)]
pub(super) struct VideoSampleStats {
    pub dropped: u32,
    pub source_frame_duration_us: Option<u64>,
    pub encoded_resolution: Option<(u32, u32)>,
}

pub(super) struct AudioRtp {
    samples: SampleBuilder<OpusPacket>,
    payload_type: u8,
}

impl AudioRtp {
    pub(super) fn new(sample_rate: u32, payload_type: u8) -> Self {
        Self {
            samples: SampleBuilder::new(AUDIO_MAX_LATE_PACKETS, OpusPacket, sample_rate)
                .with_max_time_delay(std::time::Duration::from_millis(80)),
            payload_type,
        }
    }

    pub(super) fn receive(&mut self, packet: Packet, audio_packets: &mut Vec<Bytes>) {
        if packet.header.payload_type != self.payload_type {
            return;
        }

        self.samples.push(packet);
        while audio_packets.len() < MAX_PENDING_AUDIO_PACKETS {
            let Some(sample) = self.samples.pop() else {
                break;
            };
            audio_packets.push(sample.data);
        }
    }
}

pub(super) struct VideoRtp {
    depacketizer: H264Packet,
    pending: std::collections::VecDeque<PendingVideoFrame>,
    /// Packets that arrived after their frame was already decoded or dropped.
    late_packets: u64,
    /// Sequence numbers of recent empty padding packets.
    padding_sequences: std::collections::VecDeque<u16>,
    /// Frames completed only after waiting (usually thanks to a resent packet).
    recovered_frames: u64,
    next_sequence: Option<u16>,
    last_frame_timestamp: Option<u32>,
    source_frame_duration_us: Option<u64>,
    damage_score: u8,
    stream_too_large: bool,
    waiting_for_keyframe: bool,
    /// Setup of the hardware decoder currently used by the worker.
    decoder_config: DecoderConfig,
    /// Last SPS seen, used for logging changes and sizing the decoder.
    last_sps: Option<SpsSummary>,
    /// Decoder failures in a row without a decoded picture in between.
    failures_without_picture: u32,
    access_units: u64,
    /// Latest SPS and PPS NAL units (without start code). A rebuilt hardware decoder forgets
    /// them, and xCloud keyframes sent after a loss do not always repeat them, so they are put
    /// back in front of any keyframe that lacks them.
    cached_sps_nal: Option<Vec<u8>>,
    cached_pps_nal: Option<Vec<u8>>,
    parameter_sets_reinjected: u64,
    /// Highest RTP sequence number seen, to count gaps (packets lost before any resend).
    highest_sequence: Option<u16>,
    lost_packets: u64,
    /// Times the picture was frozen on the last clean frame to wait for a keyframe.
    freezes: u64,
}

/// The parts of an H.264 SPS that matter for the Vita hardware decoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct SpsSummary {
    pub width: u32,
    pub height: u32,
    /// Size in whole macroblocks, as the decoder allocates it (before cropping).
    pub coded_width: u32,
    pub coded_height: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub ref_frames: u32,
    pub reorder_frames: Option<u32>,
    pub frame_mbs_only: bool,
}

impl std::fmt::Display for SpsSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let profile = match self.profile_idc {
            66 => "Baseline",
            77 => "Main",
            88 => "Extended",
            100 => "High",
            110 => "High10",
            122 => "High422",
            244 => "High444",
            _ => "other",
        };
        write!(
            f,
            "{}x{} (coded {}x{}) profile={}({}) level={}.{} ref_frames={} reorder={} progressive={}",
            self.width,
            self.height,
            self.coded_width,
            self.coded_height,
            profile,
            self.profile_idc,
            self.level_idc / 10,
            self.level_idc % 10,
            self.ref_frames,
            self.reorder_frames
                .map_or_else(|| "?".to_owned(), |value| value.to_string()),
            self.frame_mbs_only,
        )
    }
}

struct PendingVideoFrame {
    timestamp: u32,
    first_packet_at: Instant,
    packets: Vec<Packet>,
}

enum FrameAssembly {
    Pending,
    Complete { data: Bytes, marker_sequence: u16 },
    Invalid,
}

impl PendingVideoFrame {
    fn new(packet: Packet) -> Self {
        Self {
            timestamp: packet.header.timestamp,
            first_packet_at: Instant::now(),
            packets: vec![packet],
        }
    }

    fn insert(&mut self, packet: Packet) {
        if !self
            .packets
            .iter()
            .any(|existing| existing.header.sequence_number == packet.header.sequence_number)
        {
            self.packets.push(packet);
        }
    }

    fn marker_sequence(&self) -> Option<u16> {
        self.packets
            .iter()
            .find(|packet| packet.header.marker)
            .map(|packet| packet.header.sequence_number)
    }

    fn assemble(
        &self,
        depacketizer: &mut H264Packet,
        expected_sequence: Option<u16>,
    ) -> FrameAssembly {
        let Some(marker_sequence) = self.marker_sequence() else {
            return FrameAssembly::Pending;
        };
        let mut packets = self.packets.iter().collect::<Vec<_>>();
        packets.sort_unstable_by_key(|packet| {
            std::cmp::Reverse(marker_sequence.wrapping_sub(packet.header.sequence_number))
        });
        let Some(first) = packets.first() else {
            return FrameAssembly::Pending;
        };
        if expected_sequence.is_some_and(|expected| first.header.sequence_number != expected)
            || !depacketizer.is_partition_head(&first.payload)
        {
            return FrameAssembly::Pending;
        }
        if packets.windows(2).any(|pair| {
            pair[1].header.sequence_number != pair[0].header.sequence_number.wrapping_add(1)
        }) {
            return FrameAssembly::Pending;
        }

        *depacketizer = H264Packet::default();
        let mut data = Vec::new();
        for packet in packets {
            let Ok(nalu) = depacketizer.depacketize(&packet.payload) else {
                *depacketizer = H264Packet::default();
                return FrameAssembly::Invalid;
            };
            data.extend_from_slice(&nalu);
            if data.len() > MAX_H264_ACCESS_UNIT_BYTES {
                *depacketizer = H264Packet::default();
                return FrameAssembly::Invalid;
            }
        }
        *depacketizer = H264Packet::default();
        FrameAssembly::Complete {
            data: Bytes::from(data),
            marker_sequence,
        }
    }
}

impl VideoRtp {
    pub(super) fn new(decoder_config: DecoderConfig) -> Self {
        diag_log::log(
            "video",
            format!(
                "Stream start: decoder {}x{} ref_frames={} compat_mode={}",
                decoder_config.decode_width,
                decoder_config.decode_height,
                decoder_config.ref_frames,
                video_compat_mode()
            ),
        );
        Self {
            depacketizer: H264Packet::default(),
            pending: std::collections::VecDeque::new(),
            late_packets: 0,
            padding_sequences: std::collections::VecDeque::new(),
            recovered_frames: 0,
            next_sequence: None,
            last_frame_timestamp: None,
            source_frame_duration_us: None,
            damage_score: 0,
            stream_too_large: false,
            waiting_for_keyframe: false,
            decoder_config,
            last_sps: None,
            failures_without_picture: 0,
            access_units: 0,
            cached_sps_nal: None,
            cached_pps_nal: None,
            parameter_sets_reinjected: 0,
            highest_sequence: None,
            lost_packets: 0,
            freezes: 0,
        }
    }

    pub(super) fn decoder_config(&self) -> DecoderConfig {
        self.decoder_config
    }

    /// `(late packets, frames completed after waiting)` since the stream started.
    pub(super) fn recovery_counters(&self) -> (u64, u64) {
        (self.late_packets, self.recovered_frames)
    }

    /// `(packets missing on arrival, picture freezes)` since the stream started.
    pub(super) fn loss_counters(&self) -> (u64, u64) {
        (self.lost_packets, self.freezes)
    }

    fn count_sequence_gap(&mut self, sequence: u16) {
        let Some(highest) = self.highest_sequence else {
            self.highest_sequence = Some(sequence);
            return;
        };
        let ahead = sequence.wrapping_sub(highest);
        // Newer packet (allowing wrap-around); older ones are resends or reordered packets.
        if ahead != 0 && ahead < 0x8000 {
            if ahead < 1000 {
                self.lost_packets = self.lost_packets.saturating_add(u64::from(ahead - 1));
            }
            self.highest_sequence = Some(sequence);
        }
    }

    /// Called for every picture the hardware decoder outputs.
    pub(super) fn on_picture_decoded(&mut self) {
        self.failures_without_picture = 0;
    }

    /// Called when the decoder reported errors. After repeated failures, rebuild the decoder
    /// with more reference frames, which is what most "sound but no image" streams need.
    pub(super) fn on_decode_failure(&mut self, worker: &VideoDecodeWorker, error: &str) {
        self.failures_without_picture = self.failures_without_picture.saturating_add(1);
        if self.failures_without_picture <= 20 || self.failures_without_picture % 50 == 0 {
            diag_log::log(
                "video",
                format!(
                    "Decode failure #{} (decoder {}x{} ref_frames={}): {error}",
                    self.failures_without_picture,
                    self.decoder_config.decode_width,
                    self.decoder_config.decode_height,
                    self.decoder_config.ref_frames,
                ),
            );
        }
        let no_picture = error.starts_with(crate::streaming::video::NO_PICTURE_ERROR);
        if self.failures_without_picture < 2 && !no_picture {
            return;
        }

        let current = self.decoder_config;
        let wanted_refs = self
            .last_sps
            .map(|sps| sps.ref_frames.max(sps.reorder_frames.unwrap_or(0) + 1))
            .unwrap_or(4)
            .max(current.ref_frames.saturating_mul(2))
            .clamp(2, MAX_REF_FRAMES);
        if wanted_refs <= current.ref_frames {
            return;
        }
        let new_config = DecoderConfig {
            ref_frames: wanted_refs,
            ..current
        };
        diag_log::log(
            "video",
            format!(
                "Rebuilding decoder after failures: ref_frames {} -> {}",
                current.ref_frames, new_config.ref_frames
            ),
        );
        self.decoder_config = new_config;
        self.failures_without_picture = 0;
        worker.reconfigure(new_config);
    }

    /// Returns a decoder setup that can handle `sps`, or `None` if the current one already can.
    fn config_for_sps(&self, sps: &SpsSummary) -> Option<DecoderConfig> {
        let current = self.decoder_config;
        let ref_frames = if video_compat_mode() {
            current.ref_frames.max(sps.ref_frames.min(MAX_REF_FRAMES))
        } else {
            current.ref_frames
        };
        let wanted = DecoderConfig {
            decode_width: current.decode_width.max(sps.coded_width),
            decode_height: current.decode_height.max(sps.coded_height),
            ref_frames,
            ..current
        };
        (wanted != current).then_some(wanted)
    }

    pub(super) fn waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    pub(super) fn wait_for_keyframe(&mut self) {
        self.waiting_for_keyframe = true;
    }

    pub(super) fn receive(
        &mut self,
        worker: &VideoDecodeWorker,
        packet: Packet,
        keyframe_requested: &mut bool,
    ) -> VideoSampleStats {
        let mut stats = VideoSampleStats::default();
        self.count_sequence_gap(packet.header.sequence_number);
        if packet.payload.is_empty() {
            // Padding (bandwidth probing). Remember it so a frame that follows it is not
            // considered to have a missing first packet.
            if self.padding_sequences.len() >= 64 {
                self.padding_sequences.pop_front();
            }
            self.padding_sequences
                .push_back(packet.header.sequence_number);
            self.drain_pending_frames(worker, keyframe_requested, &mut stats);
            return stats;
        }

        let packet_timestamp = packet.header.timestamp;
        // A late or retransmitted packet for a frame that was already decoded or given up on.
        if self
            .last_frame_timestamp
            .is_some_and(|last| !timestamp_is_newer(packet_timestamp, last))
        {
            self.late_packets = self.late_packets.saturating_add(1);
            return stats;
        }
        if let Some(frame) = self
            .pending
            .iter_mut()
            .find(|frame| frame.timestamp == packet_timestamp)
        {
            frame.insert(packet);
        } else {
            // Keep frames ordered by timestamp; packets can arrive out of order on 4G.
            let position = self
                .pending
                .iter()
                .position(|frame| timestamp_is_newer(frame.timestamp, packet_timestamp))
                .unwrap_or(self.pending.len());
            self.pending
                .insert(position, PendingVideoFrame::new(packet));
        }

        self.drain_pending_frames(worker, keyframe_requested, &mut stats);
        stats
    }

    /// Emits frames in order. An incomplete frame is kept for up to `MAX_FRAME_WAIT` so that
    /// packets resent after a NACK can still complete it; frames behind it wait so the decoder
    /// always gets them in order.
    fn drain_pending_frames(
        &mut self,
        worker: &VideoDecodeWorker,
        keyframe_requested: &mut bool,
        stats: &mut VideoSampleStats,
    ) {
        let mut frame_was_damaged = false;
        loop {
            while let Some(expected) = self.next_sequence
                && self.padding_sequences.contains(&expected)
            {
                self.next_sequence = Some(expected.wrapping_add(1));
            }
            let Some(front) = self.pending.front() else {
                break;
            };
            match front.assemble(&mut self.depacketizer, self.next_sequence) {
                FrameAssembly::Complete {
                    data,
                    marker_sequence,
                } => {
                    let completed = self.pending.pop_front().expect("front frame exists");
                    if completed.first_packet_at.elapsed() > NORMAL_FRAME_ASSEMBLY {
                        self.recovered_frames = self.recovered_frames.saturating_add(1);
                    }
                    self.process_frame(
                        worker,
                        completed,
                        data,
                        marker_sequence,
                        frame_was_damaged,
                        keyframe_requested,
                        stats,
                    );
                    frame_was_damaged = false;
                }
                FrameAssembly::Invalid => {
                    let invalid = self.pending.pop_front().expect("front frame exists");
                    self.last_frame_timestamp = Some(invalid.timestamp);
                    self.next_sequence = None;
                    *keyframe_requested = true;
                    self.record_damage(worker);
                    frame_was_damaged = true;
                    stats.dropped = stats.dropped.saturating_add(1);
                }
                FrameAssembly::Pending => {
                    let expired =
                        self.pending.len() > 1 && front.first_packet_at.elapsed() >= MAX_FRAME_WAIT;
                    if !expired && self.pending.len() <= MAX_BUFFERED_FRAMES {
                        break;
                    }
                    let incomplete = self.pending.pop_front().expect("front frame exists");
                    self.last_frame_timestamp = Some(incomplete.timestamp);
                    self.next_sequence = incomplete
                        .marker_sequence()
                        .map(|sequence| sequence.wrapping_add(1));
                    self.depacketizer = H264Packet::default();
                    *keyframe_requested = true;
                    self.record_damage(worker);
                    frame_was_damaged = true;
                    stats.dropped = stats.dropped.saturating_add(1);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_frame(
        &mut self,
        worker: &VideoDecodeWorker,
        completed: PendingVideoFrame,
        data: Bytes,
        marker_sequence: u16,
        frame_was_damaged: bool,
        keyframe_requested: &mut bool,
        stats: &mut VideoSampleStats,
    ) {
        // Record both average and worst-case RTP assembly time for the stream HUD.
        let assembly_us = completed.first_packet_at.elapsed().as_micros() as u64;
        crate::streaming::video::metrics::METRICS
            .rtp_assembly_sum_us
            .fetch_add(assembly_us, Ordering::Relaxed);
        crate::streaming::video::metrics::METRICS
            .rtp_assembly_count
            .fetch_add(1, Ordering::Relaxed);
        crate::streaming::video::metrics::METRICS
            .rtp_assembly_max_us
            .fetch_max(assembly_us, Ordering::Relaxed);
        self.next_sequence = Some(marker_sequence.wrapping_add(1));
        stats.source_frame_duration_us = self.last_frame_timestamp.map(|previous| {
            u64::from(completed.timestamp.wrapping_sub(previous)) * 1_000_000
                / u64::from(VIDEO_RTP_CLOCK_RATE)
        });
        if let Some(duration) = stats.source_frame_duration_us {
            self.source_frame_duration_us = Some(
                self.source_frame_duration_us
                    .map(|average| (average * 7 + duration) / 8)
                    .unwrap_or(duration),
            );
        }
        self.last_frame_timestamp = Some(completed.timestamp);

        let unit = inspect_h264_access_unit(&data);
        self.access_units += 1;
        stats.encoded_resolution = unit.sps.map(|sps| (sps.width, sps.height));
        if let Some(sps) = unit.sps {
            if self.last_sps != Some(sps) {
                diag_log::log(
                    "video",
                    format!(
                        "SPS: {sps} idr={} au_bytes={} au#={}",
                        unit.has_idr,
                        data.len(),
                        self.access_units
                    ),
                );
                self.last_sps = Some(sps);
            }
            if let Some(new_config) = self.config_for_sps(&sps) {
                if new_config.decode_width <= MAX_DECODE_WIDTH
                    && new_config.decode_height <= MAX_DECODE_HEIGHT
                {
                    // Before this fix, a stream bigger than 1280x720 was dropped frame by
                    // frame: sound and controls worked but the screen stayed black.
                    diag_log::log(
                        "video",
                        format!(
                            "Rebuilding decoder for stream: {}x{} ref_frames={} -> {}x{} ref_frames={}",
                            self.decoder_config.decode_width,
                            self.decoder_config.decode_height,
                            self.decoder_config.ref_frames,
                            new_config.decode_width,
                            new_config.decode_height,
                            new_config.ref_frames
                        ),
                    );
                    self.decoder_config = new_config;
                    self.stream_too_large = false;
                    worker.reconfigure(new_config);
                    // The new decoder must start on a keyframe.
                    self.waiting_for_keyframe = true;
                } else {
                    if !self.stream_too_large {
                        diag_log::log(
                            "video",
                            format!(
                                "Stream {}x{} is larger than the Vita decoder limit {}x{}; dropping video",
                                sps.coded_width,
                                sps.coded_height,
                                MAX_DECODE_WIDTH,
                                MAX_DECODE_HEIGHT
                            ),
                        );
                    }
                    self.stream_too_large = true;
                    *keyframe_requested = true;
                    if !self.waiting_for_keyframe {
                        worker.begin_resync();
                    }
                    self.waiting_for_keyframe = true;
                    stats.dropped = stats.dropped.saturating_add(1);
                    return;
                }
            }
        }
        if self.stream_too_large {
            if unit.sps.is_none() || !unit.has_idr {
                *keyframe_requested = true;
                self.waiting_for_keyframe = true;
                stats.dropped = stats.dropped.saturating_add(1);
                return;
            }
            self.stream_too_large = false;
        }
        if self.waiting_for_keyframe {
            // Later keyframes may contain only IDR; AVCDEC retains SPS/PPS across resyncs.
            if !unit.has_idr {
                *keyframe_requested = true;
                stats.dropped = stats.dropped.saturating_add(1);
                return;
            }
            self.waiting_for_keyframe = false;
            self.damage_score = 0;
        } else if !frame_was_damaged {
            self.damage_score = self.damage_score.saturating_sub(1);
        }

        if let Some(sps_nal) = &unit.sps_nal {
            self.cached_sps_nal = Some(sps_nal.clone());
        }
        if let Some(pps_nal) = &unit.pps_nal {
            self.cached_pps_nal = Some(pps_nal.clone());
        }
        let mut access_unit = data.to_vec();
        if unit.has_idr
            && (unit.sps_nal.is_none() || unit.pps_nal.is_none())
            && let (Some(sps_nal), Some(pps_nal)) = (&self.cached_sps_nal, &self.cached_pps_nal)
        {
            let mut with_parameter_sets =
                Vec::with_capacity(8 + sps_nal.len() + pps_nal.len() + access_unit.len());
            for nal in [sps_nal, pps_nal] {
                with_parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
                with_parameter_sets.extend_from_slice(nal);
            }
            with_parameter_sets.extend_from_slice(&access_unit);
            access_unit = with_parameter_sets;
            self.parameter_sets_reinjected += 1;
            if self.parameter_sets_reinjected <= 3 || self.parameter_sets_reinjected % 50 == 0 {
                diag_log::log(
                    "video",
                    format!(
                        "Keyframe without SPS/PPS: re-sent cached ones to the decoder (x{})",
                        self.parameter_sets_reinjected
                    ),
                );
            }
        }

        if !worker.submit_access_unit(access_unit, self.source_frame_duration_us) {
            eprintln!("Video decoder queue is full; continuing while requesting a keyframe");
            *keyframe_requested = true;
            stats.dropped = stats.dropped.saturating_add(1);
        }
    }

    fn record_damage(&mut self, worker: &VideoDecodeWorker) {
        if self.waiting_for_keyframe {
            return;
        }

        if crate::streaming::video::freeze_on_loss() {
            // Frames after a lost one reference a broken picture: decoding them shows blocky,
            // smeared areas. Keep the last clean picture on screen until a keyframe arrives
            // (one is requested right away by the caller).
            worker.begin_resync();
            self.waiting_for_keyframe = true;
            self.damage_score = 0;
            self.freezes = self.freezes.saturating_add(1);
            return;
        }

        self.damage_score = self.damage_score.saturating_add(1);
        let source_fps = self
            .source_frame_duration_us
            .filter(|duration| *duration > 0)
            .map(|duration| 1_000_000 / duration)
            .unwrap_or(30);
        let damage_limit = if source_fps <= 30 {
            LOW_FPS_DAMAGE_LIMIT
        } else if source_fps >= 60 {
            HIGH_FPS_DAMAGE_LIMIT
        } else {
            LOW_FPS_DAMAGE_LIMIT
                - (((source_fps - 30) * u64::from(LOW_FPS_DAMAGE_LIMIT - HIGH_FPS_DAMAGE_LIMIT)
                    + 29)
                    / 30) as u8
        };
        if self.damage_score < damage_limit {
            return;
        }

        worker.begin_resync();
        self.waiting_for_keyframe = true;
        self.damage_score = 0;
    }
}

fn timestamp_is_newer(candidate: u32, reference: u32) -> bool {
    let distance = candidate.wrapping_sub(reference);
    distance != 0 && distance < (1 << 31)
}

struct AccessUnitInfo {
    has_idr: bool,
    sps: Option<SpsSummary>,
    sps_nal: Option<Vec<u8>>,
    pps_nal: Option<Vec<u8>>,
}

fn nal_bytes(nal: &RefNal<'_>) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    nal.reader().read_to_end(&mut bytes).ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

fn summarize_sps(sps: &SeqParameterSet) -> Option<SpsSummary> {
    let (width, height) = sps.pixel_dimensions().ok()?;
    let frame_mbs_only = matches!(
        sps.frame_mbs_flags,
        h264_reader::nal::sps::FrameMbsFlags::Frames
    );
    let height_in_mbs = sps.pic_height_in_map_units() * if frame_mbs_only { 1 } else { 2 };
    Some(SpsSummary {
        width,
        height,
        coded_width: sps.pic_width_in_mbs() * 16,
        coded_height: height_in_mbs * 16,
        profile_idc: sps.profile_idc.into(),
        level_idc: sps.level_idc,
        ref_frames: sps.max_num_ref_frames,
        reorder_frames: sps
            .vui_parameters
            .as_ref()
            .and_then(|vui| vui.bitstream_restrictions.as_ref())
            .map(|restrictions| restrictions.max_num_reorder_frames),
        frame_mbs_only,
    })
}

fn inspect_h264_access_unit(data: &[u8]) -> AccessUnitInfo {
    let mut info = AccessUnitInfo {
        has_idr: false,
        sps: None,
        sps_nal: None,
        pps_nal: None,
    };
    let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
        let Ok(header) = nal.header() else {
            return NalInterest::Ignore;
        };
        match header.nal_unit_type() {
            UnitType::SliceLayerWithoutPartitioningIdr => {
                info.has_idr = true;
                NalInterest::Ignore
            }
            UnitType::SeqParameterSet => {
                if nal.is_complete() {
                    info.sps = SeqParameterSet::from_bits(nal.rbsp_bits())
                        .ok()
                        .and_then(|sps| summarize_sps(&sps));
                    info.sps_nal = nal_bytes(&nal);
                }
                NalInterest::Buffer
            }
            UnitType::PicParameterSet => {
                if nal.is_complete() {
                    info.pps_nal = nal_bytes(&nal);
                }
                NalInterest::Buffer
            }
            _ => NalInterest::Ignore,
        }
    });
    reader.push(data);
    reader.reset();
    info
}
