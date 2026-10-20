//! What kind of network a connection runs over, from its selected ICE pair:
//! the same local network, some other direct path, or the relay (TURN).
//! Mirrors `pathKind` in stats.js, which the site uses for its panel.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use webrtc::peer_connection::transport::{DtlsTransport, IceTransport, SctpTransport};
use webrtc::peer_connection::{PeerConnection, RTCIceCandidate, RTCIceCandidateType, StatsSelector};
use webrtc::rtp_transceiver::RtpSender;

/// Private addresses can also be a VPN across the world: a round trip longer
/// than a local network (even a poor Wi-Fi) rules the LAN out.
const LAN_MAX_RTT_MS: f64 = 25.0;
/// Without a private address to go by (public IPv6 on the same link, an
/// address we cannot see), only a round trip this short says "same network".
const BARE_MAX_RTT_MS: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// Both ends on the same local network.
    Lan,
    /// Direct, but across the internet (NAT hole punching, public addresses).
    Direct,
    /// Through the TURN relay.
    Relay,
}

impl Path {
    pub fn as_str(self) -> &'static str {
        match self {
            Path::Lan => "lan",
            Path::Direct => "direct",
            Path::Relay => "relay",
        }
    }
}

/// One side of the selected pair.
#[derive(Debug, Clone, Copy)]
pub struct Side {
    pub kind: RTCIceCandidateType,
    pub addr: Option<IpAddr>,
}

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_link_local(),
        // Unique local fc00::/7 and link-local fe80::/10; IPv4-mapped follows IPv4.
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => m.is_private() || m.is_link_local(),
            None => (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80,
        },
    }
}

/// Peer-reflexive counts as direct-local: it is how a host address shows up
/// when the other side's name or candidate had not reached us yet.
fn on_link(kind: RTCIceCandidateType) -> bool {
    matches!(kind, RTCIceCandidateType::Host | RTCIceCandidateType::Prflx)
}

pub fn classify(local: Side, remote: Side, rtt_ms: Option<f64>) -> Path {
    if local.kind == RTCIceCandidateType::Relay || remote.kind == RTCIceCandidateType::Relay {
        return Path::Relay;
    }
    if !on_link(local.kind) || !on_link(remote.kind) {
        return Path::Direct;
    }
    let addrs: Vec<IpAddr> = [local.addr, remote.addr].into_iter().flatten().collect();
    let all_private = !addrs.is_empty() && addrs.iter().all(|a| private(*a));
    let lan = if all_private { rtt_ms.is_none_or(|r| r <= LAN_MAX_RTT_MS) } else { rtt_ms.is_some_and(|r| r <= BARE_MAX_RTT_MS) };
    if lan { Path::Lan } else { Path::Direct }
}

/// The selected pair: its kind, round trip (ms) and the candidate types of each end.
pub struct Selected {
    pub path: Path,
    pub rtt_ms: Option<f64>,
    pub types: String,
}

fn type_name(kind: RTCIceCandidateType) -> &'static str {
    match kind {
        RTCIceCandidateType::Host => "host",
        RTCIceCandidateType::Srflx => "srflx",
        RTCIceCandidateType::Prflx => "prflx",
        RTCIceCandidateType::Relay => "relay",
        _ => "?",
    }
}

/// The ICE transport of a connection: through the data channels' SCTP, or, for a
/// connection that only carries media, through a sender.
async fn ice_transport(pc: &Arc<dyn PeerConnection>) -> Option<Arc<dyn IceTransport>> {
    if let Some(sctp) = pc.sctp().await {
        return Some(sctp.transport().ice_transport());
    }
    for sender in pc.get_senders().await {
        if let Ok(Some(dtls)) = sender.transport().await {
            return Some(dtls.ice_transport());
        }
    }
    None
}

/// The pair ICE selected, once it has one. Types and addresses come from the ICE
/// transport: the statistics leave out a remote candidate learned from the other
/// side's own check, which is nearly every browser.
pub async fn of_connection(pc: &Arc<dyn PeerConnection>) -> Option<Selected> {
    let pair = ice_transport(pc).await?.get_selected_candidate_pair().await.ok()??;
    let side = |c: &RTCIceCandidate| Side { kind: c.typ, addr: c.address.parse().ok() };
    let (local, remote) = (side(pair.local()), side(pair.remote()));
    // Only the round trip is read from the statistics.
    let report = pc.get_stats(Instant::now(), StatsSelector::None).await;
    let selected = report.transport().map(|t| t.selected_candidate_pair_id.clone()).unwrap_or_default();
    let rtt = report.candidate_pairs().find(|p| p.stats.id == selected).map(|p| p.current_round_trip_time * 1000.0).filter(|r| *r > 0.0);
    Some(Selected { path: classify(local, remote, rtt), rtt_ms: rtt, types: format!("{}/{}", type_name(local.kind), type_name(remote.kind)) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use RTCIceCandidateType::{Host, Prflx, Relay, Srflx};

    fn s(kind: RTCIceCandidateType, addr: &str) -> Side {
        Side { kind, addr: addr.parse().ok() }
    }

    #[test]
    fn same_network_is_lan() {
        assert_eq!(classify(s(Host, "192.168.1.5"), s(Host, "192.168.1.9"), Some(1.5)), Path::Lan);
        assert_eq!(classify(s(Host, "10.0.0.2"), s(Prflx, "10.0.0.7"), None), Path::Lan);
        assert_eq!(classify(s(Host, "fd12::1"), s(Host, "fd12::2"), Some(2.0)), Path::Lan);
        // A browser hides its own address: the other side's is enough.
        assert_eq!(classify(Side { kind: Host, addr: None }, s(Host, "172.20.1.4"), Some(3.0)), Path::Lan);
    }

    #[test]
    fn nat_and_relay_are_not_lan() {
        assert_eq!(classify(s(Host, "192.168.1.5"), s(Srflx, "203.0.113.7"), Some(1.0)), Path::Direct);
        assert_eq!(classify(s(Srflx, "203.0.113.7"), s(Srflx, "198.51.100.2"), Some(30.0)), Path::Direct);
        assert_eq!(classify(s(Relay, "203.0.113.9"), s(Host, "192.168.1.9"), Some(1.0)), Path::Relay);
        assert_eq!(classify(s(Host, "192.168.1.5"), s(Relay, "203.0.113.9"), None), Path::Relay);
    }

    #[test]
    fn a_vpn_or_a_far_peer_is_not_lan() {
        // Private addresses over a VPN with a long round trip.
        assert_eq!(classify(s(Host, "10.8.0.2"), s(Host, "10.8.0.3"), Some(60.0)), Path::Direct);
        // Public host addresses (IPv6 across the internet).
        assert_eq!(classify(s(Host, "2001:db8::1"), s(Host, "2001:db8:1::2"), Some(40.0)), Path::Direct);
        // Public host addresses with a round trip only a shared link has.
        assert_eq!(classify(s(Host, "2001:db8::1"), s(Host, "2001:db8::2"), Some(1.0)), Path::Lan);
        // Nothing known about the addresses and no measurement.
        assert_eq!(classify(Side { kind: Host, addr: None }, Side { kind: Host, addr: None }, None), Path::Direct);
    }
}
