//! Message Stream Encryption (MSE/PE).
//!
//! Not a security feature, and it is worth saying so before anything else. The
//! key exchange is unauthenticated, so anyone who can sit between two peers can
//! read everything; the cipher is RC4, which has been broken for years. Nobody
//! in this protocol believes otherwise.
//!
//! It is here because it is what everybody else speaks. A meaningful share of
//! public-swarm peers are configured to require an obfuscated connection and
//! will refuse a plaintext one outright, and some consumer ISPs still shape or
//! block plaintext BitTorrent. Without this, those peers are simply invisible,
//! which on the thin swarms balerion spends its time on is the difference
//! between a magnet resolving and not.
//!
//! The shape of it, since the specification is scattered across a wiki:
//!
//! ```text
//! A -> B: Ya, PadA                       Diffie-Hellman, 96 bytes plus padding
//! B -> A: Yb, PadB
//!         both compute S = the shared secret
//! A -> B: HASH('req1', S)                 the marker B scans for
//!         HASH('req2', SKEY) xor HASH('req3', S)   proves A knows the infohash
//!         ENCRYPT(VC, crypto_provide, len(PadC), PadC, len(IA))
//!         ENCRYPT(IA)
//! B -> A: ENCRYPT(VC, crypto_select, len(PadD), PadD)
//! ```
//!
//! `SKEY` is the infohash, which is what stops this being a way to talk to a
//! swarm you cannot name: B can only find the marker if A already knew what it
//! was asking for.
//!
//! The primitives are separated from the conversation on purpose. RC4 and the
//! key derivation have published test vectors and can be checked against
//! somebody else's arithmetic rather than against our own.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use num_bigint::BigUint;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// The 768-bit prime every BitTorrent client uses for this. From the
/// specification, and identical everywhere: it is a handshake, not a secret.
const PRIME: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E08\
                     8A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B\
                     302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9\
                     A63A36210000000000090563";

/// Both sides use 2. Also from the specification.
const GENERATOR: u32 = 2;

/// Public keys are exactly this long, left-padded with zeroes.
const KEY_LEN: usize = 96;

/// Most padding either side may send. The specification's figure.
const MAX_PAD: usize = 512;

/// The eight zero bytes that tell the other side decryption is working.
const VERIFICATION: [u8; 8] = [0; 8];

/// What we are willing to speak, as the `crypto_provide` bitfield.
///
/// RC4 only. Plaintext-after-handshake is in the specification and is what a
/// peer chooses when it wants the obfuscated *handshake* without paying for the
/// stream cipher, but offering it means implementing a third mode for no gain:
/// the reason to be here at all is reaching peers that insist on encryption,
/// and those peers insist on the cipher too.
const CRYPTO_RC4: u32 = 0x02;

/// How far to read looking for the other side's marker before giving up.
///
/// The padding is bounded at 512 bytes, so a marker further in than this is not
/// a slow peer, it is not a peer.
const MAX_SEARCH: usize = MAX_PAD + 96 + 64;

/// RC4, with BitTorrent's discard.
///
/// The first 1024 bytes of an RC4 stream are famously biased, which is the
/// weakness that broke it in WEP. Every implementation of this protocol throws
/// them away, so a stream that does not is not merely weaker, it is
/// incompatible.
#[derive(Clone)]
pub struct Rc4 {
    state: [u8; 256],
    i: u8,
    j: u8,
}

impl std::fmt::Debug for Rc4 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the state: it is the key.
        f.write_str("Rc4")
    }
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Self {
        let mut state = [0u8; 256];
        for (index, slot) in state.iter_mut().enumerate() {
            *slot = index as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j
                .wrapping_add(state[i])
                .wrapping_add(key[i % key.len().max(1)]);
            state.swap(i, j as usize);
        }
        Self { state, i: 0, j: 0 }
    }

    /// A keystream with the first 1024 bytes already discarded.
    pub fn for_stream(key: &[u8]) -> Self {
        let mut rc4 = Self::new(key);
        let mut discard = [0u8; 1024];
        rc4.apply(&mut discard);
        rc4
    }

    /// Encrypt or decrypt in place. RC4 is its own inverse.
    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.state[self.i as usize]);
            self.state.swap(self.i as usize, self.j as usize);
            let k = self.state[self.i as usize].wrapping_add(self.state[self.j as usize]);
            *byte ^= self.state[k as usize];
        }
    }
}

/// SHA-1 over several pieces, which is all this protocol ever asks for.
fn hash(parts: &[&[u8]]) -> [u8; 20] {
    let mut digest = Sha1::new();
    for part in parts {
        digest.update(part);
    }
    digest.finalize().into()
}

/// One side's half of the key exchange.
#[derive(Debug)]
pub struct KeyExchange {
    private: BigUint,
    pub public: [u8; KEY_LEN],
}

impl KeyExchange {
    /// Generate a private key and the public value to send.
    pub fn new() -> Self {
        use rand::Rng;
        // The specification allows 160 bits, which is what everybody uses:
        // the exponent does not have to match the modulus for this to be as
        // strong as it is ever going to be, which is not very.
        let mut secret = [0u8; 20];
        rand::rng().fill_bytes(&mut secret);
        Self::from_secret(&secret)
    }

    /// The same, from a chosen secret. For tests, which need both sides.
    pub fn from_secret(secret: &[u8]) -> Self {
        let prime = prime();
        let private = BigUint::from_bytes_be(secret);
        let public = BigUint::from(GENERATOR).modpow(&private, &prime);
        Self {
            private,
            public: left_pad(&public),
        }
    }

    /// The shared secret, from the other side's public value.
    pub fn shared(&self, theirs: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
        let theirs = BigUint::from_bytes_be(theirs);
        left_pad(&theirs.modpow(&self.private, &prime()))
    }
}

impl Default for KeyExchange {
    fn default() -> Self {
        Self::new()
    }
}

fn prime() -> BigUint {
    BigUint::parse_bytes(PRIME.replace(char::is_whitespace, "").as_bytes(), 16)
        .expect("the specification's prime is a constant in this file")
}

/// Public values are exactly 96 bytes, with leading zeroes kept.
///
/// Trimming them is the classic interoperability bug here: roughly one exchange
/// in 256 produces a value with a leading zero byte, so an implementation that
/// trims works with everybody 255 times out of 256 and is impossible to debug.
fn left_pad(value: &BigUint) -> [u8; KEY_LEN] {
    let bytes = value.to_bytes_be();
    let mut out = [0u8; KEY_LEN];
    let start = KEY_LEN.saturating_sub(bytes.len());
    out[start..].copy_from_slice(&bytes[bytes.len().saturating_sub(KEY_LEN)..]);
    out
}

/// The two RC4 streams a connection uses, from the shared secret and the
/// infohash.
///
/// Named for the roles rather than the directions on purpose: `key_a` is what
/// the side that *dialled* uses to send, whichever side you happen to be.
pub fn stream_keys(shared: &[u8; KEY_LEN], info_hash: InfoHash) -> (Rc4, Rc4) {
    let key_a = hash(&[b"keyA", shared, info_hash.as_bytes()]);
    let key_b = hash(&[b"keyB", shared, info_hash.as_bytes()]);
    (Rc4::for_stream(&key_a), Rc4::for_stream(&key_b))
}

/// The marker the receiving side scans its input for.
pub fn req1(shared: &[u8; KEY_LEN]) -> [u8; 20] {
    hash(&[b"req1", shared])
}

/// The obfuscated infohash: it can be recognised by somebody who already knows
/// which torrent they are looking for, and by nobody else.
pub fn req2_xor_req3(shared: &[u8; KEY_LEN], info_hash: InfoHash) -> [u8; 20] {
    let req2 = hash(&[b"req2", info_hash.as_bytes()]);
    let req3 = hash(&[b"req3", shared]);
    let mut out = [0u8; 20];
    for index in 0..20 {
        out[index] = req2[index] ^ req3[index];
    }
    out
}

/// A random amount of padding, which exists so the handshake has no fixed
/// length for anybody watching to key on.
fn padding() -> Vec<u8> {
    use rand::{Rng, RngExt};
    let length = rand::rng().random_range(0..=MAX_PAD.min(200));
    let mut pad = vec![0u8; length];
    rand::rng().fill_bytes(&mut pad);
    pad
}

/// A stream that may or may not be encrypted.
///
/// Everything above this point in the peer code is written against a plain
/// socket, and none of it should have to learn about ciphers. So the cipher
/// lives here, under the framing.
#[derive(Debug)]
pub enum PeerStream<S> {
    Plain(S),
    Encrypted(Box<Encrypted<S>>),
}

impl<S> PeerStream<S> {
    pub fn is_encrypted(&self) -> bool {
        matches!(self, PeerStream::Encrypted(_))
    }
}

/// A socket with RC4 in both directions.
#[derive(Debug)]
pub struct Encrypted<S> {
    inner: S,
    send: Rc4,
    receive: Rc4,
}

impl<S> Encrypted<S> {
    pub fn new(inner: S, send: Rc4, receive: Rc4) -> Self {
        Self {
            inner,
            send,
            receive,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Encrypted<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                // Only what arrived this time: the rest has been decrypted
                // already, and RC4 is a stream cipher that would happily
                // decrypt it a second time into rubbish.
                let filled = buf.filled_mut();
                this.receive.apply(&mut filled[before..]);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Encrypted<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        // Encrypted into a copy, because the caller's buffer is not ours and a
        // partial write must not leave half of it enciphered.
        let mut scratch = buf.to_vec();
        this.send.apply(&mut scratch);
        match Pin::new(&mut this.inner).poll_write(cx, &scratch) {
            Poll::Ready(Ok(written)) if written < scratch.len() => {
                // A short write would desynchronise the keystream: the bytes
                // that were not sent have already advanced the cipher. Rather
                // than unwind it, refuse. Tokio's `write_all` over a TCP socket
                // does not produce these in practice.
                Poll::Ready(Err(std::io::Error::other(format!(
                    "short write ({written} of {}) would desynchronise the cipher",
                    scratch.len()
                ))))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PeerStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            PeerStream::Plain(inner) => Pin::new(inner).poll_read(cx, buf),
            PeerStream::Encrypted(inner) => Pin::new(inner.as_mut()).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PeerStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            PeerStream::Plain(inner) => Pin::new(inner).poll_write(cx, buf),
            PeerStream::Encrypted(inner) => Pin::new(inner.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            PeerStream::Plain(inner) => Pin::new(inner).poll_flush(cx),
            PeerStream::Encrypted(inner) => Pin::new(inner.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            PeerStream::Plain(inner) => Pin::new(inner).poll_shutdown(cx),
            PeerStream::Encrypted(inner) => Pin::new(inner.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Do the encrypted handshake as the side that dialled.
///
/// Returns a stream with the cipher already in place, positioned exactly where
/// the plaintext BitTorrent handshake would have started. Everything above this
/// carries on as though nothing had happened, which is the point.
pub async fn handshake_outgoing<S>(
    mut stream: S,
    info_hash: InfoHash,
    addr: SocketAddr,
) -> Result<Encrypted<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let keys = KeyExchange::new();

    // Step one: our public value and some padding.
    let mut opening = keys.public.to_vec();
    opening.extend_from_slice(&padding());
    stream.write_all(&opening).await?;

    // Step two: theirs. It is the first 96 bytes; whatever padding follows is
    // found by looking for the marker later.
    let mut theirs = [0u8; KEY_LEN];
    stream.read_exact(&mut theirs).await?;
    let shared = keys.shared(&theirs);

    let (mut send, receive) = stream_keys(&shared, info_hash);

    // Step three: prove we know which torrent we are asking for, and say what
    // we can speak.
    let mut request = Vec::new();
    request.extend_from_slice(&req1(&shared));
    request.extend_from_slice(&req2_xor_req3(&shared, info_hash));

    let pad_c = padding();
    let mut encrypted = Vec::new();
    encrypted.extend_from_slice(&VERIFICATION);
    encrypted.extend_from_slice(&CRYPTO_RC4.to_be_bytes());
    encrypted.extend_from_slice(&(pad_c.len() as u16).to_be_bytes());
    encrypted.extend_from_slice(&pad_c);
    // No initial payload: the BitTorrent handshake goes over the finished
    // stream instead, which is simpler and costs one round trip nobody notices.
    encrypted.extend_from_slice(&0u16.to_be_bytes());
    send.apply(&mut encrypted);

    request.extend_from_slice(&encrypted);
    stream.write_all(&request).await?;

    // Step four: their answer, which begins with our verification constant once
    // it is decrypted. Finding it is how we know the cipher agrees.
    let mut receive = receive;
    let selected = read_response(&mut stream, &mut receive, addr).await?;
    if selected & CRYPTO_RC4 == 0 {
        return Err(Error::Peer(format!(
            "{addr}: chose an encryption mode we did not offer ({selected:#x})"
        )));
    }

    Ok(Encrypted::new(stream, send, receive))
}

/// Read until the other side's encrypted verification constant turns up.
///
/// Byte at a time, which sounds worse than it is: the padding is at most a few
/// hundred bytes and the socket is buffered underneath. Reading in blocks would
/// mean holding decrypted bytes that belong to the stream proper and handing
/// them back, which is a great deal of bookkeeping to save a handful of
/// syscalls on one handshake.
async fn read_response<S>(stream: &mut S, receive: &mut Rc4, addr: SocketAddr) -> Result<u32>
where
    S: AsyncRead + Unpin,
{
    let mut window = [0u8; 8];
    let mut seen = 0usize;

    loop {
        if seen > MAX_SEARCH {
            return Err(Error::Peer(format!(
                "{addr}: no encrypted verification in {seen} bytes"
            )));
        }
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        receive.apply(&mut byte);
        seen += 1;

        window.rotate_left(1);
        window[7] = byte[0];
        if seen >= 8 && window == VERIFICATION {
            break;
        }
    }

    // What follows the constant: the mode they chose and their padding length.
    let mut tail = [0u8; 6];
    stream.read_exact(&mut tail).await?;
    receive.apply(&mut tail);
    let selected = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
    let pad_len = u16::from_be_bytes([tail[4], tail[5]]) as usize;

    if pad_len > MAX_PAD {
        return Err(Error::Peer(format!("{addr}: {pad_len} bytes of padding")));
    }
    if pad_len > 0 {
        let mut pad = vec![0u8; pad_len];
        stream.read_exact(&mut pad).await?;
        receive.apply(&mut pad);
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published RC4 test vectors, from RFC 6229 and the original description.
    ///
    /// Checked against somebody else's arithmetic rather than our own, which is
    /// the only kind of check worth having on a cipher.
    #[test]
    fn rc4_matches_the_published_vectors() {
        let cases: &[(&[u8], &[u8], &[u8])] = &[
            (
                b"Key",
                b"Plaintext",
                &[0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3],
            ),
            (b"Wiki", b"pedia", &[0x10, 0x21, 0xBF, 0x04, 0x20]),
            (
                b"Secret",
                b"Attack at dawn",
                &[
                    0x45, 0xA0, 0x1F, 0x64, 0x5F, 0xC3, 0x5B, 0x38, 0x35, 0x52, 0x54, 0x4B, 0x9B,
                    0xF5,
                ],
            ),
        ];

        for (key, plain, expected) in cases {
            let mut data = plain.to_vec();
            Rc4::new(key).apply(&mut data);
            assert_eq!(&data, expected, "key {key:?}");
        }
    }

    #[test]
    fn rc4_is_its_own_inverse() {
        let mut data = b"the quick brown fox".to_vec();
        let original = data.clone();
        Rc4::new(b"key").apply(&mut data);
        assert_ne!(data, original, "it should actually have done something");
        Rc4::new(b"key").apply(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn the_stream_cipher_throws_away_the_first_kilobyte() {
        // Not an optimisation and not optional: an implementation that keeps
        // those bytes is not weaker, it is incompatible with every other
        // client, and it fails as a connection that hangs.
        let mut plain = vec![0u8; 16];
        let mut discarded = plain.clone();
        Rc4::new(b"key").apply(&mut plain);
        Rc4::for_stream(b"key").apply(&mut discarded);
        assert_ne!(plain, discarded);

        // And it is exactly a kilobyte: skip 1024 by hand and compare.
        let mut by_hand = Rc4::new(b"key");
        let mut skip = [0u8; 1024];
        by_hand.apply(&mut skip);
        let mut theirs = vec![0u8; 16];
        by_hand.apply(&mut theirs);
        assert_eq!(theirs, discarded);
    }

    #[test]
    fn both_sides_of_the_exchange_agree_on_the_secret() {
        let a = KeyExchange::from_secret(&[1u8; 20]);
        let b = KeyExchange::from_secret(&[2u8; 20]);
        assert_eq!(a.shared(&b.public), b.shared(&a.public));
        assert_ne!(a.public, b.public);
    }

    #[test]
    fn public_values_are_always_ninety_six_bytes() {
        // The interoperability bug this prevents: roughly one exchange in 256
        // produces a value with a leading zero byte, so trimming works with
        // everybody 255 times out of 256 and is impossible to reproduce.
        for seed in 0..64u8 {
            let keys = KeyExchange::from_secret(&[seed.max(1); 20]);
            assert_eq!(keys.public.len(), KEY_LEN);
        }
    }

    #[test]
    fn a_short_public_value_is_padded_at_the_front_not_the_back() {
        // Padding at the back is the same bug with the bytes in a different
        // order, and it is just as invisible.
        let small = BigUint::from(1u32);
        let padded = left_pad(&small);
        assert_eq!(padded[KEY_LEN - 1], 1);
        assert!(padded[..KEY_LEN - 1].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn the_two_directions_get_different_keys() {
        // One key for both directions means the two keystreams cancel, which
        // presents as a peer that connects and then sends rubbish.
        let shared = [7u8; KEY_LEN];
        let hash = InfoHash::new([9u8; 20]);
        let (mut send, mut receive) = stream_keys(&shared, hash);

        let mut one = vec![0u8; 8];
        let mut two = vec![0u8; 8];
        send.apply(&mut one);
        receive.apply(&mut two);
        assert_ne!(one, two);
    }

    #[test]
    fn the_obfuscated_infohash_needs_both_halves_to_recover() {
        // The point of the xor: somebody who does not know which torrent is
        // being asked for cannot tell from watching.
        let shared = [3u8; KEY_LEN];
        let hash = InfoHash::new([4u8; 20]);
        let obfuscated = req2_xor_req3(&shared, hash);

        let req3 = hash_of(&[b"req3", &shared]);
        let mut recovered = [0u8; 20];
        for index in 0..20 {
            recovered[index] = obfuscated[index] ^ req3[index];
        }
        assert_eq!(recovered, hash_of(&[b"req2", hash.as_bytes()]));

        // A different torrent gives a different value under the same secret.
        assert_ne!(req2_xor_req3(&shared, InfoHash::new([5u8; 20])), obfuscated);
    }

    fn hash_of(parts: &[&[u8]]) -> [u8; 20] {
        super::hash(parts)
    }

    #[test]
    fn the_marker_depends_on_the_secret_and_nothing_else() {
        assert_eq!(req1(&[1u8; KEY_LEN]), req1(&[1u8; KEY_LEN]));
        assert_ne!(req1(&[1u8; KEY_LEN]), req1(&[2u8; KEY_LEN]));
    }

    #[tokio::test]
    async fn an_encrypted_stream_round_trips_through_a_socket() {
        // Both halves against each other over a real pair of sockets, which is
        // where a wrong keystream direction or a mishandled partial read shows
        // up and a unit test of the cipher does not.
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let shared = [11u8; KEY_LEN];
        let hash = InfoHash::new([12u8; 20]);

        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            // The dialling side's send key is the accepting side's receive key.
            let (a, b) = stream_keys(&shared, hash);
            let mut stream = Encrypted::new(socket, b, a);

            let mut heard = vec![0u8; 11];
            stream.read_exact(&mut heard).await.unwrap();
            assert_eq!(&heard, b"hello there");
            stream.write_all(b"and to you").await.unwrap();
        });

        let socket = TcpStream::connect(addr).await.unwrap();
        let (a, b) = stream_keys(&shared, hash);
        let mut stream = Encrypted::new(socket, a, b);
        stream.write_all(b"hello there").await.unwrap();

        let mut heard = vec![0u8; 10];
        stream.read_exact(&mut heard).await.unwrap();
        assert_eq!(&heard, b"and to you");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn what_goes_over_the_wire_is_not_the_plaintext() {
        // The whole point, and worth asserting rather than assuming: a
        // direction wired to the wrong key would still round trip against
        // itself while sending plaintext to everybody else.
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = vec![0u8; 19];
            socket.read_exact(&mut raw).await.unwrap();
            raw
        });

        let socket = TcpStream::connect(addr).await.unwrap();
        let (a, b) = stream_keys(&[13u8; KEY_LEN], InfoHash::new([14u8; 20]));
        let mut stream = Encrypted::new(socket, a, b);
        stream.write_all(b"BitTorrent protocol").await.unwrap();

        let raw = server.await.unwrap();
        assert_ne!(&raw, b"BitTorrent protocol");
    }
}
