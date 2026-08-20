//! BEP 10 (extension protocol), BEP 9 (`ut_metadata`) and BEP 11 (`ut_pex`).
//!
//! The first two are what turn an infohash into a torrent. BEP 10 negotiates
//! which extensions both sides speak and, crucially, **which message id each
//! side wants them on**. The mapping is per-peer and per-direction: if a peer
//! says `{"ut_metadata": 3}`, we send it `ut_metadata` on id 3 while it sends
//! us the same extension on whatever id we advertised. Hardcoding 1 or 2 here
//! is the classic bug and works against roughly half the swarm.
//!
//! BEP 11 is how a swarm introduces its members to each other. It matters more
//! than its size suggests: on a public swarm most of what a tracker names is
//! unreachable, and the peer that will actually answer is frequently one only
//! another peer knows about.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde_bencode::value::Value as Bencode;

use crate::bencode;
use crate::error::{Error, Result};

/// The extended handshake always arrives on id 0.
pub const HANDSHAKE_ID: u8 = 0;

/// The ids *we* want extensions on. Peers will echo these back to us.
pub const OUR_UT_METADATA_ID: u8 = 1;
pub const OUR_UT_PEX_ID: u8 = 2;

/// BEP 9 transfers the info dict in 16 KiB blocks; all but the last are full.
pub const METADATA_BLOCK_SIZE: usize = 16 * 1024;

/// An info dict larger than this is not a torrent, it is an attack.
pub const MAX_METADATA_SIZE: usize = 16 * 1024 * 1024;

/// What a peer told us in its extended handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtendedHandshake {
    /// Extension name → the id *this peer* wants to receive it on.
    pub message_ids: HashMap<String, u8>,
    /// Total size of the info dictionary, if the peer knows it.
    pub metadata_size: Option<usize>,
    pub client: Option<String>,
    pub listen_port: Option<u16>,
    /// How many requests the peer is happy to have outstanding.
    pub request_queue: Option<usize>,
    pub yourip: Option<Vec<u8>>,
}

impl ExtendedHandshake {
    /// Our side of the handshake.
    pub fn ours(port: u16, metadata_size: Option<usize>) -> Self {
        let mut message_ids = HashMap::new();
        message_ids.insert("ut_metadata".to_string(), OUR_UT_METADATA_ID);
        message_ids.insert("ut_pex".to_string(), OUR_UT_PEX_ID);
        Self {
            message_ids,
            metadata_size,
            client: Some(concat!("balerion ", env!("CARGO_PKG_VERSION")).to_string()),
            listen_port: Some(port),
            request_queue: Some(250),
            yourip: None,
        }
    }

    /// The id to use when *sending* `ut_metadata` to this peer, if it wants it.
    pub fn ut_metadata_id(&self) -> Option<u8> {
        self.message_ids
            .get("ut_metadata")
            .copied()
            .filter(|id| *id != 0)
    }

    pub fn ut_pex_id(&self) -> Option<u8> {
        self.message_ids
            .get("ut_pex")
            .copied()
            .filter(|id| *id != 0)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut m: HashMap<Vec<u8>, Bencode> = HashMap::new();
        for (name, id) in &self.message_ids {
            m.insert(name.as_bytes().to_vec(), Bencode::Int(i64::from(*id)));
        }

        let mut root: HashMap<Vec<u8>, Bencode> = HashMap::new();
        root.insert(b"m".to_vec(), Bencode::Dict(m));
        if let Some(size) = self.metadata_size {
            root.insert(b"metadata_size".to_vec(), Bencode::Int(size as i64));
        }
        if let Some(client) = &self.client {
            root.insert(b"v".to_vec(), Bencode::Bytes(client.as_bytes().to_vec()));
        }
        if let Some(port) = self.listen_port {
            root.insert(b"p".to_vec(), Bencode::Int(i64::from(port)));
        }
        if let Some(reqq) = self.request_queue {
            root.insert(b"reqq".to_vec(), Bencode::Int(reqq as i64));
        }
        bencode::encode(&Bencode::Dict(root))
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let root = match serde_bencode::from_bytes::<Bencode>(payload) {
            Ok(Bencode::Dict(dict)) => dict,
            Ok(_) => return Err(Error::Peer("extended handshake is not a dict".into())),
            Err(err) => return Err(Error::Peer(format!("extended handshake: {err}"))),
        };

        let mut message_ids = HashMap::new();
        if let Some(m) = bencode::dict_dict(&root, b"m") {
            for (name, id) in m {
                if let Bencode::Int(id) = id
                    && (0..=255).contains(id)
                {
                    message_ids.insert(String::from_utf8_lossy(name).into_owned(), *id as u8);
                }
            }
        }

        Ok(Self {
            message_ids,
            metadata_size: bencode::dict_int(&root, b"metadata_size")
                .filter(|size| *size > 0 && *size as usize <= MAX_METADATA_SIZE)
                .map(|size| size as usize),
            client: bencode::dict_string(&root, b"v"),
            listen_port: bencode::dict_int(&root, b"p")
                .filter(|p| (1..=u16::MAX as i64).contains(p))
                .map(|p| p as u16),
            request_queue: bencode::dict_int(&root, b"reqq")
                .filter(|q| *q > 0)
                .map(|q| q as usize),
            yourip: bencode::dict_bytes(&root, b"yourip").map(<[u8]>::to_vec),
        })
    }
}

/// Most additions we will take from a single `ut_pex` message.
///
/// A well-behaved peer sends a handful every minute. A peer naming hundreds is
/// either broken or trying to fill our queue with addresses of its choosing,
/// and neither is worth the memory. The cap is generous enough that a busy
/// swarm's genuine introductions all survive it.
pub const MAX_PEX_ADDED: usize = 200;

/// A `ut_pex` message (BEP 11): who else is in this swarm.
///
/// Only the additions are read. `dropped` is advice about peers that have gone
/// away, and acting on it would mean hanging up on a connection that is working
/// because a third party said we should. The peer queue retires dead addresses
/// on its own evidence instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PexMessage {
    pub added: Vec<SocketAddr>,
}

impl PexMessage {
    /// Decode the additions from a `ut_pex` payload.
    ///
    /// Deliberately forgiving about a trailing partial entry, unlike the
    /// tracker parsers: a tracker sending a malformed list is a bug worth
    /// surfacing, whereas a peer doing it should cost us that one address
    /// rather than every address in the message. The whole point of the
    /// extension is to widen the search.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let root = match serde_bencode::from_bytes::<Bencode>(payload) {
            Ok(Bencode::Dict(dict)) => dict,
            Ok(_) => return Err(Error::Peer("ut_pex payload is not a dict".into())),
            Err(err) => return Err(Error::Peer(format!("ut_pex: {err}"))),
        };

        let mut added = Vec::new();
        if let Some(bytes) = bencode::dict_bytes(&root, b"added") {
            added.extend(compact_v4(bytes));
        }
        if let Some(bytes) = bencode::dict_bytes(&root, b"added6") {
            added.extend(compact_v6(bytes));
        }
        added.retain(dialable);
        added.truncate(MAX_PEX_ADDED);
        Ok(Self { added })
    }
}

/// Is this an address anyone could actually connect to?
///
/// Port zero and the unspecified address both turn up in real PEX messages,
/// from peers that have not worked out their own listening port yet. Dialling
/// them costs a connection slot and a timeout apiece.
fn dialable(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast(),
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

/// BEP 23 compact peers: four bytes of address, two of port, big endian.
fn compact_v4(bytes: &[u8]) -> impl Iterator<Item = SocketAddr> + '_ {
    bytes.chunks_exact(6).map(|entry| {
        let ip = Ipv4Addr::new(entry[0], entry[1], entry[2], entry[3]);
        SocketAddr::new(IpAddr::V4(ip), u16::from_be_bytes([entry[4], entry[5]]))
    })
}

/// The same shape with a sixteen byte address.
fn compact_v6(bytes: &[u8]) -> impl Iterator<Item = SocketAddr> + '_ {
    bytes.chunks_exact(18).map(|entry| {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&entry[..16]);
        SocketAddr::new(
            IpAddr::V6(Ipv6Addr::from(octets)),
            u16::from_be_bytes([entry[16], entry[17]]),
        )
    })
}

/// A `ut_metadata` message (BEP 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataMessage {
    Request {
        piece: usize,
    },
    Data {
        piece: usize,
        total_size: usize,
        block: Vec<u8>,
    },
    Reject {
        piece: usize,
    },
}

impl MetadataMessage {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut dict: HashMap<Vec<u8>, Bencode> = HashMap::new();
        let mut trailer: &[u8] = &[];
        match self {
            MetadataMessage::Request { piece } => {
                dict.insert(b"msg_type".to_vec(), Bencode::Int(0));
                dict.insert(b"piece".to_vec(), Bencode::Int(*piece as i64));
            }
            MetadataMessage::Data {
                piece,
                total_size,
                block,
            } => {
                dict.insert(b"msg_type".to_vec(), Bencode::Int(1));
                dict.insert(b"piece".to_vec(), Bencode::Int(*piece as i64));
                dict.insert(b"total_size".to_vec(), Bencode::Int(*total_size as i64));
                trailer = block;
            }
            MetadataMessage::Reject { piece } => {
                dict.insert(b"msg_type".to_vec(), Bencode::Int(2));
                dict.insert(b"piece".to_vec(), Bencode::Int(*piece as i64));
            }
        }
        let mut out = bencode::encode(&Bencode::Dict(dict))?;
        out.extend_from_slice(trailer);
        Ok(out)
    }

    /// Decode a `ut_metadata` payload.
    ///
    /// The raw block bytes of a `data` message follow the bencoded dict in the
    /// same payload rather than living inside it, so this has to know where
    /// the dict ended. That is what [`bencode::decode_prefix`] is for.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let (value, consumed) = bencode::decode_prefix(payload)?;
        let dict = match value {
            Bencode::Dict(dict) => dict,
            _ => return Err(Error::Peer("ut_metadata payload is not a dict".into())),
        };

        let piece = bencode::dict_int(&dict, b"piece")
            .filter(|p| *p >= 0)
            .ok_or_else(|| Error::Peer("ut_metadata message has no piece".into()))?
            as usize;

        match bencode::dict_int(&dict, b"msg_type") {
            Some(0) => Ok(MetadataMessage::Request { piece }),
            Some(1) => {
                let total_size = bencode::dict_int(&dict, b"total_size")
                    .filter(|size| *size > 0 && *size as usize <= MAX_METADATA_SIZE)
                    .ok_or_else(|| {
                        Error::Peer("ut_metadata data has no usable total_size".into())
                    })? as usize;
                Ok(MetadataMessage::Data {
                    piece,
                    total_size,
                    block: payload[consumed..].to_vec(),
                })
            }
            Some(2) => Ok(MetadataMessage::Reject { piece }),
            other => Err(Error::Peer(format!(
                "unknown ut_metadata msg_type {other:?}"
            ))),
        }
    }
}

/// Reassembles the info dictionary from 16 KiB blocks.
#[derive(Debug)]
pub struct MetadataAssembler {
    total_size: usize,
    blocks: Vec<Option<Vec<u8>>>,
}

impl MetadataAssembler {
    pub fn new(total_size: usize) -> Result<Self> {
        if total_size == 0 || total_size > MAX_METADATA_SIZE {
            return Err(Error::Peer(format!(
                "implausible metadata_size {total_size}"
            )));
        }
        Ok(Self {
            total_size,
            blocks: vec![None; total_size.div_ceil(METADATA_BLOCK_SIZE)],
        })
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn total_size(&self) -> usize {
        self.total_size
    }

    /// Expected length of a given block. Every block is full except the last.
    pub fn block_size(&self, piece: usize) -> Option<usize> {
        if piece >= self.blocks.len() {
            return None;
        }
        Some(if piece + 1 < self.blocks.len() {
            METADATA_BLOCK_SIZE
        } else {
            let remainder = self.total_size % METADATA_BLOCK_SIZE;
            if remainder == 0 {
                METADATA_BLOCK_SIZE
            } else {
                remainder
            }
        })
    }

    pub fn missing(&self) -> Vec<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.is_none())
            .map(|(index, _)| index)
            .collect()
    }

    pub fn have(&self, piece: usize) -> bool {
        self.blocks.get(piece).is_some_and(Option::is_some)
    }

    /// Store a block, rejecting wrong-sized ones so a peer cannot corrupt the
    /// dict by degrees.
    pub fn insert(&mut self, piece: usize, block: Vec<u8>) -> Result<()> {
        let expected = self
            .block_size(piece)
            .ok_or_else(|| Error::Peer(format!("metadata block {piece} is out of range")))?;
        if block.len() != expected {
            return Err(Error::Peer(format!(
                "metadata block {piece} was {} bytes, expected {expected}",
                block.len()
            )));
        }
        self.blocks[piece] = Some(block);
        Ok(())
    }

    pub fn is_complete(&self) -> bool {
        self.blocks.iter().all(Option::is_some)
    }

    /// The reassembled info dictionary, once every block has arrived. Still
    /// needs hashing against the infohash before you trust a byte of it.
    pub fn finish(self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut out = Vec::with_capacity(self.total_size);
        for block in self.blocks {
            out.extend_from_slice(&block?);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_handshake_round_trips() {
        let ours = ExtendedHandshake::ours(6881, Some(1234));
        let decoded = ExtendedHandshake::decode(&ours.encode().unwrap()).unwrap();

        assert_eq!(decoded.ut_metadata_id(), Some(OUR_UT_METADATA_ID));
        assert_eq!(decoded.ut_pex_id(), Some(OUR_UT_PEX_ID));
        assert_eq!(decoded.metadata_size, Some(1234));
        assert_eq!(decoded.listen_port, Some(6881));
        assert!(decoded.client.unwrap().starts_with("balerion"));
    }

    #[test]
    fn we_use_the_ids_the_peer_asked_for_not_our_own() {
        // A real peer that wants ut_metadata on 3 and ut_pex on 1.
        let payload = b"d1:md11:ut_metadatai3e6:ut_pexi1ee13:metadata_sizei31235e1:v11:qBittorrent1:pi51413ee";
        let peer = ExtendedHandshake::decode(payload).unwrap();

        assert_eq!(peer.ut_metadata_id(), Some(3));
        assert_eq!(peer.ut_pex_id(), Some(1));
        assert_ne!(
            peer.ut_metadata_id(),
            Some(OUR_UT_METADATA_ID),
            "the peer's id must not be assumed to match ours"
        );
        assert_eq!(peer.metadata_size, Some(31235));
        assert_eq!(peer.client.as_deref(), Some("qBittorrent"));
    }

    #[test]
    fn an_extension_id_of_zero_means_not_supported() {
        let peer = ExtendedHandshake::decode(b"d1:md11:ut_metadatai0eee").unwrap();
        assert_eq!(peer.ut_metadata_id(), None);
    }

    #[test]
    fn a_peer_with_no_extensions_is_handled() {
        let peer = ExtendedHandshake::decode(b"d1:mdee").unwrap();
        assert_eq!(peer.ut_metadata_id(), None);
        assert_eq!(peer.metadata_size, None);
    }

    #[test]
    fn absurd_metadata_sizes_are_ignored() {
        let payload = format!("d1:mde13:metadata_sizei{}ee", MAX_METADATA_SIZE + 1);
        assert_eq!(
            ExtendedHandshake::decode(payload.as_bytes())
                .unwrap()
                .metadata_size,
            None
        );
    }

    /// Bencode a `ut_pex` message the way a real client would.
    fn pex_payload(added: &[u8], added6: &[u8]) -> Vec<u8> {
        let mut out = b"d5:added".to_vec();
        out.extend(format!("{}:", added.len()).into_bytes());
        out.extend_from_slice(added);
        out.extend(b"6:added6");
        out.extend(format!("{}:", added6.len()).into_bytes());
        out.extend_from_slice(added6);
        out.extend(b"e");
        out
    }

    #[test]
    fn pex_reads_compact_ipv4_additions() {
        // 10.0.0.1:6881 and 192.168.1.2:51413.
        let added = [10, 0, 0, 1, 0x1a, 0xe1, 192, 168, 1, 2, 0xc8, 0xd5];
        let message = PexMessage::decode(&pex_payload(&added, &[])).unwrap();
        assert_eq!(
            message.added,
            vec![
                "10.0.0.1:6881".parse::<SocketAddr>().unwrap(),
                "192.168.1.2:51413".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn pex_reads_ipv6_additions_too() {
        let mut added6 = [0u8; 18];
        added6[15] = 1; // ::1
        added6[16..].copy_from_slice(&6881u16.to_be_bytes());
        let message = PexMessage::decode(&pex_payload(&[], &added6)).unwrap();
        assert_eq!(message.added, vec!["[::1]:6881".parse().unwrap()]);
    }

    #[test]
    fn pex_drops_addresses_nobody_could_dial() {
        // Port zero, then the unspecified address, then one good one.
        let added = [
            10, 0, 0, 1, 0, 0, // port 0
            0, 0, 0, 0, 0x1a, 0xe1, // 0.0.0.0
            10, 0, 0, 2, 0x1a, 0xe1, // fine
        ];
        let message = PexMessage::decode(&pex_payload(&added, &[])).unwrap();
        assert_eq!(message.added, vec!["10.0.0.2:6881".parse().unwrap()]);
    }

    #[test]
    fn a_trailing_partial_entry_costs_one_address_not_all_of_them() {
        // Strictness here would hand back nothing over one stray byte, which
        // defeats the point of an extension whose job is finding more peers.
        let added = [10, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0];
        let message = PexMessage::decode(&pex_payload(&added, &[])).unwrap();
        assert_eq!(message.added, vec!["10.0.0.1:6881".parse().unwrap()]);
    }

    #[test]
    fn pex_additions_are_capped() {
        let mut added = Vec::new();
        for index in 0..(MAX_PEX_ADDED as u32 + 50) {
            added.extend_from_slice(&index.to_be_bytes());
            added.extend_from_slice(&6881u16.to_be_bytes());
        }
        let message = PexMessage::decode(&pex_payload(&added, &[])).unwrap();
        assert_eq!(message.added.len(), MAX_PEX_ADDED);
    }

    #[test]
    fn a_pex_message_with_no_additions_is_not_an_error() {
        // Peers send these routinely when only `dropped` has changed.
        assert!(PexMessage::decode(b"de").unwrap().added.is_empty());
        assert!(PexMessage::decode(b"d5:added0:e").unwrap().added.is_empty());
    }

    #[test]
    fn malformed_pex_is_refused() {
        assert!(PexMessage::decode(b"").is_err());
        assert!(PexMessage::decode(b"i3e").is_err(), "not a dict");
    }

    #[test]
    fn metadata_messages_round_trip() {
        for message in [
            MetadataMessage::Request { piece: 2 },
            MetadataMessage::Reject { piece: 7 },
            MetadataMessage::Data {
                piece: 1,
                total_size: 40_000,
                block: b"some raw bytes".to_vec(),
            },
        ] {
            let encoded = message.encode().unwrap();
            assert_eq!(MetadataMessage::decode(&encoded).unwrap(), message);
        }
    }

    #[test]
    fn data_blocks_live_after_the_dict_not_inside_it() {
        let block = vec![0xABu8; 300];
        let encoded = MetadataMessage::Data {
            piece: 0,
            total_size: 300,
            block: block.clone(),
        }
        .encode()
        .unwrap();

        // The trailing raw bytes must be the tail of the payload.
        assert!(encoded.ends_with(&block));
        assert!(encoded.starts_with(b"d"));

        match MetadataMessage::decode(&encoded).unwrap() {
            MetadataMessage::Data { block: got, .. } => assert_eq!(got, block),
            other => panic!("expected data, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_metadata_messages() {
        assert!(MetadataMessage::decode(b"").is_err());
        assert!(MetadataMessage::decode(b"d8:msg_typei9e5:piecei0ee").is_err());
        assert!(
            MetadataMessage::decode(b"d8:msg_typei0ee").is_err(),
            "no piece"
        );
        assert!(
            MetadataMessage::decode(b"d8:msg_typei1e5:piecei0ee").is_err(),
            "data without total_size"
        );
    }

    #[test]
    fn assembler_knows_its_block_layout() {
        let assembler = MetadataAssembler::new(METADATA_BLOCK_SIZE + 100).unwrap();
        assert_eq!(assembler.block_count(), 2);
        assert_eq!(assembler.block_size(0), Some(METADATA_BLOCK_SIZE));
        assert_eq!(assembler.block_size(1), Some(100));
        assert_eq!(assembler.block_size(2), None);
        assert_eq!(assembler.missing(), vec![0, 1]);
    }

    #[test]
    fn an_exact_multiple_has_a_full_last_block() {
        let assembler = MetadataAssembler::new(METADATA_BLOCK_SIZE * 2).unwrap();
        assert_eq!(assembler.block_count(), 2);
        assert_eq!(assembler.block_size(1), Some(METADATA_BLOCK_SIZE));
    }

    #[test]
    fn assembles_blocks_in_any_order() {
        let mut assembler = MetadataAssembler::new(METADATA_BLOCK_SIZE + 5).unwrap();
        assembler.insert(1, b"tail!".to_vec()).unwrap();
        assert!(!assembler.is_complete());
        assert_eq!(assembler.missing(), vec![0]);
        assembler
            .insert(0, vec![b'x'; METADATA_BLOCK_SIZE])
            .unwrap();
        assert!(assembler.is_complete());

        let info = assembler.finish().unwrap();
        assert_eq!(info.len(), METADATA_BLOCK_SIZE + 5);
        assert!(info.ends_with(b"tail!"));
    }

    #[test]
    fn wrong_sized_blocks_are_refused() {
        let mut assembler = MetadataAssembler::new(METADATA_BLOCK_SIZE + 5).unwrap();
        assert!(assembler.insert(0, b"too short".to_vec()).is_err());
        assert!(assembler.insert(9, b"out of range".to_vec()).is_err());
        assert!(!assembler.have(0));
    }

    #[test]
    fn implausible_totals_are_refused_up_front() {
        assert!(MetadataAssembler::new(0).is_err());
        assert!(MetadataAssembler::new(MAX_METADATA_SIZE + 1).is_err());
    }

    #[test]
    fn an_incomplete_assembler_yields_nothing() {
        let assembler = MetadataAssembler::new(METADATA_BLOCK_SIZE * 2).unwrap();
        assert!(assembler.finish().is_none());
    }
}
