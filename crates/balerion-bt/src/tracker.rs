//! Tracker clients: HTTP (BEP 3, compact peers per BEP 23) and UDP (BEP 15).
//!
//! Both answer the same question — who else is in this swarm — and both are
//! best-effort. Public trackers time out, rate limit and lie about peer counts
//! constantly, so callers should query several concurrently and take the union.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::Duration;

use serde_bencode::value::Value as Bencode;
use tokio::net::UdpSocket;

use crate::bencode;
use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// BEP 15's protocol magic for the connect handshake.
const UDP_PROTOCOL_ID: u64 = 0x0417_2710_1980;

const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;

/// What we tell a tracker we are up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    None,
    Completed,
    Started,
    Stopped,
}

impl Event {
    fn as_u32(self) -> u32 {
        match self {
            Event::None => 0,
            Event::Completed => 1,
            Event::Started => 2,
            Event::Stopped => 3,
        }
    }

    fn as_str(self) -> Option<&'static str> {
        match self {
            Event::None => None,
            Event::Completed => Some("completed"),
            Event::Started => Some("started"),
            Event::Stopped => Some("stopped"),
        }
    }
}

/// Everything a tracker needs to know about us.
#[derive(Debug, Clone)]
pub struct Announce {
    pub info_hash: InfoHash,
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
    pub num_want: i32,
}

impl Announce {
    pub fn new(info_hash: InfoHash, peer_id: [u8; 20], left: u64) -> Self {
        Self {
            info_hash,
            peer_id,
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left,
            event: Event::Started,
            num_want: -1,
        }
    }
}

/// What a tracker told us.
#[derive(Debug, Clone, Default)]
pub struct TrackerResponse {
    pub peers: Vec<SocketAddr>,
    /// Seconds until we should re-announce. Honour it.
    pub interval: Option<u32>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
}

/// Announce to whatever kind of tracker the URL points at.
pub async fn announce(url: &str, req: &Announce, timeout: Duration) -> Result<TrackerResponse> {
    if url.starts_with("udp://") {
        udp_announce(url, req, timeout).await
    } else if url.starts_with("http://") || url.starts_with("https://") {
        http_announce(url, req, timeout).await
    } else {
        Err(Error::Tracker(format!("unsupported tracker scheme: {url}")))
    }
}

// ---------------------------------------------------------------- HTTP (BEP 3)

/// Build the announce query string. `info_hash` and `peer_id` are
/// percent-encoded **raw bytes**, not hex: getting that wrong is the single
/// most common tracker bug.
pub fn http_announce_url(base: &str, req: &Announce) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{base}{separator}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant={}",
        req.info_hash.to_query_escaped(),
        escape_bytes(&req.peer_id),
        req.port,
        req.uploaded,
        req.downloaded,
        req.left,
        req.num_want.max(0),
    );
    if let Some(event) = req.event.as_str() {
        url.push_str(&format!("&event={event}"));
    }
    url
}

async fn http_announce(url: &str, req: &Announce, timeout: Duration) -> Result<TrackerResponse> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("balerion/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let body = client
        .get(http_announce_url(url, req))
        .send()
        .await?
        .bytes()
        .await?;
    parse_http_response(&body)
}

/// Parse a bencoded tracker response. A `failure reason` key means the
/// announce failed whatever the HTTP status said.
pub fn parse_http_response(body: &[u8]) -> Result<TrackerResponse> {
    let root = match serde_bencode::from_bytes::<Bencode>(body) {
        Ok(Bencode::Dict(dict)) => dict,
        Ok(_) => return Err(Error::Tracker("response is not a dictionary".into())),
        Err(err) => return Err(Error::Tracker(format!("bad bencode: {err}"))),
    };

    if let Some(reason) = bencode::dict_string(&root, b"failure reason") {
        return Err(Error::Tracker(reason));
    }

    let mut peers = Vec::new();
    match root.get(b"peers".as_slice()) {
        // BEP 23 compact form: six bytes per peer.
        Some(Bencode::Bytes(compact)) => peers.extend(parse_compact_v4(compact)?),
        // The original dict form, still seen in the wild.
        Some(Bencode::List(entries)) => {
            for entry in entries {
                if let Bencode::Dict(entry) = entry
                    && let (Some(ip), Some(port)) = (
                        bencode::dict_string(entry, b"ip"),
                        bencode::dict_int(entry, b"port"),
                    )
                    && let Ok(ip) = ip.parse::<std::net::IpAddr>()
                    && (1..=u16::MAX as i64).contains(&port)
                {
                    peers.push(SocketAddr::new(ip, port as u16));
                }
            }
        }
        _ => {}
    }
    if let Some(Bencode::Bytes(compact)) = root.get(b"peers6".as_slice()) {
        peers.extend(parse_compact_v6(compact)?);
    }

    Ok(TrackerResponse {
        peers,
        interval: bencode::dict_int(&root, b"interval").map(|n| n.clamp(0, u32::MAX as i64) as u32),
        seeders: bencode::dict_int(&root, b"complete").map(|n| n.max(0) as u32),
        leechers: bencode::dict_int(&root, b"incomplete").map(|n| n.max(0) as u32),
    })
}

// ----------------------------------------------------------------- UDP (BEP 15)

async fn udp_announce(url: &str, req: &Announce, timeout: Duration) -> Result<TrackerResponse> {
    let host = url
        .trim_start_matches("udp://")
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| Error::Tracker(format!("no host in {url}")))?;

    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let addr = tokio::net::lookup_host(host)
        .await
        .map_err(|err| Error::Tracker(format!("cannot resolve {host}: {err}")))?
        .next()
        .ok_or_else(|| Error::Tracker(format!("{host} resolved to nothing")))?;
    socket.connect(addr).await?;

    // 1. Connect, to get a connection id the tracker will accept. It exists
    //    purely to make UDP source spoofing expensive.
    let connect_txn: u32 = rand::random();
    socket.send(&encode_connect(connect_txn)).await?;
    let mut buf = vec![0u8; 4096];
    let read = recv_with_timeout(&socket, &mut buf, timeout).await?;
    let connection_id = decode_connect(&buf[..read], connect_txn)?;

    // 2. Announce.
    let announce_txn: u32 = rand::random();
    socket
        .send(&encode_announce(connection_id, announce_txn, req))
        .await?;
    let read = recv_with_timeout(&socket, &mut buf, timeout).await?;
    decode_announce(&buf[..read], announce_txn)
}

async fn recv_with_timeout(socket: &UdpSocket, buf: &mut [u8], timeout: Duration) -> Result<usize> {
    tokio::time::timeout(timeout, socket.recv(buf))
        .await
        .map_err(|_| Error::Tracker("udp tracker timed out".into()))?
        .map_err(Error::Io)
}

pub fn encode_connect(transaction_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&UDP_PROTOCOL_ID.to_be_bytes());
    out.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
    out.extend_from_slice(&transaction_id.to_be_bytes());
    out
}

pub fn decode_connect(buf: &[u8], expected_txn: u32) -> Result<u64> {
    if buf.len() < 16 {
        return Err(Error::Tracker(format!(
            "connect reply was {} bytes, expected 16",
            buf.len()
        )));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let txn = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if txn != expected_txn {
        return Err(Error::Tracker(
            "connect reply had the wrong transaction id".into(),
        ));
    }
    if action == ACTION_ERROR {
        return Err(Error::Tracker(error_message(&buf[8..])));
    }
    if action != ACTION_CONNECT {
        return Err(Error::Tracker(format!(
            "unexpected connect action {action}"
        )));
    }
    Ok(u64::from_be_bytes(buf[8..16].try_into().unwrap()))
}

pub fn encode_announce(connection_id: u64, transaction_id: u32, req: &Announce) -> Vec<u8> {
    let mut out = Vec::with_capacity(98);
    out.extend_from_slice(&connection_id.to_be_bytes());
    out.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    out.extend_from_slice(&transaction_id.to_be_bytes());
    out.extend_from_slice(req.info_hash.as_bytes());
    out.extend_from_slice(&req.peer_id);
    out.extend_from_slice(&req.downloaded.to_be_bytes());
    out.extend_from_slice(&req.left.to_be_bytes());
    out.extend_from_slice(&req.uploaded.to_be_bytes());
    out.extend_from_slice(&req.event.as_u32().to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // ip: let the tracker see ours
    out.extend_from_slice(&rand::random::<u32>().to_be_bytes()); // key
    out.extend_from_slice(&req.num_want.to_be_bytes());
    out.extend_from_slice(&req.port.to_be_bytes());
    out
}

pub fn decode_announce(buf: &[u8], expected_txn: u32) -> Result<TrackerResponse> {
    if buf.len() < 8 {
        return Err(Error::Tracker(format!(
            "announce reply was {} bytes",
            buf.len()
        )));
    }
    let action = u32::from_be_bytes(buf[0..4].try_into().unwrap());
    let txn = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if txn != expected_txn {
        return Err(Error::Tracker(
            "announce reply had the wrong transaction id".into(),
        ));
    }
    if action == ACTION_ERROR {
        return Err(Error::Tracker(error_message(&buf[8..])));
    }
    if action != ACTION_ANNOUNCE || buf.len() < 20 {
        return Err(Error::Tracker(format!(
            "unexpected announce reply (action {action}, {} bytes)",
            buf.len()
        )));
    }

    Ok(TrackerResponse {
        interval: Some(u32::from_be_bytes(buf[8..12].try_into().unwrap())),
        leechers: Some(u32::from_be_bytes(buf[12..16].try_into().unwrap())),
        seeders: Some(u32::from_be_bytes(buf[16..20].try_into().unwrap())),
        peers: parse_compact_v4(&buf[20..])?,
    })
}

fn error_message(bytes: &[u8]) -> String {
    format!("tracker error: {}", String::from_utf8_lossy(bytes).trim())
}

// ------------------------------------------------------------------- helpers

/// BEP 23: four bytes of IPv4 then a big-endian port, repeated.
pub fn parse_compact_v4(bytes: &[u8]) -> Result<Vec<SocketAddr>> {
    if bytes.len() % 6 != 0 {
        return Err(Error::Tracker(format!(
            "compact peer list is {} bytes, not a multiple of 6",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(6)
        .map(|chunk| {
            let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            SocketAddr::V4(SocketAddrV4::new(ip, port))
        })
        // A peer on port 0 is not a peer.
        .filter(|addr| addr.port() != 0)
        .collect())
}

/// The IPv6 equivalent: 16 bytes of address then a port.
pub fn parse_compact_v6(bytes: &[u8]) -> Result<Vec<SocketAddr>> {
    if bytes.len() % 18 != 0 {
        return Err(Error::Tracker(format!(
            "compact ipv6 peer list is {} bytes, not a multiple of 18",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(18)
        .map(|chunk| {
            let octets: [u8; 16] = chunk[..16].try_into().unwrap();
            let port = u16::from_be_bytes([chunk[16], chunk[17]]);
            SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(octets), port, 0, 0))
        })
        .filter(|addr| addr.port() != 0)
        .collect())
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02x}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Announce {
        Announce {
            info_hash: InfoHash::parse("15c74d4165fc2ffff997d576bf44b4b25cbeb04e").unwrap(),
            peer_id: *b"-DP0001-abcdefghijkl",
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1234,
            event: Event::Started,
            num_want: 50,
        }
    }

    #[test]
    fn http_url_escapes_raw_bytes_not_hex() {
        let url = http_announce_url("http://bt1.archive.org:6969/announce", &request());
        assert!(
            url.contains("info_hash=%15%c7M%41e%fc%2f%ff%f9%97%d5v%bfD%b4%b2%5c%be%b0N")
                || url.contains("info_hash=%15%c7M"),
            "{url}"
        );
        assert!(
            !url.contains("info_hash=15c74d"),
            "must not send hex: {url}"
        );
        assert!(url.contains("peer_id=-DP0001-abcdefghijkl"), "{url}");
        assert!(url.contains("&compact=1"));
        assert!(url.contains("&event=started"));
        assert!(url.contains("&left=1234"));
    }

    #[test]
    fn http_url_respects_an_existing_query_string() {
        let url = http_announce_url("http://t.example/announce?passkey=abc", &request());
        assert!(url.contains("/announce?passkey=abc&info_hash="), "{url}");
    }

    #[test]
    fn parses_a_compact_http_response() {
        // peers = two compact entries: 1.2.3.4:6881 and 5.6.7.8:51413
        let mut body = b"d8:intervali1800e8:completei5e10:incompletei3e5:peers12:".to_vec();
        body.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1]);
        body.extend_from_slice(&[5, 6, 7, 8, 0xc8, 0xd5]);
        body.extend_from_slice(b"e");

        let resp = parse_http_response(&body).unwrap();
        assert_eq!(resp.interval, Some(1800));
        assert_eq!(resp.seeders, Some(5));
        assert_eq!(resp.leechers, Some(3));
        assert_eq!(
            resp.peers,
            vec![
                "1.2.3.4:6881".parse::<SocketAddr>().unwrap(),
                "5.6.7.8:51413".parse::<SocketAddr>().unwrap()
            ]
        );
    }

    #[test]
    fn parses_the_legacy_dict_peer_form() {
        let body = b"d5:peersld2:ip9:127.0.0.14:porti6881eeee".to_vec();
        let resp = parse_http_response(&body).unwrap();
        assert_eq!(
            resp.peers,
            vec!["127.0.0.1:6881".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn failure_reason_beats_everything_else() {
        let body = b"d14:failure reason28:Requested download is banneded";
        let err = parse_http_response(body).unwrap_err();
        assert!(format!("{err}").contains("banned"), "{err}");
    }

    #[test]
    fn rejects_a_ragged_compact_list() {
        let mut body = b"d5:peers5:".to_vec();
        body.extend_from_slice(&[1, 2, 3, 4, 5]);
        body.extend_from_slice(b"e");
        assert!(parse_http_response(&body).is_err());
    }

    #[test]
    fn drops_peers_on_port_zero() {
        let peers = parse_compact_v4(&[1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0x1a, 0xe1]).unwrap();
        assert_eq!(peers, vec!["5.6.7.8:6881".parse::<SocketAddr>().unwrap()]);
    }

    #[test]
    fn udp_connect_round_trips() {
        let packet = encode_connect(0xdead_beef);
        assert_eq!(packet.len(), 16);
        assert_eq!(
            u64::from_be_bytes(packet[0..8].try_into().unwrap()),
            UDP_PROTOCOL_ID
        );

        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        reply.extend_from_slice(&0xdead_beefu32.to_be_bytes());
        reply.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_be_bytes());
        assert_eq!(
            decode_connect(&reply, 0xdead_beef).unwrap(),
            0x0123_4567_89ab_cdef
        );
    }

    #[test]
    fn udp_replies_with_the_wrong_transaction_id_are_refused() {
        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_CONNECT.to_be_bytes());
        reply.extend_from_slice(&1u32.to_be_bytes());
        reply.extend_from_slice(&42u64.to_be_bytes());
        assert!(decode_connect(&reply, 2).is_err());
        assert!(decode_connect(&reply[..8], 1).is_err(), "truncated reply");
    }

    #[test]
    fn udp_announce_packet_has_the_right_shape() {
        let packet = encode_announce(0x0123_4567_89ab_cdef, 7, &request());
        assert_eq!(packet.len(), 98, "BEP 15 announce is exactly 98 bytes");
        assert_eq!(
            u32::from_be_bytes(packet[8..12].try_into().unwrap()),
            ACTION_ANNOUNCE
        );
        assert_eq!(&packet[16..36], request().info_hash.as_bytes());
        assert_eq!(&packet[36..56], &request().peer_id);
        assert_eq!(u64::from_be_bytes(packet[64..72].try_into().unwrap()), 1234);
        assert_eq!(
            u32::from_be_bytes(packet[80..84].try_into().unwrap()),
            Event::Started.as_u32()
        );
        assert_eq!(u16::from_be_bytes(packet[96..98].try_into().unwrap()), 6881);
    }

    #[test]
    fn udp_announce_reply_yields_peers() {
        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
        reply.extend_from_slice(&7u32.to_be_bytes());
        reply.extend_from_slice(&900u32.to_be_bytes()); // interval
        reply.extend_from_slice(&2u32.to_be_bytes()); // leechers
        reply.extend_from_slice(&9u32.to_be_bytes()); // seeders
        reply.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1]);

        let resp = decode_announce(&reply, 7).unwrap();
        assert_eq!(resp.interval, Some(900));
        assert_eq!(resp.seeders, Some(9));
        assert_eq!(resp.leechers, Some(2));
        assert_eq!(resp.peers.len(), 1);
    }

    #[test]
    fn udp_error_replies_are_surfaced() {
        let mut reply = Vec::new();
        reply.extend_from_slice(&ACTION_ERROR.to_be_bytes());
        reply.extend_from_slice(&7u32.to_be_bytes());
        reply.extend_from_slice(b"torrent not registered");
        let err = decode_announce(&reply, 7).unwrap_err();
        assert!(format!("{err}").contains("not registered"), "{err}");
    }

    #[tokio::test]
    async fn unsupported_schemes_are_rejected_without_a_socket() {
        let err = announce(
            "ftp://old.example/announce",
            &request(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("unsupported"), "{err}");
    }
}
