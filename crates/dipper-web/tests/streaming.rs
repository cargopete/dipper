//! Serving a video that has not finished downloading, offline.
//!
//! A local HTTP server stands in for a BEP 19 webseed, dipper downloads from
//! it in streaming mode, and the range endpoint is driven the way a browser
//! drives it. Nothing here touches the network.

use std::net::SocketAddr;
use std::sync::Arc;

use dipper_bt::Strategy;
use dipper_bt::metainfo::Metainfo;
use dipper_bt::session::{self, DownloadConfig, VerifyPolicy};
use dipper_web::state::{AppState, ServeConfig, Torrent};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PIECE_LENGTH: usize = 1024;
const FILE_ONE: usize = 3000;
const FILE_TWO: usize = 2000;

fn bstr(s: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", s.len()).into_bytes();
    out.extend_from_slice(s);
    out
}

fn content(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + salt as usize) % 251) as u8)
        .collect()
}

fn build_torrent(files: &[(&str, Vec<u8>)]) -> Metainfo {
    let flat: Vec<u8> = files.iter().flat_map(|(_, data)| data.clone()).collect();
    let mut hashes = Vec::new();
    for chunk in flat.chunks(PIECE_LENGTH) {
        hashes.extend_from_slice(&Sha1::digest(chunk));
    }

    let mut info = Vec::new();
    info.extend(b"d");
    info.extend(bstr(b"files"));
    info.extend(b"l");
    for (name, data) in files {
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(format!("i{}e", data.len()).into_bytes());
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(name.as_bytes()));
        info.extend(b"e");
        info.extend(b"e");
    }
    info.extend(b"e");
    info.extend(bstr(b"name"));
    info.extend(bstr(b"an-item"));
    info.extend(bstr(b"piece length"));
    info.extend(format!("i{PIECE_LENGTH}e").into_bytes());
    info.extend(bstr(b"pieces"));
    info.extend(bstr(&hashes));
    info.extend(b"e");

    Metainfo::from_info_dict(&info).expect("fixture torrent is valid")
}

/// The smallest thing that counts as a webseed: GET with `Range`.
async fn spawn_webseed(files: Vec<(String, Vec<u8>)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let files = Arc::new(files);

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let files = Arc::clone(&files);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let read = match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/')
                    .to_string();
                let key = path.strip_prefix("an-item/").unwrap_or(&path).to_string();

                let Some((_, data)) = files.iter().find(|(name, _)| *name == key) else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                };

                let range = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|line| line.split("bytes=").nth(1).map(str::trim))
                    .and_then(|spec| {
                        let (from, to) = spec.split_once('-')?;
                        Some((from.parse::<usize>().ok()?, to.parse::<usize>().ok()?))
                    });

                let (status, body) = match range {
                    Some((from, to)) if from <= to && to < data.len() => {
                        ("206 Partial Content", data[from..=to].to_vec())
                    }
                    _ => ("200 OK", data.clone()),
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    addr
}

/// A torrent being downloaded from a local webseed, served by a live dipper.
struct Harness {
    base: String,
    hash: String,
    one: Vec<u8>,
    two: Vec<u8>,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let one = content(FILE_ONE, 1);
    let two = content(FILE_TWO, 2);
    // `.mp4` so the server treats it as something to play rather than
    // something to hand over as a file.
    let files = vec![("feature.mp4", one.clone()), ("notes.txt", two.clone())];

    let seed = spawn_webseed(
        files
            .iter()
            .map(|(name, data)| (name.to_string(), data.clone()))
            .collect(),
    )
    .await;

    let mut meta = build_torrent(&files);
    meta.webseeds = vec![format!("http://{seed}/")];

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(meta.info_hash.to_hex());
    tokio::fs::create_dir_all(&root).await.unwrap();

    let (handle, task) = session::spawn(
        &meta,
        &root,
        vec![],
        &DownloadConfig {
            strategy: Strategy::Streaming,
            verify: VerifyPolicy::Never,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let hash = meta.info_hash.to_hex();
    let info_hash = meta.info_hash;
    let state = Arc::new(AppState::new(ServeConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    }));
    state.insert(
        info_hash,
        // No refill loop in the fixture: there is no tracker to ask and the
        // test drives the engine directly.
        Arc::new(Torrent::new(
            meta,
            handle,
            task,
            tokio::spawn(async {}),
            root.clone(),
        )),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, dipper_web::router(state)).await;
    });

    Harness {
        base: format!("http://{addr}"),
        hash,
        one,
        two,
        _dir: dir,
    }
}

#[tokio::test]
async fn a_mid_file_range_is_served_with_the_right_bytes_and_headers() {
    let h = harness().await;
    let client = reqwest::Client::new();

    // Straddles the piece boundary at 2048, which is where a naive
    // implementation stitches the pieces together wrongly.
    let response = client
        .get(format!("{}/stream/{}/0", h.base, h.hash))
        .header("Range", "bytes=1500-2499")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 206);
    assert_eq!(
        response.headers().get("content-range").unwrap(),
        &format!("bytes 1500-2499/{FILE_ONE}")
    );
    assert_eq!(response.headers().get("accept-ranges").unwrap(), "bytes");
    assert_eq!(response.headers().get("content-type").unwrap(), "video/mp4");
    assert_eq!(response.headers().get("content-length").unwrap(), "1000");

    let body = response.bytes().await.unwrap();
    assert_eq!(body.len(), 1000);
    assert_eq!(&body[..], &h.one[1500..2500], "wrong bytes for the range");
}

#[tokio::test]
async fn a_suffix_range_reaches_the_tail_of_the_file() {
    // The request that decides whether a non-faststart MP4 plays at all: the
    // player goes looking for the index box at the very end.
    let h = harness().await;
    let response = reqwest::Client::new()
        .get(format!("{}/stream/{}/0", h.base, h.hash))
        .header("Range", "bytes=-200")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 206);
    assert_eq!(
        response.headers().get("content-range").unwrap(),
        &format!("bytes {}-{}/{FILE_ONE}", FILE_ONE - 200, FILE_ONE - 1)
    );
    assert_eq!(response.bytes().await.unwrap()[..], h.one[FILE_ONE - 200..]);
}

#[tokio::test]
async fn a_second_file_in_the_torrent_is_offset_correctly() {
    // The second file starts partway through a piece, so serving it from the
    // wrong global offset would return plausible-looking rubbish.
    let h = harness().await;
    let response = reqwest::Client::new()
        .get(format!("{}/stream/{}/1", h.base, h.hash))
        .header("Range", "bytes=0-99")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 206);
    assert_eq!(response.bytes().await.unwrap()[..], h.two[..100]);
}

#[tokio::test]
async fn no_range_header_yields_the_whole_file() {
    let h = harness().await;
    let response = reqwest::get(format!("{}/stream/{}/0", h.base, h.hash))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(response.headers().get("content-range").is_none());
    assert_eq!(
        response.headers().get("content-length").unwrap(),
        &FILE_ONE.to_string()
    );
    assert_eq!(response.bytes().await.unwrap()[..], h.one[..]);
}

#[tokio::test]
async fn a_range_past_the_end_is_refused_with_416() {
    let h = harness().await;
    let response = reqwest::Client::new()
        .get(format!("{}/stream/{}/0", h.base, h.hash))
        .header("Range", "bytes=99999-")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 416);
    assert_eq!(
        response.headers().get("content-range").unwrap(),
        &format!("bytes */{FILE_ONE}")
    );
}

#[tokio::test]
async fn unknown_torrents_and_files_are_not_found_rather_than_a_panic() {
    let h = harness().await;

    let missing_file = reqwest::get(format!("{}/stream/{}/99", h.base, h.hash))
        .await
        .unwrap();
    assert_eq!(missing_file.status(), 404);

    let missing_torrent = reqwest::get(format!(
        "{}/stream/0000000000000000000000000000000000000000/0",
        h.base
    ))
    .await
    .unwrap();
    assert_eq!(missing_torrent.status(), 404);

    let nonsense = reqwest::get(format!("{}/stream/not-a-hash/0", h.base))
        .await
        .unwrap();
    assert_eq!(nonsense.status(), 400);
}

#[tokio::test]
async fn the_stats_endpoint_describes_the_running_download() {
    let h = harness().await;

    // Pull some bytes so there is progress to report.
    let _ = reqwest::Client::new()
        .get(format!("{}/stream/{}/0", h.base, h.hash))
        .header("Range", "bytes=0-999")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    let stats: serde_json::Value = reqwest::get(format!("{}/api/torrents/{}", h.base, h.hash))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(stats["name"], "an-item");
    assert_eq!(stats["pieces_total"], 5);
    assert!(stats["pieces_have"].as_u64().unwrap() >= 1);
    // Runs are the piece map, and must always account for every piece.
    let runs: Vec<u64> = serde_json::from_value(stats["runs"].clone()).unwrap();
    assert_eq!(runs.iter().sum::<u64>(), 5);
}

#[tokio::test]
async fn the_page_and_its_assets_are_served() {
    let h = harness().await;

    let page = reqwest::get(&h.base).await.unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("<title>dipper</title>"));
    // The em-dash ban is a house rule for this page, so it gets a test.
    assert!(!html.contains('\u{2014}'), "no em-dashes on the page");

    for (path, expected) in [("/app.css", "text/css"), ("/app.js", "text/javascript")] {
        let asset = reqwest::get(format!("{}{path}", h.base)).await.unwrap();
        assert_eq!(asset.status(), 200, "{path}");
        assert!(
            asset
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(expected),
            "{path} served with the wrong type"
        );
    }
}
