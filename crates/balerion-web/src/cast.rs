//! A second listener, for televisions.
//!
//! A television is not a browser tab. An Apple TV or a Chromecast is a separate
//! box on the network: it is handed a URL and fetches the media itself, so
//! nothing can be cast that does not exist as a resource it can reach.
//! `127.0.0.1` is not such an address, and the obvious fix, binding the whole
//! player to the LAN, hands `/api/resolve` to everyone on the wifi. That
//! endpoint downloads whatever magnet it is given.
//!
//! So this serves the media and nothing else: byte ranges of files already being
//! fetched, and the playlist and segments that go with them. It cannot start a
//! download, cannot stop one, cannot list what is on disk and cannot see the
//! library. The worst anyone on the network can do with it is watch something
//! you are already watching, which is the point.
//!
//! It shares [`AppState`] with the player, so a file cast to the television is
//! the same download the browser is reading, not a second copy of it.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;

use crate::state::AppState;
use crate::{play, stream};

/// The routes a television needs, and not one more.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // The playlist, for anything that has to be converted.
        .route("/api/play/{hash}/{file}/index.m3u8", get(play::playlist))
        .route("/api/play/{hash}/{file}/init.mp4", get(play::init))
        .route("/api/play/{hash}/{file}/seg/{index}", get(play::segment))
        .route("/api/play/{hash}/{file}/subs/{track}", get(play::embedded))
        .route("/api/subtitles/{hash}/{file}", get(play::sidecar))
        // The file itself, for anything a television can already open.
        .route("/stream/{hash}/{file}", get(stream::handler))
        .with_state(state)
}

/// Serve the media endpoints on `address` until the process ends.
pub async fn serve(state: Arc<AppState>, address: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind the cast listener to {address}"))?;
    let bound = listener.local_addr().unwrap_or(address);

    // Said plainly rather than buried: this one is meant to be reachable, and
    // whoever runs it should know exactly what it will and will not do.
    println!("casting from http://{bound} (media only: no downloads can be started through it)");

    axum::serve(listener, router(state))
        .await
        .context("the cast listener stopped unexpectedly")?;
    Ok(())
}

/// The address a television should be given for this machine.
///
/// A loopback address is useless to another device, and `0.0.0.0` is not an
/// address at all, so neither can be handed to a television. This finds the
/// LAN address the machine actually answers on, by asking the routing table
/// where it would send a packet rather than by enumerating interfaces and
/// guessing which one matters.
pub fn lan_address() -> Option<std::net::IpAddr> {
    // No packet is sent: connecting a UDP socket only fixes the local end.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let address = socket.local_addr().ok()?.ip();
    if address.is_loopback() || address.is_unspecified() {
        return None;
    }
    Some(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lan_address_is_one_another_device_could_use() {
        // On a machine with no network this is legitimately None, so the
        // assertion is about what it must never be rather than what it is.
        if let Some(address) = lan_address() {
            assert!(
                !address.is_loopback(),
                "{address} is no use to a television"
            );
            assert!(!address.is_unspecified(), "{address} is not an address");
        }
    }
}
