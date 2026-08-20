//! A single peer connection: handshake, extension negotiation, and the BEP 9
//! metadata fetch that turns a magnet into a torrent.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

use crate::error::{Error, Result};
use crate::extended::{
    ExtendedHandshake, HANDSHAKE_ID, MAX_PEX_ADDED, MetadataAssembler, MetadataMessage,
    OUR_UT_METADATA_ID, OUR_UT_PEX_ID, PexMessage,
};
use crate::infohash::InfoHash;
use crate::metainfo::Metainfo;
use crate::mse::{self, PeerStream};
use crate::wire::{Bitfield, Handshake, Message, MessageCodec};

/// How long we wait on a peer before deciding it is not interested in us.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to spend getting a connection open and handshaked.
///
/// Much shorter than the read timeout, and separate from it for a reason worth
/// stating. A tracker will happily name sixty peers of which forty are
/// unreachable, and with one timeout for both jobs each of those forty holds a
/// connection slot for the whole of it, doing nothing. Time to first byte on a
/// cold magnet is dominated by that and by nothing clever in the picker.
///
/// Three seconds is generous for a TCP handshake to anywhere that is going to
/// answer at all, and a peer that accepts the socket and then says nothing is
/// not one that was about to be useful.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// The two waits a peer connection involves, which are not the same wait.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Opening the socket and exchanging handshakes.
    pub connect: Duration,
    /// Waiting for the next message on a connection that already works.
    pub read: Duration,
    /// Retry with an obfuscated handshake when the plaintext one is refused.
    pub encrypt: bool,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT_TIMEOUT,
            read: DEFAULT_TIMEOUT,
            encrypt: true,
        }
    }
}

/// Did this failure happen *after* the socket opened?
///
/// The distinction is the whole basis for retrying with encryption. A connect
/// timeout or a refused connection means there is nothing at that address, and
/// dialling it again with a different handshake is a wasted three seconds. A
/// socket that opened and then closed, or went quiet, during the handshake is
/// the signature of a peer that will not talk to us in plaintext, and that one
/// is worth a second attempt.
fn refused_the_handshake(err: &Error) -> bool {
    match err {
        Error::Peer(said) => {
            !said.contains("connect timed out")
                && (said.contains("closed the connection")
                    || said.contains("handshake")
                    || said.contains("wrong infohash"))
        }
        // A read or write that failed on an open socket.
        Error::Io(err) => matches!(
            err.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

/// A connected, handshaken peer.
pub struct PeerConnection {
    framed: Framed<PeerStream<TcpStream>, MessageCodec>,
    pub addr: SocketAddr,
    pub handshake: Handshake,
    pub extended: Option<ExtendedHandshake>,
    /// True while the peer refuses to send us blocks.
    pub peer_choking: bool,
    pub peer_interested: bool,
    pub am_choking: bool,
    pub am_interested: bool,
    /// Which pieces the peer has. `None` until we know the piece count, since
    /// a magnet has no idea how many pieces exist until metadata arrives.
    pub have: Option<Bitfield>,
    /// Bitfield and `have` messages that arrived before we knew the count.
    pending_have: Vec<Message>,
    /// Addresses this peer has introduced us to over BEP 11, waiting to be
    /// collected by whoever is running the connection.
    pex: Vec<SocketAddr>,
}

impl PeerConnection {
    /// Connect, handshake, and exchange extended handshakes if both sides
    /// support BEP 10.
    /// Connect, handshake, and exchange extended handshakes if both sides
    /// support BEP 10.
    ///
    /// Plaintext first, then obfuscated. That order is deliberate and is the
    /// cheap one: most peers take a plaintext handshake, so the common case
    /// pays nothing, and a peer configured to require encryption drops the
    /// connection at the handshake, which is exactly the failure we can detect
    /// and retry. The other order would cost a round trip and a reconnect
    /// against every ordinary peer to reach the minority.
    pub async fn connect(
        addr: SocketAddr,
        info_hash: InfoHash,
        peer_id: [u8; 20],
        port: u16,
        timeouts: Timeouts,
    ) -> Result<Self> {
        match Self::connect_plain(addr, info_hash, peer_id, port, timeouts).await {
            Ok(peer) => Ok(peer),
            Err(plain) if timeouts.encrypt && refused_the_handshake(&plain) => {
                tracing::debug!(%addr, %plain, "plaintext refused; trying an obfuscated handshake");
                Self::connect_encrypted(addr, info_hash, peer_id, port, timeouts)
                    .await
                    // The first failure is the more useful one to report: it is
                    // what a peer that simply is not there also looks like.
                    .map_err(|encrypted| {
                        Error::Peer(format!(
                            "{addr}: plaintext: {plain}; encrypted: {encrypted}"
                        ))
                    })
            }
            Err(plain) => Err(plain),
        }
    }

    async fn connect_plain(
        addr: SocketAddr,
        info_hash: InfoHash,
        peer_id: [u8; 20],
        port: u16,
        timeouts: Timeouts,
    ) -> Result<Self> {
        let stream = tokio::time::timeout(timeouts.connect, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Peer(format!("{addr}: connect timed out")))??;
        stream.set_nodelay(true).ok();

        let mut peer = Self::handshake(
            PeerStream::Plain(stream),
            addr,
            info_hash,
            peer_id,
            timeouts.connect,
        )
        .await?;

        if peer.handshake.supports_extended() {
            let ours = ExtendedHandshake::ours(port, None);
            peer.send(Message::Extended {
                id: HANDSHAKE_ID,
                payload: Bytes::from(ours.encode()?),
            })
            .await?;
            // Read timeout from here: the connection has demonstrably worked,
            // and a peer that is thinking about its extension list deserves
            // longer than one that has not answered a TCP handshake.
            peer.await_extended_handshake(timeouts.read).await?;
        }
        Ok(peer)
    }

    /// Take over a connection somebody made to us.
    ///
    /// The mirror of [`PeerConnection::connect`], and the order is the whole
    /// difference: the side that dials speaks first, so an accepted peer has
    /// already sent its handshake (the listener had to read it to know which
    /// torrent this was for) and is waiting on ours.
    ///
    /// We remain leech-only. This peer connected hoping to be served, and will
    /// not be. What it gives us is a peer we could not have dialled, which on a
    /// swarm full of NATs is most of them.
    pub async fn accept(
        incoming: crate::inbound::Incoming,
        info_hash: InfoHash,
        peer_id: [u8; 20],
        port: u16,
        timeouts: Timeouts,
    ) -> Result<Self> {
        use tokio::io::AsyncWriteExt;

        let crate::inbound::Incoming {
            mut stream,
            addr,
            handshake: theirs,
        } = incoming;

        // The listener routed by this, so it should already match. Checked
        // again because this is the place where being wrong means mixing two
        // torrents' pieces together, and the check is one comparison.
        if theirs.info_hash != info_hash {
            return Err(Error::Peer(format!(
                "{addr}: wrong infohash ({} not {info_hash})",
                theirs.info_hash
            )));
        }

        let ours = Handshake::new(info_hash, peer_id);
        tokio::time::timeout(timeouts.connect, stream.write_all(&ours.encode()))
            .await
            .map_err(|_| Error::Peer(format!("{addr}: handshake write timed out")))??;

        let mut peer = Self {
            framed: Framed::new(PeerStream::Plain(stream), MessageCodec),
            addr,
            handshake: theirs,
            extended: None,
            peer_choking: true,
            peer_interested: false,
            am_choking: true,
            am_interested: false,
            have: None,
            pending_have: Vec::new(),
            pex: Vec::new(),
        };

        if peer.handshake.supports_extended() {
            let ours = ExtendedHandshake::ours(port, None);
            peer.send(Message::Extended {
                id: HANDSHAKE_ID,
                payload: Bytes::from(ours.encode()?),
            })
            .await?;
            peer.await_extended_handshake(timeouts.read).await?;
        }
        Ok(peer)
    }

    /// The same, over an obfuscated stream.
    ///
    /// A second connection rather than a retry on the first: by the time a
    /// plaintext handshake has failed the socket has our unencrypted bytes on
    /// it and the other side has hung up, so there is nothing to reuse.
    async fn connect_encrypted(
        addr: SocketAddr,
        info_hash: InfoHash,
        peer_id: [u8; 20],
        port: u16,
        timeouts: Timeouts,
    ) -> Result<Self> {
        let stream = tokio::time::timeout(timeouts.connect, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Peer(format!("{addr}: connect timed out")))??;
        stream.set_nodelay(true).ok();

        let encrypted = tokio::time::timeout(
            timeouts.connect,
            mse::handshake_outgoing(stream, info_hash, addr),
        )
        .await
        .map_err(|_| Error::Peer(format!("{addr}: the obfuscated handshake timed out")))??;

        let mut peer = Self::handshake(
            PeerStream::Encrypted(Box::new(encrypted)),
            addr,
            info_hash,
            peer_id,
            timeouts.connect,
        )
        .await?;
        tracing::debug!(%addr, "connected over an obfuscated stream");

        if peer.handshake.supports_extended() {
            let ours = ExtendedHandshake::ours(port, None);
            peer.send(Message::Extended {
                id: HANDSHAKE_ID,
                payload: Bytes::from(ours.encode()?),
            })
            .await?;
            peer.await_extended_handshake(timeouts.read).await?;
        }
        Ok(peer)
    }

    async fn handshake(
        mut stream: PeerStream<TcpStream>,
        addr: SocketAddr,
        info_hash: InfoHash,
        peer_id: [u8; 20],
        timeout: Duration,
    ) -> Result<Self> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let ours = Handshake::new(info_hash, peer_id);
        tokio::time::timeout(timeout, stream.write_all(&ours.encode()))
            .await
            .map_err(|_| Error::Peer(format!("{addr}: handshake write timed out")))??;

        let mut buf = [0u8; crate::wire::HANDSHAKE_LEN];
        tokio::time::timeout(timeout, stream.read_exact(&mut buf))
            .await
            .map_err(|_| Error::Peer(format!("{addr}: handshake read timed out")))??;

        let theirs = Handshake::decode(&buf)?;
        // A peer answering with a different infohash is either confused or
        // playing games. Either way it is not in our swarm.
        if theirs.info_hash != info_hash {
            return Err(Error::Peer(format!(
                "{addr}: wrong infohash ({} not {info_hash})",
                theirs.info_hash
            )));
        }

        Ok(Self {
            framed: Framed::new(stream, MessageCodec),
            addr,
            handshake: theirs,
            extended: None,
            peer_choking: true,
            peer_interested: false,
            am_choking: true,
            am_interested: false,
            have: None,
            pending_have: Vec::new(),
            pex: Vec::new(),
        })
    }

    /// Read messages until the peer's extended handshake turns up, buffering
    /// anything else. Peers often send `bitfield` first.
    async fn await_extended_handshake(&mut self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let message = match tokio::time::timeout_at(deadline, self.framed.next()).await {
                Ok(Some(message)) => message?,
                Ok(None) => {
                    return Err(Error::Peer(format!("{}: closed the connection", self.addr)));
                }
                Err(_) => {
                    return Err(Error::Peer(format!(
                        "{}: no extended handshake in time",
                        self.addr
                    )));
                }
            };
            if let Message::Extended {
                id: HANDSHAKE_ID,
                payload,
            } = &message
            {
                self.extended = Some(ExtendedHandshake::decode(payload)?);
                return Ok(());
            }
            self.absorb(message);
        }
    }

    /// Update our view of the peer from a message, and stash the ones we
    /// cannot act on yet.
    fn absorb(&mut self, message: Message) {
        match &message {
            Message::Choke => self.peer_choking = true,
            Message::Unchoke => self.peer_choking = false,
            Message::Interested => self.peer_interested = true,
            Message::NotInterested => self.peer_interested = false,
            Message::Bitfield(bits) => match &mut self.have {
                Some(have) => {
                    if let Ok(parsed) = Bitfield::from_bytes(bits, have.len()) {
                        *have = parsed;
                    }
                }
                None => self.pending_have.push(message),
            },
            Message::Have(index) => match &mut self.have {
                Some(have) => have.set(*index as usize),
                None => self.pending_have.push(message),
            },
            // BEP 11. The id is ours because we are the receiving side: we
            // advertised `ut_pex` on OUR_UT_PEX_ID in our own handshake, so
            // that is the id a peer sends it back on.
            Message::Extended {
                id: OUR_UT_PEX_ID,
                payload,
            } => match PexMessage::decode(payload) {
                Ok(message) => {
                    // Bounded even if a peer sends these faster than the
                    // session drains them.
                    let room = MAX_PEX_ADDED.saturating_sub(self.pex.len());
                    self.pex.extend(message.added.into_iter().take(room));
                }
                // A peer sending nonsense here is not worth dropping over: it
                // is still perfectly capable of sending us pieces.
                Err(err) => tracing::debug!(addr = %self.addr, %err, "unreadable ut_pex"),
            },
            _ => {}
        }
    }

    /// Take the addresses this peer has introduced us to since the last call.
    ///
    /// Drained rather than read, because the caller's job is to hand them to
    /// the peer queue and holding a second copy here would only go stale.
    pub fn take_pex_peers(&mut self) -> Vec<SocketAddr> {
        std::mem::take(&mut self.pex)
    }

    /// Once metadata is in hand we know the piece count, so the buffered
    /// bitfield and `have` messages can finally be applied.
    pub fn set_piece_count(&mut self, count: usize) -> Result<()> {
        let mut have = Bitfield::empty(count);
        for message in std::mem::take(&mut self.pending_have) {
            match message {
                Message::Bitfield(bits) => have = Bitfield::from_bytes(&bits, count)?,
                Message::Have(index) => have.set(index as usize),
                _ => {}
            }
        }
        self.have = Some(have);
        Ok(())
    }

    pub async fn send(&mut self, message: Message) -> Result<()> {
        self.framed.send(message).await
    }

    /// Receive the next message, keeping connection state up to date.
    pub async fn recv(&mut self, timeout: Duration) -> Result<Message> {
        let message = tokio::time::timeout(timeout, self.framed.next())
            .await
            .map_err(|_| Error::Peer(format!("{}: read timed out", self.addr)))?
            .ok_or_else(|| Error::Peer(format!("{}: closed the connection", self.addr)))??;
        self.absorb(message.clone());
        Ok(message)
    }

    pub fn supports_metadata(&self) -> bool {
        self.extended
            .as_ref()
            .is_some_and(|ext| ext.ut_metadata_id().is_some())
    }

    pub fn client_name(&self) -> Option<&str> {
        self.extended.as_ref()?.client.as_deref()
    }

    pub fn has_piece(&self, index: usize) -> bool {
        self.have.as_ref().is_some_and(|have| have.has(index))
    }

    /// Fetch the info dictionary over BEP 9 and verify it against the
    /// infohash we asked for.
    ///
    /// The verification is the whole point. A peer can answer a metadata
    /// request with any bytes it likes; without this check it would be
    /// choosing our file layout and our piece hashes for us.
    pub async fn fetch_metadata(&mut self, expected: InfoHash) -> Result<Metainfo> {
        let extended = self
            .extended
            .as_ref()
            .ok_or_else(|| Error::Peer(format!("{}: no extension protocol", self.addr)))?;
        let metadata_id = extended
            .ut_metadata_id()
            .ok_or_else(|| Error::Peer(format!("{}: does not serve metadata", self.addr)))?;
        let total_size = extended.metadata_size.ok_or_else(|| {
            Error::Peer(format!(
                "{}: did not say how big the metadata is",
                self.addr
            ))
        })?;

        let mut assembler = MetadataAssembler::new(total_size)?;
        // Ask for everything up front; the transfer is small and a round trip
        // per 16 KiB block is a pointless way to spend a second.
        for piece in 0..assembler.block_count() {
            let payload = MetadataMessage::Request { piece }.encode()?;
            self.send(Message::Extended {
                id: metadata_id,
                payload: Bytes::from(payload),
            })
            .await?;
        }

        while !assembler.is_complete() {
            let message = self.recv(DEFAULT_TIMEOUT).await?;
            let Message::Extended { id, payload } = message else {
                continue;
            };
            // Peers send us this extension on the id *we* advertised.
            if id != OUR_UT_METADATA_ID {
                continue;
            }
            match MetadataMessage::decode(&payload)? {
                MetadataMessage::Data { piece, block, .. } => {
                    assembler.insert(piece, block)?;
                }
                MetadataMessage::Reject { piece } => {
                    return Err(Error::Peer(format!(
                        "{}: rejected metadata block {piece}",
                        self.addr
                    )));
                }
                MetadataMessage::Request { .. } => {}
            }
        }

        let raw_info = assembler
            .finish()
            .ok_or_else(|| Error::Peer("metadata assembler lost a block".into()))?;

        let mut meta =
            Metainfo::from_verified_info_dict(&raw_info, expected).map_err(|err| match err {
                Error::MetadataMismatch { .. } => Error::MetadataMismatch {
                    peer: self.addr.to_string(),
                },
                other => other,
            })?;
        meta.announce.clear();
        Ok(meta)
    }
}

impl std::fmt::Debug for PeerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerConnection")
            .field("addr", &self.addr)
            .field("client", &self.client_name())
            .field("choking", &self.peer_choking)
            .finish()
    }
}

/// Ask peers for the torrent's metadata, taking the first verified answer.
///
/// Peers are tried in small concurrent batches: most connections fail or stall,
/// so serial attempts turn a two-second job into a two-minute one.
pub async fn fetch_metadata_from_peers(
    peers: &[SocketAddr],
    info_hash: InfoHash,
    peer_id: [u8; 20],
    port: u16,
    concurrency: usize,
) -> Result<(Metainfo, SocketAddr)> {
    fetch_metadata_collecting(
        peers,
        info_hash,
        peer_id,
        port,
        concurrency,
        &mut Vec::new(),
    )
    .await
}

/// The same search, keeping the addresses the peers we spoke to introduced us
/// to along the way.
///
/// Worth the extra parameter for the case this whole path exists to survive: a
/// swarm where the peers a tracker names will send data but will not answer a
/// metadata request. Such a peer usually knows one that will, and without this
/// its introductions die with the connection and the next sweep starts from the
/// same exhausted list.
pub async fn fetch_metadata_collecting(
    peers: &[SocketAddr],
    info_hash: InfoHash,
    peer_id: [u8; 20],
    port: u16,
    concurrency: usize,
    introduced: &mut Vec<SocketAddr>,
) -> Result<(Metainfo, SocketAddr)> {
    if peers.is_empty() {
        return Err(Error::NoPeers {
            info_hash: info_hash.to_hex(),
        });
    }

    for batch in peers.chunks(concurrency.max(1)) {
        let attempts = batch.iter().map(|addr| {
            let addr = *addr;
            async move {
                // Each attempt hands back whatever it was told about, whether
                // or not it produced metadata. A peer that refuses us is still
                // a peer that knows who else is here.
                let mut found = Vec::new();
                let result = async {
                    let mut peer = PeerConnection::connect(
                        addr,
                        info_hash,
                        peer_id,
                        port,
                        Timeouts::default(),
                    )
                    .await?;
                    if !peer.supports_metadata() {
                        found = peer.take_pex_peers();
                        return Err(Error::Peer(format!("{addr}: does not serve metadata")));
                    }
                    let meta = peer.fetch_metadata(info_hash).await;
                    found = peer.take_pex_peers();
                    Ok::<_, Error>((meta?, addr))
                }
                .await;
                (result, found)
            }
        });

        let mut answer = None;
        for (result, found) in futures_util::future::join_all(attempts).await {
            introduced.extend(found);
            match result {
                // Collected rather than returned immediately, so the rest of
                // the batch's introductions are not thrown away with it.
                Ok(found) if answer.is_none() => answer = Some(found),
                Ok(_) => {}
                Err(err) => tracing::debug!(%err, "metadata fetch failed"),
            }
        }
        if let Some(found) = answer {
            return Ok(found);
        }
    }

    Err(Error::Peer(format!(
        "asked {} peers for metadata; none answered",
        peers.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::HANDSHAKE_LEN;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn hash() -> InfoHash {
        InfoHash::parse("15c74d4165fc2ffff997d576bf44b4b25cbeb04e").unwrap()
    }

    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s);
        out
    }

    /// A real-shaped info dict, so the infohash check is meaningful.
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

    /// A pretend peer that serves one info dict over BEP 9.
    async fn spawn_fake_peer(info: Vec<u8>, info_hash: InfoHash, metadata_id: u8) -> SocketAddr {
        spawn_fake_peer_with(info, info_hash, metadata_id, &[]).await
    }

    /// A bencoded `ut_pex` message announcing `peers` as additions.
    fn pex_payload(peers: &[SocketAddr]) -> Vec<u8> {
        let mut compact = Vec::new();
        for peer in peers {
            let std::net::IpAddr::V4(ip) = peer.ip() else {
                panic!("the fixture only speaks IPv4");
            };
            compact.extend_from_slice(&ip.octets());
            compact.extend_from_slice(&peer.port().to_be_bytes());
        }
        let mut out = b"d5:added".to_vec();
        out.extend(format!("{}:", compact.len()).into_bytes());
        out.extend_from_slice(&compact);
        out.extend(b"e");
        out
    }

    /// The same peer, optionally gossiping about who else is in the swarm
    /// before it gets down to business.
    async fn spawn_fake_peer_with(
        info: Vec<u8>,
        info_hash: InfoHash,
        metadata_id: u8,
        pex: &[SocketAddr],
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pex = pex.to_vec();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut buf = [0u8; HANDSHAKE_LEN];
            stream.read_exact(&mut buf).await.unwrap();
            let theirs = Handshake::decode(&buf).unwrap();
            stream
                .write_all(&Handshake::new(info_hash, *b"-FAKE01-000000000000").encode())
                .await
                .unwrap();
            assert!(theirs.supports_extended());

            let mut framed = Framed::new(stream, MessageCodec);

            // Our extended handshake, telling the client which id to use.
            let mut ours = ExtendedHandshake::ours(6881, Some(info.len()));
            ours.message_ids.insert("ut_metadata".into(), metadata_id);
            framed
                .send(Message::Extended {
                    id: HANDSHAKE_ID,
                    payload: Bytes::from(ours.encode().unwrap()),
                })
                .await
                .unwrap();

            if !pex.is_empty() {
                // On the id the *client* advertised, which is the direction
                // that trips people up.
                framed
                    .send(Message::Extended {
                        id: OUR_UT_PEX_ID,
                        payload: Bytes::from(pex_payload(&pex)),
                    })
                    .await
                    .unwrap();
            }

            while let Some(Ok(message)) = framed.next().await {
                let Message::Extended { id, payload } = message else {
                    continue;
                };
                if id != metadata_id {
                    continue;
                }
                if let Ok(MetadataMessage::Request { piece }) = MetadataMessage::decode(&payload) {
                    let start = piece * crate::extended::METADATA_BLOCK_SIZE;
                    let end = (start + crate::extended::METADATA_BLOCK_SIZE).min(info.len());
                    let reply = MetadataMessage::Data {
                        piece,
                        total_size: info.len(),
                        block: info[start..end].to_vec(),
                    }
                    .encode()
                    .unwrap();
                    framed
                        .send(Message::Extended {
                            id: OUR_UT_METADATA_ID,
                            payload: Bytes::from(reply),
                        })
                        .await
                        .unwrap();
                }
            }
        });

        addr
    }

    #[tokio::test]
    async fn peer_exchange_addresses_survive_the_connection() {
        // The case this exists for: a swarm where the tracker's addresses are
        // spent and the only new ones come from a peer we already have open.
        let info = info_dict();
        let real = Metainfo::from_info_dict(&info).unwrap().info_hash;
        let gossip: Vec<SocketAddr> = vec![
            "10.1.2.3:6881".parse().unwrap(),
            "10.1.2.4:51413".parse().unwrap(),
        ];
        let addr = spawn_fake_peer_with(info, real, 5, &gossip).await;

        let mut introduced = Vec::new();
        let (meta, _) =
            fetch_metadata_collecting(&[addr], real, [1u8; 20], 6881, 4, &mut introduced)
                .await
                .expect("metadata");

        assert_eq!(meta.info_hash, real);
        introduced.sort();
        assert_eq!(introduced, gossip, "the introductions must not be lost");
    }

    #[tokio::test]
    async fn fetches_metadata_from_a_peer_and_verifies_it() {
        let info = info_dict();
        let real_hash = Metainfo::from_info_dict(&info).unwrap().info_hash;
        // Deliberately not our own id: the peer picks its own, and we must use it.
        let addr = spawn_fake_peer(info.clone(), real_hash, 7).await;

        let mut peer =
            PeerConnection::connect(addr, real_hash, [1u8; 20], 6881, Timeouts::default())
                .await
                .expect("connects");
        assert!(peer.supports_metadata());
        assert_eq!(peer.extended.as_ref().unwrap().ut_metadata_id(), Some(7));
        assert_eq!(
            peer.extended.as_ref().unwrap().metadata_size,
            Some(info.len())
        );

        let meta = peer.fetch_metadata(real_hash).await.expect("metadata");
        assert_eq!(meta.info_hash, real_hash);
        assert_eq!(meta.name, "a-file.bin");
        assert_eq!(meta.total_length, 2000);
        assert_eq!(meta.piece_count(), 2);
    }

    #[tokio::test]
    async fn a_peer_serving_the_wrong_info_dict_is_rejected() {
        let real = Metainfo::from_info_dict(&info_dict()).unwrap().info_hash;

        // The peer handshakes for the right infohash but serves a different
        // torrent's metadata: exactly the attack the hash check exists for.
        let mut lying = info_dict();
        let pos = lying.windows(10).position(|w| w == b"a-file.bin").unwrap();
        lying[pos..pos + 10].copy_from_slice(b"evil-file!");
        let addr = spawn_fake_peer(lying, real, 3).await;

        let mut peer = PeerConnection::connect(addr, real, [1u8; 20], 6881, Timeouts::default())
            .await
            .unwrap();
        let err = peer.fetch_metadata(real).await.unwrap_err();
        assert!(
            matches!(err, Error::MetadataMismatch { .. }),
            "expected a mismatch, got {err:?}"
        );
    }

    #[tokio::test]
    async fn multi_block_metadata_reassembles() {
        // Pad the info dict past one 16 KiB block with an ignorable key.
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"filler"));
        info.extend(bstr(&vec![b'z'; 20_000]));
        info.extend(bstr(b"length"));
        info.extend(b"i2000e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"big.bin"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i1024e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0xAAu8; 40]));
        info.extend(b"e");
        assert!(info.len() > crate::extended::METADATA_BLOCK_SIZE);

        let real = Metainfo::from_info_dict(&info).unwrap().info_hash;
        let addr = spawn_fake_peer(info, real, 1).await;

        let mut peer = PeerConnection::connect(addr, real, [1u8; 20], 6881, Timeouts::default())
            .await
            .unwrap();
        let meta = peer.fetch_metadata(real).await.unwrap();
        assert_eq!(meta.info_hash, real);
        assert_eq!(meta.name, "big.bin");
    }

    #[tokio::test]
    async fn refuses_a_peer_in_a_different_swarm() {
        let addr = spawn_fake_peer(info_dict(), InfoHash::new([9u8; 20]), 1).await;
        let err = PeerConnection::connect(addr, hash(), [1u8; 20], 6881, Timeouts::default())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("wrong infohash"), "{err}");
    }

    #[tokio::test]
    async fn buffered_bitfields_apply_once_the_piece_count_is_known() {
        let info = info_dict();
        let real = Metainfo::from_info_dict(&info).unwrap().info_hash;
        let addr = spawn_fake_peer(info, real, 1).await;

        let mut peer = PeerConnection::connect(addr, real, [1u8; 20], 6881, Timeouts::default())
            .await
            .unwrap();
        // Pretend a bitfield arrived before we had metadata.
        peer.absorb(Message::Bitfield(Bytes::from_static(&[0b1000_0000])));
        peer.absorb(Message::Have(1));
        assert!(peer.have.is_none());

        peer.set_piece_count(2).unwrap();
        assert!(peer.has_piece(0));
        assert!(peer.has_piece(1));
    }

    #[tokio::test]
    async fn an_empty_peer_list_says_so() {
        let err = fetch_metadata_from_peers(&[], hash(), [0u8; 20], 6881, 4)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoPeers { .. }));
    }
}
