//! Converting a container browsers cannot open, offline.
//!
//! A real MPEG program stream carrying MPEG-2 video and AC-3 audio, which is
//! what a great deal of the Internet Archive's older material actually is, is
//! generated with ffmpeg, served from a fake webseed, and then played through
//! balerion's segment endpoints.
//!
//! Every test here is skipped when ffmpeg is missing, so the suite still
//! passes on a machine without it. That is the same condition the feature
//! itself degrades under.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;

use balerion_bt::Strategy;
use balerion_bt::metainfo::Metainfo;
use balerion_bt::session::{self, DownloadConfig, VerifyPolicy};
use balerion_web::state::{AppState, ServeConfig, Torrent};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PIECE_LENGTH: usize = 16 * 1024;

fn have_ffmpeg() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Build a short MPEG program stream: mpeg2video plus ac3, in neither of which
/// any browser has the slightest interest.
fn build_clip(path: &std::path::Path) -> Vec<u8> {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25:duration=20",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=20",
            "-c:v",
            "mpeg2video",
            "-b:v",
            "600k",
            "-c:a",
            "ac3",
            "-ac",
            "2",
            "-f",
            "mpeg",
            "-y",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg should run");
    assert!(status.success(), "could not build the fixture clip");
    std::fs::read(path).expect("fixture should exist")
}

fn bstr(s: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", s.len()).into_bytes();
    out.extend_from_slice(s);
    out
}

fn build_torrent(name: &str, data: &[u8]) -> Metainfo {
    let mut hashes = Vec::new();
    for chunk in data.chunks(PIECE_LENGTH) {
        hashes.extend_from_slice(&Sha1::digest(chunk));
    }
    let mut info = Vec::new();
    info.extend(b"d");
    info.extend(bstr(b"files"));
    info.extend(b"l");
    info.extend(b"d");
    info.extend(bstr(b"length"));
    info.extend(format!("i{}e", data.len()).into_bytes());
    info.extend(bstr(b"path"));
    info.extend(b"l");
    info.extend(bstr(name.as_bytes()));
    info.extend(b"e");
    info.extend(b"e");
    info.extend(b"e");
    info.extend(bstr(b"name"));
    info.extend(bstr(b"a-clip"));
    info.extend(bstr(b"piece length"));
    info.extend(format!("i{PIECE_LENGTH}e").into_bytes());
    info.extend(bstr(b"pieces"));
    info.extend(bstr(&hashes));
    info.extend(b"e");
    Metainfo::from_info_dict(&info).expect("fixture torrent is valid")
}

/// A webseed that answers ranges, which is all BEP 19 needs.
async fn spawn_webseed(name: String, data: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let payload = Arc::new((name, data));

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let payload = Arc::clone(&payload);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let read = match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let range = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|line| line.split("bytes=").nth(1).map(str::trim))
                    .and_then(|spec| {
                        let (from, to) = spec.split_once('-')?;
                        Some((from.parse::<usize>().ok()?, to.parse::<usize>().ok()?))
                    });

                let data = &payload.1;
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

struct Harness {
    base: String,
    hash: String,
    _dir: tempfile::TempDir,
}

/// A running balerion with one unplayable clip, fully downloaded.
async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let clip = build_clip(&dir.path().join("source.mpeg"));
    let name = "clip.mpeg".to_string();

    let seed = spawn_webseed(name.clone(), clip.clone()).await;
    let mut meta = build_torrent(&name, &clip);
    meta.webseeds = vec![format!("http://{seed}/")];

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

    // The clip is tiny and comes from a local webseed, so waiting for the lot
    // keeps the transcode tests about transcoding rather than about timing.
    for piece in 0..meta.piece_count() {
        assert!(
            handle.wait_for_piece(piece).await,
            "the fixture should download in full"
        );
    }

    let hash = meta.info_hash.to_hex();
    let info_hash = meta.info_hash;
    let mut state = AppState::new(ServeConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    });
    state.tools = balerion_web::ffmpeg::Tools::detect().await;
    let state = Arc::new(state);
    // No refill loop in the fixture: nothing here has a tracker to ask.
    state.insert(
        info_hash,
        Arc::new(Torrent::new(
            meta,
            handle,
            task,
            tokio::spawn(async {}),
            root,
            Arc::new(balerion_web::state::Clock::started(
                std::time::Instant::now(),
            )),
        )),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    *state.self_base.lock().unwrap() = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, balerion_web::router(state)).await;
    });

    Harness {
        base: format!("http://{addr}"),
        hash,
        _dir: dir,
    }
}

/// Walk MP4 box types.
fn box_types(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= data.len() {
        let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        out.push(String::from_utf8_lossy(&data[offset + 4..offset + 8]).into_owned());
        if size < 8 {
            break;
        }
        offset += size;
    }
    out
}

#[tokio::test]
async fn an_unplayable_container_is_offered_as_a_transcode() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let h = harness().await;

    let info: serde_json::Value = reqwest::get(format!("{}/api/play/{}/0", h.base, h.hash))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        info["mode"], "transcode",
        "an MPEG program stream needs converting"
    );
    // Both streams are unplayable, so neither can be passed through.
    assert_eq!(info["remux_only"], false);
    assert_eq!(
        info["mime"], "video/mp4; codecs=\"avc1.640033,mp4a.40.2\"",
        "the codec string must match what the transcode actually produces"
    );
    assert!(info["duration"].as_f64().unwrap() > 19.0);
    assert!(info["segments"].as_u64().unwrap() >= 4);
}

#[tokio::test]
async fn the_init_segment_carries_the_header_and_nothing_else() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let h = harness().await;

    let response = reqwest::get(format!("{}/api/play/{}/0/init.mp4", h.base, h.hash))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("content-type").unwrap(), "video/mp4");

    let body = response.bytes().await.unwrap();
    let boxes = box_types(&body);
    assert_eq!(boxes.first().map(String::as_str), Some("ftyp"));
    assert!(boxes.contains(&"moov".to_string()), "boxes: {boxes:?}");
    assert!(
        !boxes.contains(&"moof".to_string()),
        "a fragment leaked into the init segment: {boxes:?}"
    );
}

#[tokio::test]
async fn media_segments_are_fragments_and_can_be_fetched_out_of_order() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let h = harness().await;

    // Segment 2 first: seeking must not require having fetched 0 and 1, which
    // is the whole point of generating them independently.
    for index in [2u32, 0] {
        let response = reqwest::get(format!("{}/api/play/{}/0/seg/{index}", h.base, h.hash))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "segment {index}");

        let body = response.bytes().await.unwrap();
        let boxes = box_types(&body);
        assert_eq!(
            boxes.first().map(String::as_str),
            Some("moof"),
            "segment {index} should start with a fragment, got {boxes:?}"
        );
        assert!(boxes.contains(&"mdat".to_string()), "segment {index}");
        assert!(!body.is_empty());
    }
}

#[tokio::test]
async fn init_and_a_segment_together_decode_as_real_video() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let h = harness().await;

    let init = reqwest::get(format!("{}/api/play/{}/0/init.mp4", h.base, h.hash))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let segment = reqwest::get(format!("{}/api/play/{}/0/seg/1", h.base, h.hash))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    // The real proof: glue them together and check ffprobe sees H.264 and AAC.
    // Anything less would pass while handing the browser something it stalls on.
    let dir = tempfile::tempdir().unwrap();
    let joined = dir.path().join("joined.mp4");
    let mut bytes = init.to_vec();
    bytes.extend_from_slice(&segment);
    std::fs::write(&joined, &bytes).unwrap();

    let output = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&joined)
        .output()
        .unwrap();
    let codecs = String::from_utf8_lossy(&output.stdout);
    assert!(codecs.contains("h264"), "expected H.264, got: {codecs}");
    assert!(codecs.contains("aac"), "expected AAC, got: {codecs}");
}

#[tokio::test]
async fn a_segment_past_the_end_fails_rather_than_serving_nonsense() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let h = harness().await;

    // A 20 second clip has no segment 900. Serving an empty body would wedge
    // the SourceBuffer with no explanation.
    let response = reqwest::get(format!("{}/api/play/{}/0/seg/900", h.base, h.hash))
        .await
        .unwrap();
    assert!(
        response.status().is_server_error() || response.status().is_client_error(),
        "expected a failure, got {}",
        response.status()
    );
}

#[tokio::test]
async fn unknown_torrents_and_files_are_refused() {
    if !have_ffmpeg() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let h = harness().await;

    let missing_file = reqwest::get(format!("{}/api/play/{}/99", h.base, h.hash))
        .await
        .unwrap();
    assert_eq!(missing_file.status(), 404);

    let bad_hash = reqwest::get(format!("{}/api/play/not-a-hash/0", h.base))
        .await
        .unwrap();
    assert_eq!(bad_hash.status(), 404);
}
