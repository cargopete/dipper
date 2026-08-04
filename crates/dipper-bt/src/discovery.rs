//! Peer discovery: ask every source at once and take the union.
//!
//! Trackers time out, DHT lookups converge slowly, and the magnet's own
//! `x.pe` peers may be stale. Running them concurrently means the fast source
//! is not held up by the slow one, which for a cold magnet is the difference
//! between two seconds and thirty.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dht::Dht;
use crate::infohash::InfoHash;
use crate::magnet::Magnet;
use crate::tracker::{self, Announce};

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// How long a single tracker gets before we give up on it.
    pub tracker_timeout: Duration,
    /// How long to keep the DHT lookup running.
    pub dht_budget: Duration,
    pub use_dht: bool,
    /// The port we claim to listen on when announcing.
    pub port: u16,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            tracker_timeout: Duration::from_secs(10),
            dht_budget: Duration::from_secs(20),
            use_dht: true,
            port: 6881,
        }
    }
}

/// Where a peer address came from, for logging and for deciding who to blame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Magnet,
    Tracker,
    Dht,
    Pex,
}

/// A running search for peers. Results accumulate; call [`Discovery::peers`]
/// whenever you want the current set.
pub struct Discovery {
    info_hash: InfoHash,
    found: Arc<Mutex<HashSet<SocketAddr>>>,
}

impl Discovery {
    pub fn new(info_hash: InfoHash) -> Self {
        Self {
            info_hash,
            found: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn peers(&self) -> Vec<SocketAddr> {
        let mut peers: Vec<SocketAddr> = self.found.lock().unwrap().iter().copied().collect();
        peers.sort();
        peers
    }

    pub fn len(&self) -> usize {
        self.found.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add peers from any source, returning how many were new.
    pub fn add(&self, peers: impl IntoIterator<Item = SocketAddr>) -> usize {
        let mut found = self.found.lock().unwrap();
        peers
            .into_iter()
            .filter(|addr| addr.port() != 0)
            .filter(|addr| found.insert(*addr))
            .count()
    }

    /// Run every source concurrently until they are all done or out of budget.
    ///
    /// `on_found` is called with the source and the newly-discovered peers, so
    /// a caller can start connecting before discovery finishes.
    pub async fn run<F>(
        &self,
        magnet: &Magnet,
        peer_id: [u8; 20],
        left: u64,
        dht: Option<&Dht>,
        config: &DiscoveryConfig,
        mut on_found: F,
    ) where
        F: FnMut(Source, &[SocketAddr]) + Send,
    {
        // The magnet's own peers cost nothing to try.
        if !magnet.peers.is_empty() {
            let fresh = self.add(magnet.peers.iter().copied());
            if fresh > 0 {
                on_found(Source::Magnet, &magnet.peers);
            }
        }

        let mut request = Announce::new(self.info_hash, peer_id, left);
        request.port = config.port;

        let tracker_jobs = magnet.trackers.iter().map(|url| {
            let url = url.clone();
            let request = request.clone();
            let timeout = config.tracker_timeout;
            async move {
                match tracker::announce(&url, &request, timeout).await {
                    Ok(response) => {
                        tracing::debug!(
                            tracker = url,
                            peers = response.peers.len(),
                            seeders = response.seeders,
                            "tracker answered"
                        );
                        response.peers
                    }
                    Err(err) => {
                        tracing::debug!(tracker = url, %err, "tracker did not answer");
                        Vec::new()
                    }
                }
            }
        });

        let trackers = futures_util::future::join_all(tracker_jobs);

        // The DHT writes into the shared set as batches arrive rather than at
        // the end, so a slow lookup still feeds the connector early.
        let dht_lookup = async {
            match dht.filter(|_| config.use_dht) {
                Some(dht) => {
                    dht.get_peers(self.info_hash, config.dht_budget, |batch| {
                        self.add(batch.iter().copied());
                    })
                    .await
                }
                None => Vec::new(),
            }
        };

        let (tracker_results, dht_peers) = futures_util::future::join(trackers, dht_lookup).await;

        for peers in tracker_results {
            if !peers.is_empty() && self.add(peers.iter().copied()) > 0 {
                on_found(Source::Tracker, &peers);
            }
        }
        if !dht_peers.is_empty() {
            on_found(Source::Dht, &dht_peers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> InfoHash {
        InfoHash::parse("15c74d4165fc2ffff997d576bf44b4b25cbeb04e").unwrap()
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn deduplicates_across_sources() {
        let discovery = Discovery::new(hash());
        assert_eq!(discovery.add([addr("1.2.3.4:6881"), addr("5.6.7.8:80")]), 2);
        assert_eq!(
            discovery.add([addr("1.2.3.4:6881"), addr("9.9.9.9:1")]),
            1,
            "only the unseen peer counts"
        );
        assert_eq!(discovery.len(), 3);
    }

    #[test]
    fn ignores_port_zero() {
        let discovery = Discovery::new(hash());
        assert_eq!(discovery.add([addr("1.2.3.4:0")]), 0);
        assert!(discovery.is_empty());
    }

    #[test]
    fn peers_come_back_sorted_for_stable_output() {
        let discovery = Discovery::new(hash());
        discovery.add([addr("9.9.9.9:1"), addr("1.1.1.1:2"), addr("5.5.5.5:3")]);
        let peers = discovery.peers();
        assert_eq!(peers[0], addr("1.1.1.1:2"));
        assert_eq!(peers[2], addr("9.9.9.9:1"));
    }

    #[tokio::test]
    async fn magnet_peers_are_used_even_with_no_trackers_or_dht() {
        let magnet = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{}&x.pe=1.2.3.4%3A6881",
            hash()
        ))
        .unwrap();
        let discovery = Discovery::new(hash());
        let mut seen = Vec::new();

        discovery
            .run(
                &magnet,
                [0u8; 20],
                0,
                None,
                &DiscoveryConfig {
                    use_dht: false,
                    ..Default::default()
                },
                |source, peers| seen.push((source, peers.to_vec())),
            )
            .await;

        assert_eq!(discovery.peers(), vec![addr("1.2.3.4:6881")]);
        assert_eq!(seen, vec![(Source::Magnet, vec![addr("1.2.3.4:6881")])]);
    }

    #[tokio::test]
    async fn a_dead_tracker_is_survivable() {
        // Port 1 on localhost refuses instantly, which is the fast version of
        // the tracker being down.
        let magnet = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{}&tr=http%3A%2F%2F127.0.0.1%3A1%2Fannounce",
            hash()
        ))
        .unwrap();
        let discovery = Discovery::new(hash());
        discovery
            .run(
                &magnet,
                [0u8; 20],
                0,
                None,
                &DiscoveryConfig {
                    use_dht: false,
                    tracker_timeout: Duration::from_millis(500),
                    ..Default::default()
                },
                |_, _| {},
            )
            .await;
        assert!(discovery.is_empty(), "no peers, but also no panic");
    }
}
