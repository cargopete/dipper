//! A BitTorrent engine: magnet link or `.torrent` in, verified bytes out.
//!
//! Deliberately provenance-agnostic. The engine consumes a 20-byte infohash
//! and knows nothing about where it came from.

pub mod bencode;
pub mod dht;
pub mod discovery;
pub mod error;
pub mod extended;
pub mod inbound;
pub mod infohash;
pub mod magnet;
pub mod metainfo;
pub mod mse;
pub mod peer;
pub mod picker;
pub mod resume;
pub mod session;
pub mod storage;
pub mod tracker;
pub mod webseed;
pub mod wire;

pub use dht::Dht;
pub use discovery::{Discovery, DiscoveryConfig};
pub use error::{Error, Result};
pub use inbound::Inbound;
pub use infohash::InfoHash;
pub use magnet::Magnet;
pub use metainfo::{FileSlice, Metainfo, TorrentFile};
pub use picker::{Picker, Strategy};
pub use resume::ResumeState;
pub use session::{
    DownloadConfig, DownloadSummary, Progress, SessionHandle, SessionStats, VerifyPolicy, download,
    spawn, spawn_with_progress,
};
pub use storage::Storage;
