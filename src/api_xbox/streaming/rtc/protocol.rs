use crate::api::streaming::rtc::peer::PeerConnection as RTCPeerConnection;
use crate::api::streaming::rtc::session::RtcSessionBackend;
use crate::api_xbox::streaming::control::channel::{self, HandshakeStage};
use crate::api_xbox::streaming::control::input::{InputQueue, PointerFrame};
use crate::streaming::input::{GamepadFrame, PointerEvent};
use bytes::BytesMut;
use rtc::data_channel::RTCDataChannelId;

#[derive(Clone, Copy)]
pub(in crate::api_xbox::streaming) struct ChannelIds {
    pub control: RTCDataChannelId,
    pub input: RTCDataChannelId,
    pub message: RTCDataChannelId,
}

pub(super) struct XboxRtcProtocol {
    handshake_stage: HandshakeStage,
    input_queue: InputQueue,
    input_channel_ready: bool,
    server_video_size: Option<(u32, u32)>,
    channel_ids: ChannelIds,
    /// Set when the input channel's send buffer is backing up (SCTP slowed down after
    /// losses). Only the latest controller state matters, so while it is set new states are
    /// not queued behind old ones - queuing them made the game replay inputs seconds late.
    input_congested_since: Option<std::time::Instant>,
    skipped_input_packets: u64,
}

/// Input send buffer level that counts as congested, and the level at which it is clear.
const INPUT_BUFFER_HIGH: u32 = 1024;
const INPUT_BUFFER_LOW: u32 = 256;
/// Safety net if the "buffer low" event never comes: send again after this long anyway.
const INPUT_CONGESTION_MAX_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

impl XboxRtcProtocol {
    pub(super) fn new(channel_ids: ChannelIds) -> Self {
        Self {
            handshake_stage: HandshakeStage::WaitingForChannels,
            input_queue: InputQueue::default(),
            input_channel_ready: false,
            server_video_size: None,
            channel_ids,
            input_congested_since: None,
            skipped_input_packets: 0,
        }
    }

    fn send_gamepad_frame_inner(
        &mut self,
        peer: &mut RTCPeerConnection,
        frame: GamepadFrame,
    ) -> bool {
        if !self.input_channel_ready {
            return false;
        }
        if let Some(since) = self.input_congested_since {
            if since.elapsed() < INPUT_CONGESTION_MAX_PAUSE {
                // Not sent: the caller retries with the newest state on its next pass.
                self.skipped_input_packets = self.skipped_input_packets.saturating_add(1);
                return false;
            }
            self.input_congested_since = Some(std::time::Instant::now());
        }
        let Some(bytes) = self.input_queue.queue_gamepad_frames([frame], true) else {
            return false;
        };
        self.send_input_bytes(peer, &bytes)
    }

    fn send_pointer_event_inner(&mut self, peer: &mut RTCPeerConnection, event: PointerEvent) {
        if !self.input_channel_ready {
            return;
        }
        let Some(bytes) = self.input_queue.queue_pointer_frame(PointerFrame {
            events: vec![event],
        }) else {
            return;
        };
        let _ = self.send_input_bytes(peer, &bytes);
    }

    fn handle_channel_open_inner(
        &mut self,
        peer: &mut RTCPeerConnection,
        channel_id: RTCDataChannelId,
    ) {
        let channel_ids = self.channel_ids;
        if channel_id == channel_ids.message
            && self.handshake_stage == HandshakeStage::WaitingForChannels
            && let Some(mut message_channel) = peer.data_channel(channel_id)
        {
            let _ = message_channel.send_text(channel::message_handshake().to_string());
            self.handshake_stage = HandshakeStage::WaitingForHandshakeAck;
        }
        if channel_id == channel_ids.input
            && let Some(mut input_channel) = peer.data_channel(channel_id)
        {
            input_channel.set_buffered_amount_high_threshold(INPUT_BUFFER_HIGH);
            input_channel.set_buffered_amount_low_threshold(INPUT_BUFFER_LOW);
            let client_metadata = self.input_queue.client_metadata_packet(0);
            let _ = input_channel.send(BytesMut::from(client_metadata.as_slice()));
            self.input_channel_ready = true;
        }
    }

    fn handle_channel_message_inner(
        &mut self,
        peer: &mut RTCPeerConnection,
        channel_id: RTCDataChannelId,
        data: &[u8],
    ) {
        let channel_ids = self.channel_ids;
        channel::handle_data_channel_message(
            peer,
            &channel_ids,
            &mut self.handshake_stage,
            channel_id,
            data,
        );
        if let Some(size) = channel::parse_server_video_size(&channel_ids, channel_id, data) {
            eprintln!("xCloud reported server video size: {size:?}");
            self.server_video_size = Some(size);
        }
    }

    fn notify_keyframe_requested_inner(&mut self, peer: &mut RTCPeerConnection) {
        if let Some(mut control_channel) = peer.data_channel(self.channel_ids.control) {
            let _ = control_channel.send_text(channel::video_keyframe_requested(true).to_string());
        }
    }

    fn send_input_bytes(&self, peer: &mut RTCPeerConnection, bytes: &[u8]) -> bool {
        if let Some(mut input_channel) = peer.data_channel(self.channel_ids.input) {
            input_channel.send(BytesMut::from(bytes)).is_ok()
        } else {
            false
        }
    }
}

impl RtcSessionBackend for XboxRtcProtocol {
    fn handle_channel_open(&mut self, peer: &mut RTCPeerConnection, channel_id: RTCDataChannelId) {
        self.handle_channel_open_inner(peer, channel_id);
    }

    fn handle_channel_message(
        &mut self,
        peer: &mut RTCPeerConnection,
        channel_id: RTCDataChannelId,
        data: &[u8],
    ) {
        self.handle_channel_message_inner(peer, channel_id, data);
    }

    fn send_gamepad_frame(&mut self, peer: &mut RTCPeerConnection, frame: GamepadFrame) -> bool {
        self.send_gamepad_frame_inner(peer, frame)
    }

    fn send_pointer_event(&mut self, peer: &mut RTCPeerConnection, event: PointerEvent) {
        self.send_pointer_event_inner(peer, event);
    }

    fn notify_keyframe_requested(&mut self, peer: &mut RTCPeerConnection) {
        self.notify_keyframe_requested_inner(peer);
    }

    fn server_video_size(&self) -> Option<(u32, u32)> {
        self.server_video_size
    }

    fn handle_buffered_amount(&mut self, channel_id: RTCDataChannelId, high: bool) {
        if channel_id != self.channel_ids.input {
            return;
        }
        if high {
            if self.input_congested_since.is_none() {
                crate::diag_log::log("input", "input channel congested: holding stale inputs");
            }
            self.input_congested_since = Some(std::time::Instant::now());
        } else {
            self.input_congested_since = None;
        }
    }

    fn skipped_input_packets(&self) -> u64 {
        self.skipped_input_packets
    }
}
