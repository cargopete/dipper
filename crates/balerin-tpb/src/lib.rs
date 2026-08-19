//! apibay search client: a query in, magnet links out.
//!
//! apibay is the JSON endpoint behind thepiratebay's frontend. The site's HTML
//! page is a shell; this is the request its own JavaScript makes.
//!
//! Deliberately the whole of the crate's remit. It knows how to search and how
//! to spell a magnet, and nothing about downloading: that stays in
//! [`balerin_bt`], which is handed a magnet and never asked where it came from.
//!
//! ```no_run
//! # async fn demo() -> Result<(), balerin_tpb::Error> {
//! use balerin_tpb::{TpbClient, category, search};
//!
//! let client = TpbClient::new()?;
//! let hits = search::search(&client, "sita sings the blues", category::find("video").unwrap().code).await?;
//! for hit in hits {
//!     println!("{:>6} seeders  {}", hit.seeders, hit.name);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! What is on the other end is a public index of whatever strangers uploaded.
//! Most of it is copyrighted, none of it is cleared, and the category label is
//! not a licence. balerin says so in the interface rather than in a comment
//! nobody reads.

pub mod category;
pub mod client;
pub mod error;
pub mod magnet;
pub mod search;

pub use category::{CATEGORIES, Category};
pub use client::{ClientConfig, TpbClient};
pub use error::{Error, Result};
pub use search::Hit;
