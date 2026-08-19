/// Everything the BitTorrent engine can complain about.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bad magnet link: {0}")]
    Magnet(String),

    #[error("malformed bencode: {0}")]
    Bencode(String),

    #[error("malformed torrent: {0}")]
    Metainfo(String),

    #[error("tracker error: {0}")]
    Tracker(String),

    #[error("peer error: {0}")]
    Peer(String),

    /// A peer served metadata whose SHA-1 did not match the infohash we asked
    /// for. Never trust it; this is the only thing standing between a magnet
    /// and an attacker-chosen file layout.
    #[error("metadata from {peer} did not hash to the requested infohash")]
    MetadataMismatch { peer: String },

    #[error("piece {index} failed its hash check")]
    PieceMismatch { index: u32 },

    #[error("no peers found for {info_hash}")]
    NoPeers { info_hash: String },

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
