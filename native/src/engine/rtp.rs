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
use rtc::rtp::extension::HeaderExtension;
use rtc::rtp::extension::playout_delay_extension::PlayoutDelayExtension;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RTCRtpHeaderExtensionCapability, RtpCodecKind,
};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use tokio::sync::broadcast;
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::peer_connection::{MediaEngine, PeerConnection};
use webrtc::rtp_transceiver::RtpSender;

use super::turbo::KeyLimiter;
use crate::video::{Control, EncodedFrame};

/// H.264 payload types offered, best first. The GPU encoders produce High
/// profile without B-frames, which is also valid Constrained High; Safari
/// lists only Constrained High, Chrome both. Constrained Baseline stays as a
/// last resort for receivers that take nothing else (older Firefox): the same
/// stream goes out under it, as it always did.
const H264_PROFILES: [(u8, &str); 3] = [(102, "640c34"), (104, "640034"), (108, "42e01f")];
pub const H264_PT: u8 = H264_PROFILES[0].0;

fn h264_codec(profile_level_id: &str) -> RTCRtpCodec {
    let fb = |typ: &str, parameter: &str| rtc::rtp_transceiver::rtp_sender::RTCPFeedback { typ: typ.into(), parameter: parameter.into() };
    RTCRtpCodec {
        mime_type: "video/H264".into(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}"),
        rtcp_feedback: vec![fb("goog-remb", ""), fb("ccm", "fir"), fb("nack", ""), fb("nack", "pli")],
    }
}

fn h264_codecs() -> Vec<RTCRtpCodecParameters> {
    H264_PROFILES.iter().map(|&(payload_type, p)| RTCRtpCodecParameters { rtp_codec: h264_codec(p), payload_type, ..Default::default() }).collect()
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

/// Asks the receiver to show each frame as soon as it is decoded, without the
/// smoothing buffer (Chrome honors it: the same as cloud gaming services).
const PLAYOUT_DELAY_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay";

/// Media engine with only H.264 and Opus: negotiation cannot pick another
/// video codec the app does not produce.
pub fn media_engine() -> Result<MediaEngine, String> {
    let mut m = MediaEngine::default();
    for codec in h264_codecs() {
        m.register_codec(codec, RtpCodecKind::Video).map_err(|e| e.to_string())?;
    }
    m.register_codec(RTCRtpCodecParameters { rtp_codec: opus_codec(), payload_type: 111, ..Default::default() }, RtpCodecKind::Audio)
        .map_err(|e| e.to_string())?;
    m.register_header_extension(RTCRtpHeaderExtensionCapability { uri: PLAYOUT_DELAY_URI.into() }, RtpCodecKind::Video, None)
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

/// The video track and its sender (which knows the negotiated payload type).
#[derive(Clone)]
pub struct VideoTrack {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
}

pub async fn add_video_track(pc: &Arc<dyn PeerConnection>) -> Result<VideoTrack, String> {
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
                    codec: h264_codec(H264_PROFILES[0].1),
                    ..Default::default()
                }],
            ),
        )
        .map_err(|e| e.to_string())?,
    );
    let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| e.to_string())?;
    // The transceiver would offer only the track's own codec: offer every profile.
    for t in pc.get_transceivers().await {
        if t.sender().await.ok().flatten().is_some_and(|s| s.id() == sender.id()) {
            t.set_codec_preferences(h264_codecs()).await.map_err(|e| e.to_string())?;
        }
    }
    Ok(VideoTrack { track, sender })
}

/// What the viewer accepted: the H.264 payload type (High profile first) and
/// whether it takes the playout delay extension.
async fn negotiated(sender: &Arc<dyn RtpSender>) -> (u8, bool) {
    let params = sender.get_parameters().await.map(|p| p.rtp_parameters).unwrap_or_default();
    let pt = params
        .codecs
        .iter()
        .filter(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case("video/H264"))
        .min_by_key(|c| !c.rtp_codec.sdp_fmtp_line.to_ascii_lowercase().contains("profile-level-id=64"))
        .map_or(H264_PT, |c| c.payload_type);
    let no_delay = params.header_extensions.iter().any(|e| e.uri == PLAYOUT_DELAY_URI);
    tracing::info!(payload_type = pt, no_delay, "RTP video negotiated");
    (pt, no_delay)
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
        let mut skipped: u64 = 0;
        while !s.load(Ordering::Relaxed) {
            let p = match tokio::time::timeout(Duration::from_millis(500), packets.recv()).await {
                Ok(Ok(p)) => p,
                // Fell behind: jump to the newest packet. What was skipped goes
                // out as lost (sequence and clock advance), so the viewer conceals
                // it instead of playing late from then on.
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    skipped += n;
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => continue,
            };
            let mut p = p;
            // After a lag the queue is full of old packets: only the newest one
            // goes out. (Bursts of 2 or 3 are normal: capture hands over ~21 ms
            // at a time.)
            while skipped > 0 {
                match packets.try_recv() {
                    Ok(newer) => {
                        skipped += 1;
                        p = newer;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(n)) => skipped += n,
                    Err(_) => break,
                }
            }
            let prev_dropped_packets = skipped.min(u16::MAX as u64) as u16;
            skipped = 0;
            let sample = Sample { data: p.data.clone(), duration: p.duration, prev_dropped_packets, ..Sample::new(Instant::now()) };
            if track.sample_writer(ssrc, OPUS_PT).write_sample(&sample).await.is_err() {
                break;
            }
        }
    });
    AudioSender { stop }
}

#[derive(Default)]
pub struct Counters {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    /// Worst fraction lost (0–255, as in RTCP) among the receiver reports
    /// since the last read, plus one; 0 = no report since then.
    fraction_lost: AtomicU32,
}

impl Counters {
    fn report(&self, fraction_lost: u8) {
        self.fraction_lost.fetch_max(u32::from(fraction_lost) + 1, Ordering::Relaxed);
    }

    /// The worst loss reported since the last call, if any report came.
    pub fn take_fraction_lost(&self) -> Option<u32> {
        self.fraction_lost.swap(0, Ordering::Relaxed).checked_sub(1)
    }
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
    video: VideoTrack,
    mut frames: broadcast::Receiver<Arc<EncodedFrame>>,
    control: Arc<Control>,
    keys: Arc<KeyLimiter>,
    fps: u32,
) -> Sender {
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));

    let VideoTrack { track, sender } = video;
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
                        c.report(r.fraction_lost);
                    }
                }
            }
        }
    });

    let (c, s) = (counters.clone(), stop.clone());
    tokio::spawn(async move {
        let ssrc = track.ssrcs().await.first().copied().unwrap_or(0);
        let (pt, no_delay) = negotiated(&sender).await;
        // The RTP clock advances by each sample's duration. It must follow the
        // real time between captures: a fixed 1/fps while frames come slower
        // (a heavy 4K screen, or a still one sending a frame every 100 ms) made
        // the timestamps fall behind, and the viewer's jitter buffer grew to
        // nearly a second trying to make up for "late" frames.
        let mut last: Option<Instant> = None;
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
            let duration = last
                .map(|t| frame.captured.saturating_duration_since(t))
                .unwrap_or(Duration::from_secs_f64(1.0 / fps.max(1) as f64))
                .clamp(Duration::from_millis(1), Duration::from_secs(1));
            last = Some(frame.captured);
            let sample = Sample { data: frame.data.clone(), duration, ..Sample::new(frame.captured) };
            // Only if the viewer accepted the extension: the sender refuses
            // packets carrying one that was not negotiated.
            let mut writer = track.sample_writer(ssrc, pt);
            if no_delay {
                writer = writer.with_extension(HeaderExtension::PlayoutDelay(PlayoutDelayExtension { min_delay: 0, max_delay: 0 }));
            }
            if writer.write_sample(&sample).await.is_err() {
                break;
            }
            c.frames.fetch_add(1, Ordering::Relaxed);
            c.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        }
    });
    Sender { counters, stop }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::peer_connection::RTCSessionDescription;

    async fn offer() -> (Arc<dyn PeerConnection>, VideoTrack, String) {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        let pc = super::super::peer::new_connection(&[], "teste", true, tx).await.unwrap();
        let video = add_video_track(&pc).await.unwrap();
        add_audio_track(&pc).await.unwrap();
        let offer = pc.create_offer(None).await.unwrap();
        pc.set_local_description(offer.clone()).await.unwrap();
        (pc, video, offer.sdp)
    }

    #[tokio::test]
    async fn oferece_high_baseline_e_atraso_zero() {
        let (_pc, _video, sdp) = offer().await;
        assert!(sdp.contains("m=video 9 UDP/TLS/RTP/SAVPF 102 104 108\r\n"), "{sdp}");
        for (pt, profile) in H264_PROFILES {
            assert!(sdp.contains(&format!("a=fmtp:{pt} level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile}")));
        }
        let video = &sdp[sdp.find("m=video").unwrap()..sdp.find("m=audio").unwrap()];
        assert!(video.contains(PLAYOUT_DELAY_URI));
        assert!(!sdp[sdp.find("m=audio").unwrap()..].contains(PLAYOUT_DELAY_URI));
    }

    // Headless Chrome 151 on Linux answering that offer through the site's
    // tuneReceiver: its H.264 decoder lists 42001f, 42e01f, 4d001f and f4001f,
    // so it takes the Constrained Baseline entry, plus the playout delay.
    const CHROME_ANSWER: &str = "v=0\r\no=- 6183926426983307877 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\na=extmap-allow-mixed\r\na=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 108\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\na=ice-ufrag:zgv0\r\na=ice-pwd:Y2HmuAafpE2QhGd3RL17mPvE\r\na=ice-options:trickle\r\n\
a=fingerprint:sha-256 8B:0F:D7:CF:1D:D4:9D:3C:FC:8F:CA:10:45:BC:DE:7E:30:5A:8C:04:D8:0F:49:9B:D8:91:E6:E8:C2:4C:A4:F5\r\na=setup:active\r\na=mid:0\r\n\
a=extmap:5 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\na=extmap:1 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay\r\n\
a=extmap:2 urn:ietf:params:rtp-hdrext:sdes:mid\r\na=extmap:3 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id\r\na=extmap:4 urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id\r\n\
a=recvonly\r\na=rtcp-mux\r\na=rtcp-rsize\r\na=rtpmap:108 H264/90000\r\na=rtcp-fb:108 goog-remb\r\na=rtcp-fb:108 transport-cc\r\na=rtcp-fb:108 ccm fir\r\n\
a=rtcp-fb:108 nack\r\na=rtcp-fb:108 nack pli\r\na=fmtp:108 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\na=ice-ufrag:zgv0\r\na=ice-pwd:Y2HmuAafpE2QhGd3RL17mPvE\r\na=ice-options:trickle\r\n\
a=fingerprint:sha-256 8B:0F:D7:CF:1D:D4:9D:3C:FC:8F:CA:10:45:BC:DE:7E:30:5A:8C:04:D8:0F:49:9B:D8:91:E6:E8:C2:4C:A4:F5\r\na=setup:active\r\na=mid:1\r\n\
a=extmap:5 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\na=recvonly\r\na=rtcp-mux\r\na=rtcp-rsize\r\na=rtpmap:111 opus/48000/2\r\n\
a=rtcp-fb:111 transport-cc\r\na=fmtp:111 minptime=10;useinbandfec=1\r\n";

    async fn negotiate(answer: String) -> (u8, bool) {
        let (pc, video, _) = offer().await;
        pc.set_remote_description(RTCSessionDescription::answer(answer).unwrap()).await.unwrap();
        negotiated(&video.sender).await
    }

    #[tokio::test]
    async fn segue_o_que_o_navegador_aceitou() {
        assert_eq!(negotiate(CHROME_ANSWER.into()).await, (108, true));
        // A receiver that takes High (Safari lists Constrained High) gets it first.
        let high = CHROME_ANSWER
            .replace("SAVPF 108\r\n", "SAVPF 108 102\r\n")
            .replace("a=rtpmap:108 H264/90000\r\n", "a=rtpmap:108 H264/90000\r\na=rtpmap:102 H264/90000\r\na=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f\r\n")
            .replace("a=extmap:1 http://www.webrtc.org/experiments/rtp-hdrext/playout-delay\r\n", "");
        assert_eq!(negotiate(high).await, (102, false));
    }
}
