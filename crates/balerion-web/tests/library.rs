//! Restarting balerion, and what it finds in the data directory.
//!
//! The regression here is the one that made "On this machine" untrue: kept
//! torrents used to disappear on restart while their bytes stayed on disk, and
//! abandoned ones became invisible to the sweeper and were never collected at
//! all.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use balerion_bt::Metainfo;
use balerion_web::state::{AppState, ServeConfig, mark_kept};
use sha1::{Digest, Sha1};

const PIECE_LENGTH: usize = 1024;

fn bstr(s: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", s.len()).into_bytes();
    out.extend_from_slice(s);
    out
}

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A single-file torrent whose piece hashes match `data`.
fn torrent_for(name: &str, data: &[u8]) -> Metainfo {
    let mut hashes = Vec::new();
    for chunk in data.chunks(PIECE_LENGTH) {
        hashes.extend_from_slice(&Sha1::digest(chunk));
    }

    let mut info = Vec::new();
    info.extend(b"d");
    info.extend(bstr(b"length"));
    info.extend(format!("i{}e", data.len()).into_bytes());
    info.extend(bstr(b"name"));
    info.extend(bstr(name.as_bytes()));
    info.extend(bstr(b"piece length"));
    info.extend(format!("i{PIECE_LENGTH}e").into_bytes());
    info.extend(bstr(b"pieces"));
    info.extend(bstr(&hashes));
    info.extend(b"e");

    Metainfo::from_info_dict(&info).expect("fixture torrent is valid")
}

/// Lay out a finished download exactly as balerion would have left it.
async fn finished_download(data_dir: &Path, name: &str, kept: bool) -> Metainfo {
    let data = content(2000);
    let meta = torrent_for(name, &data);
    let root = data_dir.join(meta.info_hash.to_hex());
    tokio::fs::create_dir_all(&root).await.unwrap();
    tokio::fs::write(root.join(name), &data).await.unwrap();
    balerion_web::library::remember(&root, &meta).await;
    if kept {
        mark_kept(&root, true).await.unwrap();
    }
    meta
}

/// Backdate everything in a directory, to stand in for time passing.
fn age(root: &Path, by: Duration) {
    let when = SystemTime::now() - by;
    let times = std::fs::FileTimes::new()
        .set_accessed(when)
        .set_modified(when);
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(times).unwrap();
    }
}

fn state_for(data_dir: &Path) -> Arc<AppState> {
    Arc::new(AppState::new(ServeConfig {
        data_dir: data_dir.to_path_buf(),
        // Nothing in this test should touch the network, and a DHT client
        // wants a UDP port that CI may not give us.
        use_dht: false,
        ..Default::default()
    }))
}

#[tokio::test]
async fn a_kept_torrent_comes_back_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let meta = finished_download(dir.path(), "a-film.mp4", true).await;

    let state = state_for(dir.path());
    let found = balerion_web::library::adopt(&state).await;

    assert_eq!(found.kept, 1, "{found:?}");
    assert_eq!(found.swept, 0);

    let torrent = state.get(&meta.info_hash).expect("back in the library");
    assert!(torrent.is_kept(), "the keep marker should have been read");
    assert_eq!(torrent.meta.name, "a-film.mp4");
    // The bytes were already there, so nothing needed downloading and the
    // session should have recognised the lot from disk.
    let stats = torrent.handle.stats();
    assert!(
        stats.is_complete(),
        "resumed {}/{} pieces",
        stats.pieces_have,
        stats.pieces_total
    );
}

#[tokio::test]
async fn an_abandoned_download_is_collected_rather_than_leaked() {
    let dir = tempfile::tempdir().unwrap();
    let meta = finished_download(dir.path(), "forgotten.mp4", false).await;
    let root = dir.path().join(meta.info_hash.to_hex());
    age(&root, Duration::from_secs(60 * 60));

    let state = state_for(dir.path());
    let found = balerion_web::library::adopt(&state).await;

    assert_eq!(found.swept, 1, "{found:?}");
    assert!(!root.exists(), "the directory should be gone");
    assert!(state.get(&meta.info_hash).is_none());
}

#[tokio::test]
async fn a_download_from_a_minute_ago_is_left_alone() {
    // Restarting balerion while watching something must not throw it away.
    let dir = tempfile::tempdir().unwrap();
    let meta = finished_download(dir.path(), "still-warm.mp4", false).await;
    let root = dir.path().join(meta.info_hash.to_hex());

    let state = state_for(dir.path());
    let found = balerion_web::library::adopt(&state).await;

    assert_eq!(found.swept, 0, "{found:?}");
    assert_eq!(found.left, 1);
    assert!(root.exists(), "too recent to collect");
}

#[tokio::test]
async fn a_kept_directory_we_cannot_read_is_never_deleted() {
    // Somebody asked for those bytes to stay. Being unable to describe them is
    // not permission to remove them.
    let dir = tempfile::tempdir().unwrap();
    let meta = finished_download(dir.path(), "unreadable.mp4", true).await;
    let root = dir.path().join(meta.info_hash.to_hex());
    tokio::fs::write(balerion_web::library::sidecar_path(&root), b"not a torrent")
        .await
        .unwrap();
    age(&root, Duration::from_secs(60 * 60));

    let state = state_for(dir.path());
    let found = balerion_web::library::adopt(&state).await;

    assert_eq!(found.kept, 0);
    assert_eq!(found.swept, 0, "{found:?}");
    assert!(
        root.exists(),
        "kept data must survive an unreadable sidecar"
    );
}

#[tokio::test]
async fn strangers_in_the_data_directory_are_left_where_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let odd = dir.path().join("not-an-infohash");
    tokio::fs::create_dir_all(&odd).await.unwrap();
    tokio::fs::write(odd.join("notes.txt"), b"mine")
        .await
        .unwrap();
    age(&odd, Duration::from_secs(60 * 60));

    let state = state_for(dir.path());
    let found = balerion_web::library::adopt(&state).await;

    assert_eq!(found, balerion_web::library::Adopted::default());
    assert!(odd.exists(), "we only own what we named");
}
