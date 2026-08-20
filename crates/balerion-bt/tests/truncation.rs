//! Nothing a stranger sends should be able to panic us.
//!
//! Everything balerion parses arrives from somewhere it does not control: a
//! peer's messages, a tracker's UDP replies, a torrent's own bytes. Each of
//! those decoders slices at fixed offsets and converts the slice into an array,
//! which is correct exactly as long as the length was checked first and is a
//! panic the moment it was not.
//!
//! An audit found every one of those sites guarded. This is that audit written
//! down so it stays true: every prefix of a valid message is fed to every
//! decoder, and the only acceptable outcomes are "parsed", "not yet" and "no".
//! A panic is none of those.
//!
//! Truncation rather than random bytes because it is the case that actually
//! occurs. A short read on a socket is ordinary; so is a resume file from a
//! process that was killed mid-write.

use balerion_bt::metainfo::Metainfo;
use balerion_bt::resume::ResumeState;
use balerion_bt::tracker;
use balerion_bt::wire::{Bitfield, Handshake, MessageCodec};
use bytes::BytesMut;
use tokio_util::codec::Decoder;

fn bstr(s: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", s.len()).into_bytes();
    out.extend_from_slice(s);
    out
}

fn info_dict() -> Vec<u8> {
    let mut info = Vec::new();
    info.extend(b"d");
    info.extend(bstr(b"length"));
    info.extend(b"i2000e");
    info.extend(bstr(b"name"));
    info.extend(bstr(b"a-file.bin"));
    info.extend(bstr(b"piece length"));
    info.extend(b"i1024e");
    info.extend(bstr(b"pieces"));
    info.extend(bstr(&[0xAAu8; 40]));
    info.extend(b"e");
    info
}

/// Feed every prefix of `whole` to `parse`, and require it not to panic.
fn every_prefix(what: &str, whole: &[u8], mut parse: impl FnMut(&[u8])) {
    for length in 0..=whole.len() {
        parse(&whole[..length]);
    }
    // And one past the end, in case a decoder is generous about trailing bytes.
    let mut extended = whole.to_vec();
    extended.extend_from_slice(&[0xFF; 8]);
    parse(&extended);
    println!("{what}: {} prefixes, no panic", whole.len() + 2);
}

#[test]
fn a_truncated_peer_message_is_incomplete_rather_than_fatal() {
    // The codec's job is to say "not yet" until the whole frame is there, and
    // it reads a four byte length before it can know how long that is.
    let mut whole = Vec::new();
    whole.extend_from_slice(&9u32.to_be_bytes());
    whole.push(7); // piece
    whole.extend_from_slice(&0u32.to_be_bytes()); // index
    whole.extend_from_slice(&0u32.to_be_bytes()); // begin

    every_prefix("peer message", &whole, |prefix| {
        let mut buffer = BytesMut::from(prefix);
        // Any answer will do; not returning is the only failure.
        let _ = MessageCodec.decode(&mut buffer);
    });
}

#[test]
fn a_peer_announcing_an_absurd_message_length_is_refused() {
    // The other half of the same guard: a length that would allocate the
    // machine's memory has to be rejected rather than reserved.
    let mut buffer = BytesMut::from(&u32::MAX.to_be_bytes()[..]);
    buffer.extend_from_slice(&[0u8; 4]);
    assert!(
        MessageCodec.decode(&mut buffer).is_err(),
        "a four gigabyte message must be refused"
    );
}

#[test]
fn a_truncated_handshake_is_refused_rather_than_fatal() {
    let whole = Handshake::new(balerion_bt::InfoHash::new([1u8; 20]), [2u8; 20]).encode();
    every_prefix("handshake", &whole, |prefix| {
        let _ = Handshake::decode(prefix);
    });
}

#[test]
fn a_truncated_udp_tracker_reply_is_refused_rather_than_fatal() {
    // Both shapes, because they check different lengths at different points
    // and the announce one reads three more fields after its second guard.
    let mut connect = Vec::new();
    connect.extend_from_slice(&0u32.to_be_bytes());
    connect.extend_from_slice(&1234u32.to_be_bytes());
    connect.extend_from_slice(&0xDEADBEEFu64.to_be_bytes());
    every_prefix("udp connect", &connect, |prefix| {
        let _ = tracker::decode_connect(prefix, 1234);
    });

    let mut announce = Vec::new();
    announce.extend_from_slice(&1u32.to_be_bytes());
    announce.extend_from_slice(&1234u32.to_be_bytes());
    announce.extend_from_slice(&1800u32.to_be_bytes());
    announce.extend_from_slice(&3u32.to_be_bytes());
    announce.extend_from_slice(&5u32.to_be_bytes());
    announce.extend_from_slice(&[10, 0, 0, 1, 0x1a, 0xe1]);
    every_prefix("udp announce", &announce, |prefix| {
        let _ = tracker::decode_announce(prefix, 1234);
    });
}

#[test]
fn a_truncated_compact_peer_list_is_refused_rather_than_fatal() {
    let v4 = [10u8, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 2, 0x1a, 0xe1];
    every_prefix("compact v4", &v4, |prefix| {
        let _ = tracker::parse_compact_v4(prefix);
    });

    let mut v6 = [0u8; 36];
    v6[15] = 1;
    v6[16..18].copy_from_slice(&6881u16.to_be_bytes());
    every_prefix("compact v6", &v6, |prefix| {
        let _ = tracker::parse_compact_v6(prefix);
    });
}

#[test]
fn a_truncated_resume_file_is_refused_rather_than_fatal() {
    // Written by a process that was killed mid-write, which is precisely the
    // situation the resume file exists to survive.
    let whole = ResumeState {
        info_hash: balerion_bt::InfoHash::new([3u8; 20]),
        piece_length: 1024,
        total_length: 2000,
        have: Bitfield::empty(2),
        clean: true,
    }
    .encode();

    every_prefix("resume file", &whole, |prefix| {
        let _ = ResumeState::decode(prefix);
    });
}

#[test]
fn a_truncated_info_dict_is_refused_rather_than_fatal() {
    // This one arrives from a peer over BEP 9, so every prefix of it is a
    // thing some peer can send us on purpose.
    let whole = info_dict();
    every_prefix("info dict", &whole, |prefix| {
        let _ = Metainfo::from_info_dict(prefix);
    });
}

#[test]
fn a_truncated_torrent_file_is_refused_rather_than_fatal() {
    let info = info_dict();
    let mut whole = Vec::new();
    whole.extend(b"d");
    whole.extend(bstr(b"announce"));
    whole.extend(bstr(b"http://tracker.example/announce"));
    whole.extend(bstr(b"info"));
    whole.extend(&info);
    whole.extend(b"e");

    every_prefix("torrent file", &whole, |prefix| {
        let _ = Metainfo::parse(prefix);
    });
}

#[test]
fn a_truncated_magnet_is_refused_rather_than_fatal() {
    let whole = b"magnet:?xt=urn:btih:30f15834bd5cb994bec71635455691acd64875e4\
                  &dn=A%20Film&tr=udp%3A%2F%2Ftracker.example%3A1337%2Fannounce";
    every_prefix("magnet", whole, |prefix| {
        // Not UTF-8-clean at every boundary, which is itself worth not
        // panicking over.
        let text = String::from_utf8_lossy(prefix);
        let _ = balerion_bt::Magnet::parse(&text);
    });
}
