//! WebRTC connections (webrtc-rs) in the format the site's PeerJS expects.

use std::sync::Arc;

use bytes::BytesMut;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit,
    RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState, Registry, register_default_interceptors,
};

/// Minimum-latency video channel: negotiated on both sides with the same id,
/// unordered, with retransmission for up to 400 ms. On a direct link a resend
/// takes ~1 ms; over the relay (~130 ms round trip) 400 ms leaves time to retry.
pub const VIDEO_CHANNEL_ID: u16 = 100;
pub const VIDEO_PACKET_LIFETIME_MS: u16 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Control,
    Video,
}

pub enum PeerEvent {
    LocalCandidate { conn: String, candidate: RTCIceCandidateInit },
    State { conn: String, state: RTCPeerConnectionState },
    RemoteChannel { conn: String, channel: Arc<dyn DataChannel> },
    Open { conn: String, which: Which },
    Message { conn: String, which: Which, data: BytesMut },
    Closed { conn: String, which: Which },
}

#[derive(Clone)]
struct Handler {
    conn: String,
    tx: mpsc::Sender<PeerEvent>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(candidate) = event.candidate.to_json() {
            let _ = self.tx.send(PeerEvent::LocalCandidate { conn: self.conn.clone(), candidate }).await;
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.tx.send(PeerEvent::State { conn: self.conn.clone(), state }).await;
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.tx.send(PeerEvent::RemoteChannel { conn: self.conn.clone(), channel }).await;
    }
}

/// `video`: media connection (RTP with H.264/Opus); otherwise data channels only.
pub async fn new_connection(ice: &[RTCIceServer], conn: &str, video: bool, tx: mpsc::Sender<PeerEvent>) -> Result<Arc<dyn PeerConnection>, String> {
    let mut media = if video {
        super::rtp::media_engine()?
    } else {
        let mut m = MediaEngine::default();
        m.register_default_codecs().map_err(|e| e.to_string())?;
        m
    };
    let registry = register_default_interceptors(Registry::new(), &mut media)
        .map_err(|e| e.to_string())?
        .with(rtc::interceptor::Slot::from(14_000), super::rtp::Feedback::new());
    let config = RTCConfigurationBuilder::new().with_ice_servers(ice.to_vec()).build();
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler { conn: conn.to_owned(), tx }))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(pc))
}

/// PeerJS control channel (the "DataConnection"): reliable, ordered,
/// label = connection id.
pub async fn control_channel(pc: &Arc<dyn PeerConnection>, label: &str) -> Result<Arc<dyn DataChannel>, String> {
    pc.create_data_channel(label, Some(RTCDataChannelInit { ordered: true, ..Default::default() })).await.map_err(|e| e.to_string())
}

pub async fn video_channel(pc: &Arc<dyn PeerConnection>) -> Result<Arc<dyn DataChannel>, String> {
    pc.create_data_channel(
        "telinha-video",
        Some(RTCDataChannelInit {
            ordered: false,
            max_packet_life_time: Some(VIDEO_PACKET_LIFETIME_MS),
            negotiated: Some(VIDEO_CHANNEL_ID),
            ..Default::default()
        }),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Forwards a channel's events to the session.
pub fn pump(channel: Arc<dyn DataChannel>, conn: String, which: Which, tx: mpsc::Sender<PeerEvent>) {
    tokio::spawn(async move {
        while let Some(ev) = channel.poll().await {
            let e = match ev {
                DataChannelEvent::OnOpen => PeerEvent::Open { conn: conn.clone(), which },
                DataChannelEvent::OnMessage(m) => PeerEvent::Message { conn: conn.clone(), which, data: m.data },
                DataChannelEvent::OnClose => {
                    let _ = tx.send(PeerEvent::Closed { conn: conn.clone(), which }).await;
                    break;
                }
                _ => continue,
            };
            if tx.send(e).await.is_err() {
                break;
            }
        }
    });
}

/// JSON message in PeerJS "json" serialization format: UTF-8 text
/// sent as binary (the browser decodes it with TextDecoder).
pub async fn send_json(channel: &Arc<dyn DataChannel>, v: &Value) {
    let _ = channel.send(BytesMut::from(v.to_string().as_bytes())).await;
}

pub fn candidate_payload(candidate: &RTCIceCandidateInit, kind: &str, conn: &str) -> Value {
    json!({
        "candidate": {
            "candidate": candidate.candidate,
            "sdpMid": candidate.sdp_mid,
            "sdpMLineIndex": candidate.sdp_mline_index,
            "usernameFragment": candidate.username_fragment,
        },
        "type": kind,
        "connectionId": conn,
    })
}

pub fn parse_candidate(payload: &Value) -> Option<RTCIceCandidateInit> {
    let c = &payload["candidate"];
    Some(RTCIceCandidateInit {
        candidate: c["candidate"].as_str()?.to_owned(),
        sdp_mid: c["sdpMid"].as_str().map(str::to_owned),
        sdp_mline_index: c["sdpMLineIndex"].as_u64().map(|v| v as u16),
        username_fragment: c["usernameFragment"].as_str().map(str::to_owned),
        ..Default::default()
    })
}
