//! OpenSubtitles: finding a subtitle file that matches this exact release.
//!
//! Deliberately the whole of the crate's remit. It knows how to identify a
//! file, how to ask for subtitles for it, and how to fetch one. It knows
//! nothing about torrents, about players, or about putting a track back in step
//! with the speech, which is [`balerion_web::subsync`]'s job and a different
//! problem entirely.
//!
//! **The quota is the thing to know about.** An API key is required, and the
//! free tiers are small: five downloads a day anonymously and ten for a
//! registered account, against a rate limit measured per ten seconds. That is a
//! household's evening, not a service. Two consequences follow, and both are
//! designed for rather than discovered later: anything fetched is cached
//! locally for ever, and this is one source among several rather than the
//! answer.
//!
//! Without a key configured, every entry point here reports that plainly and
//! balerion carries on with the subtitles it can find in the torrent.

pub mod client;
pub mod error;
pub mod hash;
pub mod search;

pub use client::{ClientConfig, OsdbClient};
pub use error::{Error, Result};
pub use search::{Match, Query};
