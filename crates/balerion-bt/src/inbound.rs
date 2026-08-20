//! Accepting connections, rather than only making them.
//!
//! balerion announces a listening port to trackers, to the DHT and to every
//! peer it handshakes with. Until this module existed, nothing was listening on
//! it, which is a small lie with a real cost: a peer behind a NAT can dial us
//! and cannot be dialled, so every one of them was unreachable in both
//! directions at once. On the thin, awkward swarms this program spends its time
//! on, refusing half the available connections is the wrong economy.
//!
//! One socket serves every torrent in the process, because there is one
//! announced port and a peer picks the swarm by putting an infohash in its
//! handshake. So the listener reads that handshake, looks the infohash up, and
//! hands the connection to whichever session claimed it. A connection naming a
//! torrent nobody is running is dropped without a reply.
//!
//! This does not make balerion a seeder. An accepted peer is one we can ask for
//! pieces; we still never unchoke and never serve a block. What it buys is
//! reach.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::error::Result;
use crate::infohash::InfoHash;
use crate::wire::{HANDSHAKE_LEN, Handshake};

/// How long a connection has to send its handshake before we lose interest.
///
/// Short on purpose. A peer that has connected and said nothing is either a
/// port scanner or a machine that will not be useful in the next minute, and
/// each one is holding a task.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections queued for one torrent before we start dropping them.
///
/// Small deliberately: these are peers offering to talk to us, and one that
/// waits thirty seconds in a queue has usually given up by the time it is
/// answered. Dropping is honest and it reconnects.
const QUEUE_DEPTH: usize = 8;

/// A connection that arrived, with its handshake already read.
///
/// The handshake had to be read to know which torrent it was for, so it comes
/// along rather than being read twice. Our own reply has not been sent yet:
/// that is the session's job, because it is the thing that knows our peer id.
#[derive(Debug)]
pub struct Incoming {
    pub stream: TcpStream,
    pub addr: SocketAddr,
    pub handshake: Handshake,
}

type Registry = Arc<Mutex<HashMap<InfoHash, mpsc::Sender<Incoming>>>>;

/// The listening socket, shared by every torrent in the process.
#[derive(Debug, Clone)]
pub struct Inbound {
    port: u16,
    registry: Registry,
}

impl Inbound {
    /// Bind and start accepting.
    ///
    /// Falls back to an ephemeral port when the requested one is taken, because
    /// a second balerion on the same machine should not fail to start over a
    /// port nobody chose deliberately. Announce [`Inbound::port`] rather than
    /// the number you asked for: they are frequently different, and announcing
    /// a port we are not on is the fault this module exists to fix.
    pub async fn bind(port: u16) -> Result<Self> {
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(listener) => listener,
            Err(err) if port != 0 => {
                tracing::debug!(port, %err, "that port is taken; taking any free one");
                TcpListener::bind(("0.0.0.0", 0)).await?
            }
            Err(err) => return Err(err.into()),
        };
        let port = listener.local_addr()?.port();
        let registry: Registry = Arc::default();

        let accepting = Arc::clone(&registry);
        tokio::spawn(async move {
            loop {
                let Ok((stream, addr)) = listener.accept().await else {
                    // The socket is gone. Nothing to be done and nothing worth
                    // retrying, so stop rather than spin.
                    tracing::debug!("the inbound listener stopped accepting");
                    return;
                };
                let registry = Arc::clone(&accepting);
                // One task per connection: reading a handshake from a peer
                // that never sends one must not hold up the accept loop.
                tokio::spawn(async move {
                    if let Err(err) = greet(stream, addr, &registry).await {
                        tracing::trace!(%addr, %err, "inbound connection came to nothing");
                    }
                });
            }
        });

        tracing::info!(port, "listening for peers");
        Ok(Self { port, registry })
    }

    /// The port actually bound, which is what should be announced.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Claim inbound connections for one torrent.
    ///
    /// The registration lives as long as the returned guard, so a session that
    /// ends stops receiving connections without anyone having to remember to
    /// say so. Registering an infohash twice replaces the first claim, which is
    /// the sane reading of running the same torrent twice.
    pub fn register(&self, info_hash: InfoHash) -> (Registration, mpsc::Receiver<Incoming>) {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        self.registry
            .lock()
            .expect("inbound registry lock")
            .insert(info_hash, tx);
        (
            Registration {
                info_hash,
                registry: Arc::clone(&self.registry),
            },
            rx,
        )
    }
}

/// Keeps one torrent's claim on the listener alive.
#[derive(Debug)]
pub struct Registration {
    info_hash: InfoHash,
    registry: Registry,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry
            .lock()
            .expect("inbound registry lock")
            .remove(&self.info_hash);
    }
}

/// Read one handshake and route the connection to whoever wants it.
async fn greet(mut stream: TcpStream, addr: SocketAddr, registry: &Registry) -> Result<()> {
    let mut buf = [0u8; HANDSHAKE_LEN];
    tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .map_err(|_| crate::error::Error::Peer(format!("{addr}: no handshake in time")))??;

    let handshake = Handshake::decode(&buf)?;
    let sender = registry
        .lock()
        .expect("inbound registry lock")
        .get(&handshake.info_hash)
        .cloned();

    let Some(sender) = sender else {
        // Someone asking for a torrent we are not running. Perfectly ordinary
        // on a machine that has finished with one, and not worth a reply.
        return Err(crate::error::Error::Peer(format!(
            "{addr}: wanted {}, which we are not running",
            handshake.info_hash
        )));
    };

    stream.set_nodelay(true).ok();
    // try_send rather than send: a full queue means the session already has
    // more offers than it can use, and holding this task open to wait would
    // only convert a dropped connection into a stalled one.
    sender
        .try_send(Incoming {
            stream,
            addr,
            handshake,
        })
        .map_err(|_| crate::error::Error::Peer(format!("{addr}: no room for another peer")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn hash(byte: u8) -> InfoHash {
        InfoHash::new([byte; 20])
    }

    /// Dial the listener and send a handshake for `info_hash`.
    async fn dial(port: u16, info_hash: InfoHash) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(&Handshake::new(info_hash, *b"-TEST01-000000000000").encode())
            .await
            .unwrap();
        stream
    }

    #[tokio::test]
    async fn a_connection_reaches_the_torrent_it_asked_for() {
        let inbound = Inbound::bind(0).await.unwrap();
        let (_registration, mut rx) = inbound.register(hash(1));

        let _client = dial(inbound.port(), hash(1)).await;

        let incoming = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("routed in time")
            .expect("a connection");
        assert_eq!(incoming.handshake.info_hash, hash(1));
    }

    #[tokio::test]
    async fn the_port_actually_bound_is_the_one_reported() {
        // Announcing a port we are not on is the exact fault this fixes.
        let inbound = Inbound::bind(0).await.unwrap();
        assert_ne!(inbound.port(), 0);
        // And it is dialable, which is the only proof that matters.
        let _client = dial(inbound.port(), hash(2)).await;
    }

    #[tokio::test]
    async fn a_torrent_nobody_is_running_gets_no_answer() {
        let inbound = Inbound::bind(0).await.unwrap();
        let (_registration, mut rx) = inbound.register(hash(1));

        let _client = dial(inbound.port(), hash(9)).await;

        // Nothing should arrive for the torrent we do run.
        let waited = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(waited.is_err(), "a stranger's torrent must not be routed");
    }

    #[tokio::test]
    async fn dropping_the_registration_stops_the_connections() {
        let inbound = Inbound::bind(0).await.unwrap();
        let (registration, mut rx) = inbound.register(hash(1));
        drop(registration);

        let _client = dial(inbound.port(), hash(1)).await;

        // Dropping the guard drops the sender with it, so the receiver closes
        // rather than merely going quiet. Either way nothing is delivered.
        let waited = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            matches!(waited, Ok(None) | Err(_)),
            "the claim was given up, so no connection should arrive"
        );
    }
}
