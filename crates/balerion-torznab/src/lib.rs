//! Torznab: one protocol, and every self-hosted indexer that speaks it.
//!
//! This is the highest-leverage thing in balerion's search half, and the reason
//! is arithmetic. Prowlarr, Jackett, Zilean and bitmagnet all speak Torznab, so
//! one client implementation reaches all of them, and reaches them configured
//! by the person running balerion rather than chosen by us.
//!
//! It also sidesteps the problem [`balerion_web::relay`] exists to work around.
//! apibay serves a Cloudflare bot challenge to datacentre addresses, which is
//! why searching it from a hosted deployment needs a relay on somebody's
//! domestic connection. A Torznab indexer runs on the user's own machine by
//! definition: there is no challenge, and the credential is one the indexer
//! issued rather than one we invented.
//!
//! Deliberately the whole of the crate's remit. It knows how to ask an indexer
//! a question and how to spell a magnet, and nothing about downloading, which
//! stays in [`balerion_bt`], which is handed a magnet and never asked where it
//! came from.
//!
//! Without an indexer configured, every entry point here reports that plainly
//! and balerion carries on with the two indexes it has.

pub mod client;
pub mod error;
pub mod search;

pub use client::{Indexer, TorznabClient};
pub use error::{Error, Result};
pub use search::{Answer, Hit, Query};
