//! A BitTorrent engine: magnet link or `.torrent` in, verified bytes out.
//!
//! Deliberately provenance-agnostic. The engine consumes a 20-byte infohash
//! and knows nothing about where it came from.

pub mod bencode;
pub mod error;
pub mod infohash;
pub mod magnet;
pub mod metainfo;

pub use error::{Error, Result};
pub use infohash::InfoHash;
pub use magnet::Magnet;
pub use metainfo::{FileSlice, Metainfo, TorrentFile};
