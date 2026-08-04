//! The 20-byte content identifier, and nothing else.
//!
//! Hex strings are for humans and URLs. Everything inside the engine passes
//! [`InfoHash`] so a 40-character string and a 20-byte array can never be
//! confused for one another.

use std::fmt;

use data_encoding::BASE32;

use crate::error::{Error, Result};

/// SHA-1 of a torrent's info dictionary: the swarm identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InfoHash([u8; 20]);

impl InfoHash {
    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Percent-encoded raw bytes, as trackers want them in `info_hash=`.
    /// Note this is the *bytes*, not the hex: a classic tracker bug.
    pub fn to_query_escaped(&self) -> String {
        let mut out = String::with_capacity(60);
        for byte in self.0 {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02x}")),
            }
        }
        out
    }

    /// Parse the 40-character hex or 32-character base32 form. Both are legal
    /// in magnet links and both decode to the same 20 bytes; base32 magnets
    /// are rarer and a reliable source of parser bugs.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        match text.len() {
            40 => {
                let bytes = hex::decode(text)
                    .map_err(|err| Error::Magnet(format!("bad hex infohash: {err}")))?;
                Ok(Self(bytes.try_into().expect("40 hex chars is 20 bytes")))
            }
            32 => {
                let bytes = BASE32
                    .decode(text.to_ascii_uppercase().as_bytes())
                    .map_err(|err| Error::Magnet(format!("bad base32 infohash: {err}")))?;
                let bytes: [u8; 20] = bytes
                    .try_into()
                    .map_err(|_| Error::Magnet("base32 infohash was not 20 bytes".into()))?;
                Ok(Self(bytes))
            }
            other => Err(Error::Magnet(format!(
                "infohash must be 40 hex or 32 base32 characters, got {other}"
            ))),
        }
    }
}

impl From<[u8; 20]> for InfoHash {
    fn from(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHash({})", self.to_hex())
    }
}

/// Our own 20-byte peer id, Azureus style: `-DP0001-` plus 12 random bytes.
pub fn generate_peer_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(b"-DP0001-");
    for byte in &mut id[8..] {
        *byte = rand::random::<u8>();
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "15c74d4165fc2ffff997d576bf44b4b25cbeb04e";

    #[test]
    fn parses_hex() {
        let hash = InfoHash::parse(HEX).unwrap();
        assert_eq!(hash.to_hex(), HEX);
        assert_eq!(hash.as_bytes()[0], 0x15);
    }

    #[test]
    fn parses_base32_to_the_same_bytes() {
        let from_hex = InfoHash::parse(HEX).unwrap();
        let base32 = BASE32.encode(from_hex.as_bytes());
        assert_eq!(base32.trim_end_matches('=').len(), 32);
        let from_base32 = InfoHash::parse(base32.trim_end_matches('=')).unwrap();
        assert_eq!(from_hex, from_base32);
    }

    #[test]
    fn accepts_mixed_case() {
        assert_eq!(
            InfoHash::parse(&HEX.to_uppercase()).unwrap(),
            InfoHash::parse(HEX).unwrap()
        );
    }

    #[test]
    fn rejects_nonsense() {
        assert!(InfoHash::parse("").is_err());
        assert!(InfoHash::parse("cafe").is_err());
        assert!(InfoHash::parse(&"z".repeat(40)).is_err());
    }

    #[test]
    fn escapes_raw_bytes_for_trackers() {
        let hash = InfoHash::new([
            0x00, 0x41, 0xff, 0x2d, 0x7e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let escaped = hash.to_query_escaped();
        assert!(escaped.starts_with("%00A%ff-~"), "{escaped}");
        // 15 zero bytes at three characters each, plus the five above.
        assert_eq!(escaped.len(), 45 + 9);
    }

    #[test]
    fn peer_ids_are_prefixed_and_unique() {
        let a = generate_peer_id();
        let b = generate_peer_id();
        assert_eq!(&a[..8], b"-DP0001-");
        assert_ne!(a, b, "peer ids should not repeat");
    }
}
