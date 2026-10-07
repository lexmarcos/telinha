//! RTP path: the same H.264 from the GPU, but as regular WebRTC video
//! (retransmission with NACK, keyframe requests with PLI). This is the path for
//! viewers on the relay, with poor reception over the data channel, or using a
//! browser without WebCodecs (Firefox, Safari).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::media::Sample;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::receiver_report::ReceiverReport;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use tokio::sync::broadcast;
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::peer_connection::{MediaEngine, PeerConnection};

use super::turbo::KeyLimiter;
use crate::video::{Control, EncodedFrame};

pub const H264_PT: u8 = 102;

fn h264_codec() -> RTCRtpCodec {
    let fb = |typ: &str, parameter: &str| rtc::rtp_transceiver::rtp_sender::RTCPFeedback { typ: typ.into(), parameter: parameter.into() };
    RTCRtpCodec {
        mime_type: "video/H264".into(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into(),
        rtcp_feedback: vec![fb("goog-remb", ""), fb("ccm", "fir"), fb("nack", ""), fb("nack", "pli")],
    }
}

pub fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: "audio/opus".into(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".into(),
        rtcp_feedback: vec![],
    }
}

/// Media engine with only H.264 and Opus: negotiation cannot pick another
/// video codec the app does not produce.
pub fn media_engine() -> Result<MediaEngine, String> {
    let mut m = MediaEngine::default();
    m.register_codec(RTCRtpCodecParameters { rtp_codec: h264_codec(), payload_type: H264_PT, ..Default::default() }, RtpCodecKind::Video)
        .map_err(|e| e.to_string())?;
    m.register_codec(RTCRtpCodecParameters { rtp_codec: opus_codec(), payload_type: 111, ..Default::default() }, RtpCodecKind::Audio)
        .map_err(|e| e.to_string())?;
    Ok(m)
}

/* ---------------- interceptor: PLI/FIR and reports for the app ---------------- */

/// Incoming RTCP stops at the interceptors; this one passes to the app only
/// what matters: keyframe requests and receiver reports (loss).
pub struct Feedback {
    read: VecDeque<TaggedPacket>,
    write: VecDeque<TaggedPacket>,
}

impl Feedback {
    pub fn new() -> Self {
        Self { read: VecDeque::new(), write: VecDeque::new() }
    }
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for Feedback {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let keep: Vec<Box<dyn rtc::rtcp::Packet>> = packets
                .iter()
                .filter(|p| {
                    let a = p.as_any();
                    a.is::<PictureLossIndication>() || a.is::<FullIntraRequest>() || a.is::<ReceiverReport>()
                })
                .cloned()
                .collect();
            if keep.is_empty() {
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(keep);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.read.push_back(msg);
        Ok(())
    }
    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read.pop_front()
    }
    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write.push_back(msg);
        Ok(())
    }
    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write.pop_front()
    }
}

impl Interceptor for Feedback {
    fn bind_local_stream(&mut self, _: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _: &StreamInfo) {}
}

/* ---------------- track and sender ---------------- */

pub async fn add_video_track(pc: &Arc<dyn PeerConnection>) -> Result<Arc<TrackLocalStaticSample>, String> {
    let ssrc = rand::random::<u32>();
    let track = Arc::new(
        TrackLocalStaticSample::new(
            Instant::now(),
            rtc::media_stream::MediaStreamTrack::new(
                "telinha".into(),
                "telinha-video".into(),
                "Tela".into(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                    codec: h264_codec(),
                    ..Default::default()
                }],
            ),
        )
        .map_err(|e| e.to_string())?,
    );
    pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| e.to_string())?;
    Ok(track)
}

pub const OPUS_PT: u8 = 111;

pub async fn add_audio_track(pc: &Arc<dyn PeerConnection>) -> Result<Arc<TrackLocalStaticSample>, String> {
    let ssrc = rand::random::<u32>();
    let track = Arc::new(
        TrackLocalStaticSample::new(
            Instant::now(),
            rtc::media_stream::MediaStreamTrack::new(
                "telinha".into(),
                "telinha-som".into(),
                "Som".into(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                    codec: opus_codec(),
                    ..Default::default()
                }],
            ),
        )
        .map_err(|e| e.to_string())?,
    );
    pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| e.to_string())?;
    Ok(track)
}

pub struct AudioSender {
    stop: Arc<AtomicBool>,
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sends the Opus packets (already encoded, shared by everyone) on a track.
pub fn spawn_audio(track: Arc<TrackLocalStaticSample>, mut packets: broadcast::Receiver<Arc<crate::audio::OpusPacket>>) -> AudioSender {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    tokio::spawn(async move {
        let ssrc = track.ssrcs().await.first().copied().unwrap_or(0);
        while !s.load(Ordering::Relaxed) {
            match tokio::time::timeout(Duration::from_millis(500), packets.recv()).await {
                Ok(Ok(p)) => {
                    let sample = Sample { data: p.data.clone(), duration: p.duration, ..Sample::new(Instant::now()) };
                    if track.sample_writer(ssrc, OPUS_PT).write_sample(&sample).await.is_err() {
                        break;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => {}
            }
        }
    });
    AudioSender { stop }
}

#[derive(Default)]
pub struct Counters {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    /// Fraction lost in the last receiver report (0–255, as in RTCP).
    pub fraction_lost: AtomicU32,
}

pub struct Sender {
    pub counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn spawn(
    track: Arc<TrackLocalStaticSample>,
    mut frames: broadcast::Receiver<Arc<EncodedFrame>>,
    control: Arc<Control>,
    keys: Arc<KeyLimiter>,
    fps: u32,
) -> Sender {
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));

    // Keyframe requests and loss reports from the viewer.
    let (t, c, s, ctl, k) = (track.clone(), counters.clone(), stop.clone(), control.clone(), keys.clone());
    tokio::spawn(async move {
        while !s.load(Ordering::Relaxed) {
            let Some(TrackLocalEvent::OnRtcpPacket(packets)) = t.poll().await else { break };
            for p in packets {
                let a = p.as_any();
                if a.is::<PictureLossIndication>() || a.is::<FullIntraRequest>() {
                    k.request(&ctl, Duration::from_millis(300));
                } else if let Some(rr) = a.downcast_ref::<ReceiverReport>() {
                    if let Some(r) = rr.reports.first() {
                        c.fraction_lost.store(r.fraction_lost as u32, Ordering::Relaxed);
                    }
                }
            }
        }
    });

    let (c, s) = (counters.clone(), stop.clone());
    tokio::spawn(async move {
        let ssrc = track.ssrcs().await.first().copied().unwrap_or(0);
        let duration = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let mut need_key = true;
        keys.request(&control, Duration::ZERO);
        while !s.load(Ordering::Relaxed) {
            let frame = match tokio::time::timeout(Duration::from_millis(500), frames.recv()).await {
                Ok(Ok(f)) => f,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    need_key = true;
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => continue,
            };
            if need_key && !frame.key {
                keys.request(&control, Duration::from_millis(500));
                continue;
            }
            need_key = false;
            let sample = Sample { data: frame.data.clone(), duration, ..Sample::new(frame.captured) };
            if track.sample_writer(ssrc, H264_PT).write_sample(&sample).await.is_err() {
                break;
            }
            c.frames.fetch_add(1, Ordering::Relaxed);
            c.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        }
    });
    Sender { counters, stop }
}
