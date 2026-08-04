//! The peer wire protocol (BEP 3): handshake, message framing, and the codec
//! that turns a TCP stream into a stream of [`Message`]s.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::{Error, Result};
use crate::infohash::InfoHash;

pub const PROTOCOL: &[u8] = b"BitTorrent protocol";
pub const HANDSHAKE_LEN: usize = 68;

/// Blocks are 16 KiB by convention; every client in the wild assumes it.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Refuse absurd frames rather than allocating for them. The largest sane
/// message is a piece: 16 KiB of block plus nine bytes of header. We allow a
/// good deal more so an unusual-but-honest peer still works.
const MAX_MESSAGE_LEN: u32 = 1 << 20;

/// Reserved-byte bit for BEP 10 (extension protocol).
const RESERVED_EXTENDED: (usize, u8) = (5, 0x10);
/// Reserved-byte bit for BEP 5 (DHT).
const RESERVED_DHT: (usize, u8) = (7, 0x01);

/// The 68-byte opening handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub reserved: [u8; 8],
    pub info_hash: InfoHash,
    pub peer_id: [u8; 20],
}

impl Handshake {
    /// Our handshake, advertising BEP 10 and BEP 5 support.
    pub fn new(info_hash: InfoHash, peer_id: [u8; 20]) -> Self {
        let mut reserved = [0u8; 8];
        reserved[RESERVED_EXTENDED.0] |= RESERVED_EXTENDED.1;
        reserved[RESERVED_DHT.0] |= RESERVED_DHT.1;
        Self {
            reserved,
            info_hash,
            peer_id,
        }
    }

    pub fn supports_extended(&self) -> bool {
        self.reserved[RESERVED_EXTENDED.0] & RESERVED_EXTENDED.1 != 0
    }

    pub fn supports_dht(&self) -> bool {
        self.reserved[RESERVED_DHT.0] & RESERVED_DHT.1 != 0
    }

    pub fn encode(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        out[0] = PROTOCOL.len() as u8;
        out[1..20].copy_from_slice(PROTOCOL);
        out[20..28].copy_from_slice(&self.reserved);
        out[28..48].copy_from_slice(self.info_hash.as_bytes());
        out[48..68].copy_from_slice(&self.peer_id);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < HANDSHAKE_LEN {
            return Err(Error::Peer(format!(
                "handshake was {} bytes, expected {HANDSHAKE_LEN}",
                buf.len()
            )));
        }
        if buf[0] as usize != PROTOCOL.len() || &buf[1..20] != PROTOCOL {
            return Err(Error::Peer("not a BitTorrent handshake".into()));
        }
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&buf[20..28]);
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&buf[28..48]);
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&buf[48..68]);
        Ok(Self {
            reserved,
            info_hash: InfoHash::new(info_hash),
            peer_id,
        })
    }
}

/// A peer wire message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Bytes),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        block: Bytes,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// BEP 5: the peer's DHT port.
    Port(u16),
    /// BEP 10. `id` is the *peer's* chosen id for the extension, or 0 for the
    /// extended handshake itself.
    Extended {
        id: u8,
        payload: Bytes,
    },
    /// Something we do not implement. Skipped, not fatal.
    Unknown {
        id: u8,
        payload: Bytes,
    },
}

impl Message {
    pub fn id(&self) -> Option<u8> {
        Some(match self {
            Message::KeepAlive => return None,
            Message::Choke => 0,
            Message::Unchoke => 1,
            Message::Interested => 2,
            Message::NotInterested => 3,
            Message::Have(_) => 4,
            Message::Bitfield(_) => 5,
            Message::Request { .. } => 6,
            Message::Piece { .. } => 7,
            Message::Cancel { .. } => 8,
            Message::Port(_) => 9,
            Message::Extended { .. } => 20,
            Message::Unknown { id, .. } => *id,
        })
    }

    /// A short label for logs, so we do not print 16 KiB of block data.
    pub fn kind(&self) -> &'static str {
        match self {
            Message::KeepAlive => "keep-alive",
            Message::Choke => "choke",
            Message::Unchoke => "unchoke",
            Message::Interested => "interested",
            Message::NotInterested => "not-interested",
            Message::Have(_) => "have",
            Message::Bitfield(_) => "bitfield",
            Message::Request { .. } => "request",
            Message::Piece { .. } => "piece",
            Message::Cancel { .. } => "cancel",
            Message::Port(_) => "port",
            Message::Extended { .. } => "extended",
            Message::Unknown { .. } => "unknown",
        }
    }
}

/// Length-prefixed framing for [`Message`].
#[derive(Debug, Default, Clone, Copy)]
pub struct MessageCodec;

impl Decoder for MessageCodec {
    type Item = Message;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>> {
        if src.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes(src[..4].try_into().unwrap());
        if length > MAX_MESSAGE_LEN {
            return Err(Error::Peer(format!(
                "peer announced a {length}-byte message"
            )));
        }
        if src.len() < 4 + length as usize {
            // Ask for the rest rather than reallocating on every poll.
            src.reserve(4 + length as usize - src.len());
            return Ok(None);
        }

        src.advance(4);
        if length == 0 {
            return Ok(Some(Message::KeepAlive));
        }
        let mut payload = src.split_to(length as usize);
        let id = payload.get_u8();

        let message = match (id, payload.len()) {
            (0, 0) => Message::Choke,
            (1, 0) => Message::Unchoke,
            (2, 0) => Message::Interested,
            (3, 0) => Message::NotInterested,
            (4, 4) => Message::Have(payload.get_u32()),
            (5, _) => Message::Bitfield(payload.freeze()),
            (6, 12) => Message::Request {
                index: payload.get_u32(),
                begin: payload.get_u32(),
                length: payload.get_u32(),
            },
            (7, len) if len >= 8 => Message::Piece {
                index: payload.get_u32(),
                begin: payload.get_u32(),
                block: payload.freeze(),
            },
            (8, 12) => Message::Cancel {
                index: payload.get_u32(),
                begin: payload.get_u32(),
                length: payload.get_u32(),
            },
            (9, 2) => Message::Port(payload.get_u16()),
            (20, len) if len >= 1 => Message::Extended {
                id: payload.get_u8(),
                payload: payload.freeze(),
            },
            (id, len) => {
                tracing::trace!(id, len, "skipping unrecognised peer message");
                Message::Unknown {
                    id,
                    payload: payload.freeze(),
                }
            }
        };
        Ok(Some(message))
    }
}

impl Encoder<Message> for MessageCodec {
    type Error = Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<()> {
        let mut body = BytesMut::new();
        if let Some(id) = item.id() {
            body.put_u8(id);
        }
        match item {
            Message::KeepAlive
            | Message::Choke
            | Message::Unchoke
            | Message::Interested
            | Message::NotInterested => {}
            Message::Have(index) => body.put_u32(index),
            Message::Bitfield(bits) => body.put_slice(&bits),
            Message::Request {
                index,
                begin,
                length,
            }
            | Message::Cancel {
                index,
                begin,
                length,
            } => {
                body.put_u32(index);
                body.put_u32(begin);
                body.put_u32(length);
            }
            Message::Piece {
                index,
                begin,
                block,
            } => {
                body.put_u32(index);
                body.put_u32(begin);
                body.put_slice(&block);
            }
            Message::Port(port) => body.put_u16(port),
            // The extended id is chosen by the *receiving* peer, so it is part
            // of the payload rather than a constant.
            Message::Extended { id, payload } => {
                body.put_u8(id);
                body.put_slice(&payload);
            }
            Message::Unknown { payload, .. } => body.put_slice(&payload),
        }
        dst.put_u32(body.len() as u32);
        dst.put_slice(&body);
        Ok(())
    }
}

/// A bitfield of which pieces a peer has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitfield {
    bits: Vec<u8>,
    count: usize,
}

impl Bitfield {
    pub fn empty(count: usize) -> Self {
        Self {
            bits: vec![0; count.div_ceil(8)],
            count,
        }
    }

    /// Build from a peer's `bitfield` message, validating the length. A peer
    /// that sends a bitfield of the wrong size is either broken or hostile;
    /// either way we do not want it indexing past the end of our vector.
    pub fn from_bytes(bytes: &[u8], count: usize) -> Result<Self> {
        let expected = count.div_ceil(8);
        if bytes.len() != expected {
            return Err(Error::Peer(format!(
                "bitfield was {} bytes, expected {expected} for {count} pieces",
                bytes.len()
            )));
        }
        // Spare bits in the final byte must be zero.
        if count % 8 != 0 {
            let spare = 8 - (count % 8);
            if bytes[expected - 1] & ((1u8 << spare) - 1) != 0 {
                return Err(Error::Peer("bitfield has spare bits set".into()));
            }
        }
        Ok(Self {
            bits: bytes.to_vec(),
            count,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Bits are most-significant-first within each byte.
    pub fn has(&self, index: usize) -> bool {
        if index >= self.count {
            return false;
        }
        self.bits[index / 8] & (0b1000_0000 >> (index % 8)) != 0
    }

    pub fn set(&mut self, index: usize) {
        if index < self.count {
            self.bits[index / 8] |= 0b1000_0000 >> (index % 8);
        }
    }

    pub fn count_set(&self) -> usize {
        (0..self.count).filter(|i| self.has(*i)).count()
    }

    pub fn is_complete(&self) -> bool {
        self.count_set() == self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::codec::{Decoder, Encoder};

    fn hash() -> InfoHash {
        InfoHash::parse("15c74d4165fc2ffff997d576bf44b4b25cbeb04e").unwrap()
    }

    #[test]
    fn handshake_round_trips() {
        let handshake = Handshake::new(hash(), *b"-DP0001-abcdefghijkl");
        let bytes = handshake.encode();
        assert_eq!(bytes.len(), HANDSHAKE_LEN);
        assert_eq!(bytes[0], 19);

        let decoded = Handshake::decode(&bytes).unwrap();
        assert_eq!(decoded, handshake);
        assert!(decoded.supports_extended(), "we must advertise BEP 10");
        assert!(decoded.supports_dht());
    }

    #[test]
    fn handshake_rejects_other_protocols() {
        let mut bytes = Handshake::new(hash(), [0; 20]).encode().to_vec();
        bytes[1] = b'X';
        assert!(Handshake::decode(&bytes).is_err());
        assert!(Handshake::decode(&bytes[..40]).is_err());
    }

    #[test]
    fn a_peer_without_extensions_is_detected() {
        let mut bytes = Handshake::new(hash(), [0; 20]).encode();
        bytes[20..28].fill(0);
        let decoded = Handshake::decode(&bytes).unwrap();
        assert!(!decoded.supports_extended());
    }

    fn round_trip(message: Message) -> Message {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();
        codec.encode(message, &mut buf).unwrap();
        codec.decode(&mut buf).unwrap().expect("a whole message")
    }

    #[test]
    fn every_message_round_trips() {
        for message in [
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(42),
            Message::Bitfield(Bytes::from_static(&[0xff, 0x00])),
            Message::Request {
                index: 1,
                begin: 16384,
                length: 16384,
            },
            Message::Piece {
                index: 3,
                begin: 0,
                block: Bytes::from_static(b"hello"),
            },
            Message::Cancel {
                index: 1,
                begin: 0,
                length: 16384,
            },
            Message::Port(6881),
        ] {
            assert_eq!(round_trip(message.clone()), message, "{}", message.kind());
        }
    }

    #[test]
    fn extended_messages_carry_their_peer_chosen_id() {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();
        // Hand-frame an extended message with id 3, as a peer would.
        let payload = b"d1:mde";
        buf.put_u32(2 + payload.len() as u32);
        buf.put_u8(20);
        buf.put_u8(3);
        buf.put_slice(payload);

        match codec.decode(&mut buf).unwrap().unwrap() {
            Message::Extended { id, payload: body } => {
                assert_eq!(id, 3);
                assert_eq!(&body[..], payload);
            }
            other => panic!("expected extended, got {other:?}"),
        }
    }

    #[test]
    fn a_partial_message_yields_nothing_until_complete() {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();
        buf.put_u32(5);
        buf.put_u8(4);
        assert_eq!(codec.decode(&mut buf).unwrap(), None, "header only");
        buf.put_u32(7);
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Have(7)));
        assert!(buf.is_empty());
    }

    #[test]
    fn two_messages_in_one_read_both_decode() {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();
        codec.encode(Message::Unchoke, &mut buf).unwrap();
        codec.encode(Message::Have(9), &mut buf).unwrap();
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Unchoke));
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Have(9)));
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn absurd_lengths_are_refused_rather_than_allocated() {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();
        buf.put_u32(u32::MAX);
        buf.put_u8(5);
        assert!(codec.decode(&mut buf).is_err());
    }

    #[test]
    fn malformed_fixed_length_messages_become_unknown_not_panics() {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();
        buf.put_u32(3); // a `have` should be 5 bytes, not 3
        buf.put_u8(4);
        buf.put_u16(1);
        assert!(matches!(
            codec.decode(&mut buf).unwrap(),
            Some(Message::Unknown { id: 4, .. })
        ));
    }

    #[test]
    fn bitfield_bits_are_msb_first() {
        let field = Bitfield::from_bytes(&[0b1010_0000], 3).unwrap();
        assert!(field.has(0));
        assert!(!field.has(1));
        assert!(field.has(2));
        assert!(!field.has(3), "out of range is not held");
        assert_eq!(field.count_set(), 2);
        assert!(!field.is_complete());
    }

    #[test]
    fn bitfield_length_is_validated() {
        // 9 pieces needs 2 bytes.
        assert!(Bitfield::from_bytes(&[0xff], 9).is_err());
        assert!(Bitfield::from_bytes(&[0xff, 0x80], 9).is_ok());
        // Spare bits must be clear.
        assert!(Bitfield::from_bytes(&[0xff, 0xff], 9).is_err());
    }

    #[test]
    fn a_full_bitfield_is_complete() {
        let mut field = Bitfield::empty(10);
        assert!(!field.is_complete());
        for index in 0..10 {
            field.set(index);
        }
        assert!(field.is_complete());
        assert_eq!(field.count_set(), 10);
        // Setting past the end is a no-op, not a panic.
        field.set(99);
    }
}
