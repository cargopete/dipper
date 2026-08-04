//! Just enough bencode to be safe with untrusted input.
//!
//! Two jobs the typed serde path cannot do:
//!
//! 1. **Byte spans.** The infohash is SHA-1 over the `info` dictionary exactly
//!    as it appeared on the wire. Re-encoding a parsed struct drops unknown
//!    keys and can reorder others, which silently produces the wrong hash.
//! 2. **Consumed length.** BEP 9 `ut_metadata` messages put raw block bytes
//!    immediately *after* a bencoded dict in the same payload, so the decoder
//!    has to report where the dict ended.

use std::collections::HashMap;
use std::ops::Range;

use serde_bencode::value::Value as Bencode;

use crate::error::{Error, Result};

/// Deepest nesting we will follow before assuming malice.
const MAX_DEPTH: usize = 32;

/// Scan one bencoded value starting at `pos`; returns the index just past it.
pub fn scan(raw: &[u8], pos: usize) -> Result<usize> {
    scan_at(raw, pos, 0)
}

fn scan_at(raw: &[u8], pos: usize, depth: usize) -> Result<usize> {
    if depth > MAX_DEPTH {
        return Err(Error::Bencode("nested too deeply".into()));
    }
    match raw.get(pos) {
        Some(b'i') => Ok(find(raw, pos + 1, b'e')? + 1),
        Some(b'l') | Some(b'd') => {
            let mut cursor = pos + 1;
            loop {
                match raw.get(cursor) {
                    Some(b'e') => return Ok(cursor + 1),
                    Some(_) => cursor = scan_at(raw, cursor, depth + 1)?,
                    None => return Err(Error::Bencode("truncated container".into())),
                }
            }
        }
        Some(c) if c.is_ascii_digit() => {
            let colon = find(raw, pos, b':')?;
            let len: usize = std::str::from_utf8(&raw[pos..colon])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Error::Bencode("bad string length".into()))?;
            colon
                .checked_add(1 + len)
                .filter(|end| *end <= raw.len())
                .ok_or_else(|| Error::Bencode("string runs past end of input".into()))
        }
        Some(c) => Err(Error::Bencode(format!(
            "unexpected byte {:?} at offset {pos}",
            *c as char
        ))),
        None => Err(Error::Bencode("truncated input".into())),
    }
}

fn find(raw: &[u8], from: usize, needle: u8) -> Result<usize> {
    raw[from.min(raw.len())..]
        .iter()
        .position(|b| *b == needle)
        .map(|offset| from + offset)
        .ok_or_else(|| Error::Bencode("truncated input".into()))
}

/// Decode the bencoded value at the start of `raw`, ignoring anything after
/// it, and report how many bytes it consumed. This is what BEP 9 needs.
pub fn decode_prefix(raw: &[u8]) -> Result<(Bencode, usize)> {
    let end = scan(raw, 0)?;
    let value = serde_bencode::from_bytes::<Bencode>(&raw[..end])
        .map_err(|err| Error::Bencode(err.to_string()))?;
    Ok((value, end))
}

/// Byte range of the `info` dictionary's value within a raw metainfo file.
pub fn info_span(raw: &[u8]) -> Result<Range<usize>> {
    if raw.first() != Some(&b'd') {
        return Err(Error::Bencode("metainfo does not start with a dict".into()));
    }
    let mut pos = 1;
    while pos < raw.len() && raw[pos] != b'e' {
        let key_end = scan(raw, pos)?;
        let key = &raw[pos..key_end];
        let value_end = scan(raw, key_end)?;
        // Keys are bencoded strings, so `info` arrives as `4:info`.
        if key.strip_prefix(b"4:") == Some(b"info") {
            return Ok(key_end..value_end);
        }
        pos = value_end;
    }
    Err(Error::Bencode("no info dictionary".into()))
}

pub type Dict = HashMap<Vec<u8>, Bencode>;

/// A bencoded string value, lossily as UTF-8.
pub fn dict_string(dict: &Dict, key: &[u8]) -> Option<String> {
    match dict.get(key)? {
        Bencode::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

pub fn dict_bytes<'a>(dict: &'a Dict, key: &[u8]) -> Option<&'a [u8]> {
    match dict.get(key)? {
        Bencode::Bytes(bytes) => Some(bytes),
        _ => None,
    }
}

pub fn dict_int(dict: &Dict, key: &[u8]) -> Option<i64> {
    match dict.get(key)? {
        Bencode::Int(n) => Some(*n),
        _ => None,
    }
}

pub fn dict_dict<'a>(dict: &'a Dict, key: &[u8]) -> Option<&'a Dict> {
    match dict.get(key)? {
        Bencode::Dict(inner) => Some(inner),
        _ => None,
    }
}

/// Encode a flat dict of string keys to bencode. Enough for the small control
/// messages we send (BEP 9 requests, BEP 10 handshakes).
pub fn encode(value: &Bencode) -> Result<Vec<u8>> {
    serde_bencode::to_bytes(value).map_err(|err| Error::Bencode(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s);
        out
    }

    #[test]
    fn scans_every_kind_of_value() {
        assert_eq!(scan(b"i42e", 0).unwrap(), 4);
        assert_eq!(scan(b"4:spam", 0).unwrap(), 6);
        assert_eq!(scan(b"le", 0).unwrap(), 2);
        assert_eq!(scan(b"l4:spami3ee", 0).unwrap(), 11);
        assert_eq!(scan(b"d3:onei1e3:twoi2ee", 0).unwrap(), 18);
        assert_eq!(scan(b"0:", 0).unwrap(), 2);
    }

    #[test]
    fn rejects_hostile_input() {
        assert!(scan(b"", 0).is_err());
        assert!(scan(b"i42", 0).is_err());
        assert!(scan(b"99999:short", 0).is_err());
        assert!(scan(b"d", 0).is_err());
        assert!(scan(b"x", 0).is_err());
        // A deeply nested list should be refused, not blow the stack.
        let bomb: Vec<u8> = std::iter::repeat_n(b'l', 500).collect();
        assert!(scan(&bomb, 0).is_err());
    }

    #[test]
    fn decode_prefix_reports_consumed_length() {
        // A BEP 9 data message: dict, then raw block bytes.
        let mut payload = b"d8:msg_typei1e5:piecei0e10:total_sizei1234ee".to_vec();
        let dict_len = payload.len();
        payload.extend_from_slice(b"\x00\x01\x02raw block bytes");

        let (value, consumed) = decode_prefix(&payload).unwrap();
        assert_eq!(consumed, dict_len);
        assert_eq!(&payload[consumed..], b"\x00\x01\x02raw block bytes");
        match value {
            Bencode::Dict(dict) => {
                assert_eq!(dict_int(&dict, b"msg_type"), Some(1));
                assert_eq!(dict_int(&dict, b"total_size"), Some(1234));
            }
            other => panic!("expected a dict, got {other:?}"),
        }
    }

    #[test]
    fn finds_the_info_span_exactly() {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"thing"));
        info.extend(b"e");

        let mut raw = Vec::new();
        raw.extend(b"d");
        raw.extend(bstr(b"announce"));
        raw.extend(bstr(b"http://tracker/announce"));
        raw.extend(bstr(b"info"));
        raw.extend(&info);
        raw.extend(b"e");

        let span = info_span(&raw).unwrap();
        assert_eq!(&raw[span], info.as_slice());
    }

    #[test]
    fn info_span_is_not_fooled_by_a_key_called_info_elsewhere() {
        let mut raw = Vec::new();
        raw.extend(b"d");
        // A *value* of "info" under another key must not be mistaken for it.
        raw.extend(bstr(b"comment"));
        raw.extend(bstr(b"info"));
        raw.extend(bstr(b"info"));
        raw.extend(b"d");
        raw.extend(bstr(b"name"));
        raw.extend(bstr(b"real"));
        raw.extend(b"e");
        raw.extend(b"e");

        let span = info_span(&raw).unwrap();
        assert_eq!(&raw[span], b"d4:name4:reale");
    }
}
