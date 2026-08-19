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
    ExtendedHandshake, HANDSHAKE_ID, MetadataAssembler, MetadataMessage, OUR_UT_METADATA_ID,
};
use crate::infohash::InfoHash;
use crate::metainfo::Metainfo;
use crate::wire::{Bitfield, Handshake, Message, MessageCodec};

/// How long we wait on a peer before deciding it is not interested in us.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// A connected, handshaken peer.
pub struct PeerConnection {
    framed: Framed<TcpStream, MessageCodec>,
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
}

impl PeerConnection {
    /// Connect, handshake, and exchange extended handshakes if both sides
    /// support BEP 10.
    pub async fn connect(
        addr: SocketAddr,
        info_hash: InfoHash,
        peer_id: [u8; 20],
        port: u16,
        timeout: Duration,
    ) -> Result<Self> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| Error::Peer(format!("{addr}: connect timed out")))??;
        stream.set_nodelay(true).ok();

        let mut peer = Self::handshake(stream, addr, info_hash, peer_id, timeout).await?;

        if peer.handshake.supports_extended() {
            let ours = ExtendedHandshake::ours(port, None);
            peer.send(Message::Extended {
                id: HANDSHAKE_ID,
                payload: Bytes::from(ours.encode()?),
            })
            .await?;
            peer.await_extended_handshake(timeout).await?;
        }
        Ok(peer)
    }

    async fn handshake(
        mut stream: TcpStream,
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
            _ => {}
        }
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
    if peers.is_empty() {
        return Err(Error::NoPeers {
            info_hash: info_hash.to_hex(),
        });
    }

    for batch in peers.chunks(concurrency.max(1)) {
        let attempts = batch.iter().map(|addr| {
            let addr = *addr;
            async move {
                let mut peer =
                    PeerConnection::connect(addr, info_hash, peer_id, port, DEFAULT_TIMEOUT)
                        .await?;
                if !peer.supports_metadata() {
                    return Err(Error::Peer(format!("{addr}: does not serve metadata")));
                }
                let meta = peer.fetch_metadata(info_hash).await?;
                Ok::<_, Error>((meta, addr))
            }
        });

        for result in futures_util::future::join_all(attempts).await {
            match result {
                Ok(found) => return Ok(found),
                Err(err) => tracing::debug!(%err, "metadata fetch failed"),
            }
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

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
    async fn fetches_metadata_from_a_peer_and_verifies_it() {
        let info = info_dict();
        let real_hash = Metainfo::from_info_dict(&info).unwrap().info_hash;
        // Deliberately not our own id: the peer picks its own, and we must use it.
        let addr = spawn_fake_peer(info.clone(), real_hash, 7).await;

        let mut peer = PeerConnection::connect(addr, real_hash, [1u8; 20], 6881, DEFAULT_TIMEOUT)
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

        let mut peer = PeerConnection::connect(addr, real, [1u8; 20], 6881, DEFAULT_TIMEOUT)
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

        let mut peer = PeerConnection::connect(addr, real, [1u8; 20], 6881, DEFAULT_TIMEOUT)
            .await
            .unwrap();
        let meta = peer.fetch_metadata(real).await.unwrap();
        assert_eq!(meta.info_hash, real);
        assert_eq!(meta.name, "big.bin");
    }

    #[tokio::test]
    async fn refuses_a_peer_in_a_different_swarm() {
        let addr = spawn_fake_peer(info_dict(), InfoHash::new([9u8; 20]), 1).await;
        let err = PeerConnection::connect(addr, hash(), [1u8; 20], 6881, DEFAULT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("wrong infohash"), "{err}");
    }

    #[tokio::test]
    async fn buffered_bitfields_apply_once_the_piece_count_is_known() {
        let info = info_dict();
        let real = Metainfo::from_info_dict(&info).unwrap().info_hash;
        let addr = spawn_fake_peer(info, real, 1).await;

        let mut peer = PeerConnection::connect(addr, real, [1u8; 20], 6881, DEFAULT_TIMEOUT)
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
