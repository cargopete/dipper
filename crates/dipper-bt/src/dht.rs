//! Mainline DHT peer discovery (BEP 5), via the `mainline` crate.
//!
//! Note for anyone arriving from libp2p: this is *not* libp2p-kad. Same
//! Kademlia idea, wire-incompatible in every particular — 160-bit SHA-1
//! keyspace instead of SHA-256, bencoded KRPC over UDP instead of
//! length-delimited protobuf over streams, and raw IP:port peer lists instead
//! of `PeerId` provider records. The concepts transfer; no code does.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt;
use mainline::{Dht as Mainline, Id};

use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// A running DHT node. Cloning is cheap; it shares one background actor.
#[derive(Clone)]
pub struct Dht {
    inner: mainline::async_dht::AsyncDht,
}

impl Dht {
    /// Start a client-mode node: it queries the DHT but does not offer to
    /// store routing entries for others. Bootstrapping happens in the
    /// background, so the first lookup may be slower than the rest.
    pub fn client() -> Result<Self> {
        let dht = Mainline::client().map_err(Error::Io)?;
        Ok(Self {
            inner: dht.as_async(),
        })
    }

    /// Start in server mode, which also serves routing queries. Only polite if
    /// you are actually reachable from the internet.
    pub fn server() -> Result<Self> {
        let dht = Mainline::server().map_err(Error::Io)?;
        Ok(Self {
            inner: dht.as_async(),
        })
    }

    /// Wait until the routing table has enough contacts to be useful.
    pub async fn bootstrapped(&self) -> bool {
        self.inner.bootstrapped().await
    }

    /// Look up peers for an infohash, calling `on_peers` as each batch lands.
    ///
    /// Returns everything found, de-duplicated. `budget` caps how long we are
    /// willing to keep looking: DHT lookups converge but never formally
    /// "finish", so the caller decides when enough is enough.
    pub async fn get_peers<F>(
        &self,
        info_hash: InfoHash,
        budget: Duration,
        mut on_peers: F,
    ) -> Vec<SocketAddr>
    where
        F: FnMut(&[SocketAddr]),
    {
        let id = Id::from_bytes(info_hash.as_bytes()).expect("an infohash is exactly 20 bytes");
        let mut found: Vec<SocketAddr> = Vec::new();

        let lookup = async {
            let mut stream = self.inner.get_peers(id);
            while let Some(batch) = stream.next().await {
                let fresh: Vec<SocketAddr> = batch
                    .into_iter()
                    .map(SocketAddr::V4)
                    .filter(|addr| addr.port() != 0 && !found.contains(addr))
                    .collect();
                if !fresh.is_empty() {
                    found.extend(fresh.iter().copied());
                    on_peers(&fresh);
                }
            }
        };

        // A timeout is not an error here: whatever we found is what we use.
        let _ = tokio::time::timeout(budget, lookup).await;
        found
    }

    /// Tell the DHT we are a peer for this infohash, so others can find us.
    pub async fn announce(&self, info_hash: InfoHash, port: Option<u16>) -> Result<()> {
        let id = Id::from_bytes(info_hash.as_bytes()).expect("an infohash is exactly 20 bytes");
        self.inner
            .announce_peer(id, port)
            .await
            .map(|_| ())
            .map_err(|err| Error::Tracker(format!("dht announce failed: {err}")))
    }
}

impl std::fmt::Debug for Dht {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dht")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_infohash_is_a_valid_dht_id() {
        let hash = InfoHash::parse("15c74d4165fc2ffff997d576bf44b4b25cbeb04e").unwrap();
        let id = Id::from_bytes(hash.as_bytes()).unwrap();
        assert_eq!(id.as_bytes(), hash.as_bytes());
    }

    #[tokio::test]
    async fn a_client_node_starts_without_touching_the_network() {
        // Binding a UDP socket is local; bootstrapping is what needs the net.
        let dht = Dht::client().expect("dht starts");
        assert_eq!(format!("{dht:?}"), "Dht");
    }
}
