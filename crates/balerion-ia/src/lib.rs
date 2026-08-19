//! Internet Archive client: metadata, search and derived torrents.
//!
//! ```no_run
//! # async fn demo() -> Result<(), balerion_ia::Error> {
//! use balerion_ia::{AdvancedQuery, IaClient, advanced, metadata, torrent};
//!
//! let client = IaClient::new()?;
//! let query = AdvancedQuery::new(r#"jazz AND mediatype:audio AND format:"Archive BitTorrent""#);
//! let hits = advanced::collect(&client, &query, 10, |_, _| {}).await?;
//!
//! let item = metadata::fetch(&client, &hits[0].identifier).await?;
//! let meta = torrent::fetch(&client, &item).await?;
//! println!("{}", meta.magnet_uri());
//! # Ok(())
//! # }
//! ```

pub mod advanced;
pub mod client;
pub mod error;
pub mod metadata;
pub mod search;
pub mod torrent;

pub use advanced::{AdvancedPage, AdvancedQuery};
pub use client::{ClientConfig, IaClient};
pub use error::{Error, Result};
pub use metadata::{IaFile, ItemMetadata, Meta};
pub use search::{SearchHit, SearchPage, SearchQuery};
pub use torrent::{Metainfo, TorrentFile};
