//! The obfuscated handshake, against something that answers it.
//!
//! The unit tests in `mse` check RC4 against published vectors and check that
//! the two halves of our own key exchange agree. Neither says anything about
//! the *conversation*: whether the bytes go out in the order the specification
//! wants, whether the marker can be found in a stream with padding either side
//! of it, and whether the cipher is positioned where the BitTorrent handshake
//! expects to start.
//!
//! So here is the other side of it, written from the specification rather than
//! from the implementation under test. That is still one reading of one
//! document on both sides and it is not the same as talking to qBittorrent, but
//! it catches every structural way this can be wrong.
//!
//! The peer here **refuses plaintext**, which is the whole reason the feature
//! exists: it is what a client configured for encryption-only looks like from
//! the outside.

use std::net::SocketAddr;

use balerion_bt::InfoHash;
use balerion_bt::mse::{KeyExchange, Rc4, req1, req2_xor_req3, stream_keys};
use balerion_bt::peer::{PeerConnection, Timeouts};
use balerion_bt::wire::{HANDSHAKE_LEN, Handshake};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const KEY_LEN: usize = 96;

/// A peer that will only speak obfuscated, and hangs up on anything else.
///
/// `plaintext_first` mirrors what actually happens on a swarm: the first
/// connection is our plaintext attempt, which it drops, and the second is the
/// one it answers.
async fn spawn_encrypted_peer(info_hash: InfoHash) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };

            // A plaintext BitTorrent handshake begins with 0x13. Anything that
            // does is refused, exactly as a peer configured for encryption
            // only would refuse it.
            let mut first = [0u8; 1];
            if stream.read_exact(&mut first).await.is_err() {
                continue;
            }
            if first[0] == 19 {
                drop(stream);
                continue;
            }

            if let Err(err) = answer(stream, first[0], info_hash).await {
                eprintln!("fixture peer gave up: {err}");
            }
        }
    });

    addr
}

/// The receiving half of MSE, from the specification.
async fn answer(mut stream: TcpStream, first_byte: u8, info_hash: InfoHash) -> std::io::Result<()> {
    // Their public value: one byte of it has already been read.
    let mut theirs = [0u8; KEY_LEN];
    theirs[0] = first_byte;
    stream.read_exact(&mut theirs[1..]).await?;

    // Ours, with no padding, which is allowed and makes the fixture simpler.
    let keys = KeyExchange::from_secret(&[42u8; 20]);
    stream.write_all(&keys.public).await?;

    let shared = keys.shared(&theirs);
    // The dialling side's send key is our receive key.
    let (their_send, our_send) = stream_keys(&shared, info_hash);
    let mut receive = their_send;
    let mut send = our_send;

    // Scan the stream for the marker, which skips whatever padding they sent.
    // Byte at a time, because the padding has no announced length.
    let marker = req1(&shared);
    let mut window = [0u8; 20];
    let mut seen = 0usize;
    loop {
        if seen > 1024 {
            return Err(std::io::Error::other("no marker"));
        }
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        seen += 1;
        window.rotate_left(1);
        window[19] = byte[0];
        if seen >= 20 && window == marker {
            break;
        }
    }

    // The obfuscated infohash, which we can only check because we know which
    // torrent we are serving.
    let mut obfuscated = [0u8; 20];
    stream.read_exact(&mut obfuscated).await?;
    assert_eq!(
        obfuscated,
        req2_xor_req3(&shared, info_hash),
        "the dialling side named a different torrent"
    );

    // From here everything is enciphered.
    let mut header = [0u8; 14];
    stream.read_exact(&mut header).await?;
    receive.apply(&mut header);
    assert_eq!(&header[..8], &[0u8; 8], "their verification constant");
    let provided = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    assert!(provided & 0x02 != 0, "they must offer RC4");

    let pad_c = u16::from_be_bytes([header[12], header[13]]) as usize;
    if pad_c > 0 {
        let mut pad = vec![0u8; pad_c];
        stream.read_exact(&mut pad).await?;
        receive.apply(&mut pad);
    }
    let mut ia_len = [0u8; 2];
    stream.read_exact(&mut ia_len).await?;
    receive.apply(&mut ia_len);
    let ia = u16::from_be_bytes(ia_len) as usize;
    if ia > 0 {
        let mut payload = vec![0u8; ia];
        stream.read_exact(&mut payload).await?;
        receive.apply(&mut payload);
    }

    // Our answer: the constant, what we chose, and no padding.
    let mut reply = Vec::new();
    reply.extend_from_slice(&[0u8; 8]);
    reply.extend_from_slice(&0x02u32.to_be_bytes());
    reply.extend_from_slice(&0u16.to_be_bytes());
    send.apply(&mut reply);
    stream.write_all(&reply).await?;

    // And now ordinary BitTorrent, over the cipher.
    serve_handshake(stream, send, receive, info_hash).await
}

/// A plain BEP 3 handshake, enciphered.
async fn serve_handshake(
    mut stream: TcpStream,
    mut send: Rc4,
    mut receive: Rc4,
    info_hash: InfoHash,
) -> std::io::Result<()> {
    let mut theirs = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut theirs).await?;
    receive.apply(&mut theirs);
    let decoded = Handshake::decode(&theirs).expect("a handshake, decrypted");
    assert_eq!(decoded.info_hash, info_hash);

    let mut ours = Handshake {
        // No extended bit: this fixture is testing the cipher, not BEP 10.
        reserved: [0u8; 8],
        info_hash,
        peer_id: *b"-FIXTURE-00000000000",
    }
    .encode();
    send.apply(&mut ours);
    stream.write_all(&ours).await?;

    // Sit still so the connection stays up for the assertions.
    let mut sink = [0u8; 64];
    let _ = stream.read(&mut sink).await;
    Ok(())
}

#[tokio::test]
async fn a_peer_that_refuses_plaintext_is_reached_over_an_obfuscated_stream() {
    let info_hash = InfoHash::new([0x5Au8; 20]);
    let addr = spawn_encrypted_peer(info_hash).await;

    let peer = PeerConnection::connect(addr, info_hash, [1u8; 20], 6881, Timeouts::default())
        .await
        .expect("the fallback should reach this peer");

    assert_eq!(peer.handshake.info_hash, info_hash);
    assert_eq!(&peer.handshake.peer_id, b"-FIXTURE-00000000000");
}

#[tokio::test]
async fn with_encryption_off_the_same_peer_is_unreachable() {
    // The control. Without this the test above could be passing because the
    // fixture accepts plaintext after all.
    let info_hash = InfoHash::new([0x5Bu8; 20]);
    let addr = spawn_encrypted_peer(info_hash).await;

    let timeouts = Timeouts {
        encrypt: false,
        ..Default::default()
    };
    assert!(
        PeerConnection::connect(addr, info_hash, [1u8; 20], 6881, timeouts)
            .await
            .is_err(),
        "this peer speaks nothing but MSE"
    );
}

#[tokio::test]
async fn a_peer_that_is_simply_absent_is_not_dialled_twice_for_nothing() {
    // Port 1 on loopback refuses rather than accepting, which is a different
    // thing from refusing our handshake and must not earn a second attempt.
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let started = std::time::Instant::now();

    let err = PeerConnection::connect(
        addr,
        InfoHash::new([1u8; 20]),
        [1u8; 20],
        6881,
        Timeouts::default(),
    )
    .await
    .unwrap_err();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a refused connection should fail at once, not after two handshakes: {err}"
    );
}
