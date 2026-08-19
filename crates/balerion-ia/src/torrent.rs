//! Fetching archive.org's auto-derived `.torrent` files.
//!
//! Parsing lives in [`balerion_bt::metainfo`]; there is nothing archive.org
//! specific about a metainfo file, and the engine has to parse them anyway.

pub use balerion_bt::metainfo::{FileSlice, IA_TRACKERS, Metainfo, TorrentFile};

use crate::client::IaClient;
use crate::error::{Error, Result};
use crate::metadata::ItemMetadata;

/// Download and parse an item's derived `.torrent`.
pub async fn fetch(client: &IaClient, item: &ItemMetadata) -> Result<Metainfo> {
    let raw = fetch_raw(client, item).await?;
    Ok(Metainfo::parse(&raw)?)
}

/// The raw `.torrent` bytes, for when you want to write them to disk as well.
pub async fn fetch_raw(client: &IaClient, item: &ItemMetadata) -> Result<Vec<u8>> {
    let url = item.torrent_url().ok_or_else(|| Error::Missing {
        identifier: item.identifier.clone(),
        what: "derived .torrent".into(),
    })?;
    client.get(&url).await
}
