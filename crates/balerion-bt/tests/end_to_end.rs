//! A whole download, offline: a local HTTP server acts as a BEP 19 webseed
//! and the engine fetches, verifies and assembles a multi-file torrent from
//! it. This exercises the picker, storage, piece/file mapping, verification
//! and the session coordinator together, with no network and no fixtures.

use std::net::SocketAddr;
use std::sync::Arc;

use balerion_bt::metainfo::Metainfo;
use balerion_bt::session::{self, DownloadConfig, Progress};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PIECE_LENGTH: usize = 1024;

fn bstr(s: &[u8]) -> Vec<u8> {
    let mut out = format!("{}:", s.len()).into_bytes();
    out.extend_from_slice(s);
    out
}

/// Deterministic content that is not compressible into a lucky hash collision.
fn content(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + salt as usize) % 251) as u8)
        .collect()
}

/// A three-file torrent whose pieces straddle every boundary.
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
        for segment in name.split('/') {
            info.extend(bstr(segment.as_bytes()));
        }
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

/// The smallest HTTP server that can be a webseed: GET with `Range`.
///
/// `corrupt` makes it serve wrong bytes for one file, so we can prove the
/// engine rejects them rather than writing them to disk.
async fn spawn_webseed(files: Vec<(String, Vec<u8>)>, corrupt: Option<String>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let files = Arc::new(files);

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let files = Arc::clone(&files);
            let corrupt = corrupt.clone();
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
                let path = urlencoding::decode(&path)
                    .map(|p| p.into_owned())
                    .unwrap_or(path);
                // Strip the torrent name that BEP 19 tells clients to append.
                let key = path.strip_prefix("an-item/").unwrap_or(&path).to_string();

                let Some((_, data)) = files.iter().find(|(name, _)| *name == key) else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                };
                let mut data = data.clone();
                if corrupt.as_deref() == Some(key.as_str()) {
                    data.iter_mut().for_each(|byte| *byte ^= 0xff);
                }

                // Parse `Range: bytes=from-to`, if present.
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
                        // Answer a whole-file range with 200 and the whole
                        // file, exactly as archive.org does. The client is
                        // expected to slice it itself.
                        if from == 0 && to == data.len() - 1 {
                            ("200 OK", data.clone())
                        } else {
                            ("206 Partial Content", data[from..=to].to_vec())
                        }
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

fn fixture_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("sub/one.bin".to_string(), content(2500, 1)),
        ("two.bin".to_string(), content(1500, 2)),
        ("three.bin".to_string(), content(300, 3)),
    ]
}

#[tokio::test]
async fn downloads_a_multi_file_torrent_from_a_webseed_and_verifies_every_byte() {
    let files = fixture_files();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.clone()))
        .collect();
    let mut meta = build_torrent(&borrowed);
    assert_eq!(meta.total_length, 4300);
    assert_eq!(
        meta.piece_count(),
        5,
        "pieces must straddle file boundaries"
    );

    let addr = spawn_webseed(files.clone(), None).await;
    meta.webseeds = vec![format!("http://{addr}/")];

    let dir = tempfile::tempdir().unwrap();
    let mut pieces_seen = 0;
    let summary = session::download(
        &meta,
        dir.path(),
        vec![],
        &DownloadConfig::default(),
        |update| {
            if matches!(update, Progress::Piece { .. }) {
                pieces_seen += 1;
            }
        },
    )
    .await
    .expect("download completes");

    assert_eq!(summary.pieces, 5);
    assert_eq!(summary.bytes, 4300);
    assert_eq!(summary.from_webseeds, 5);
    assert_eq!(summary.from_peers, 0);
    assert_eq!(summary.failed_hashes, 0);
    assert_eq!(pieces_seen, 5);

    // Every file must be byte-identical to what the server holds.
    for (name, expected) in &files {
        let path = dir.path().join("an-item").join(name);
        let got = tokio::fs::read(&path)
            .await
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        assert_eq!(&got, expected, "{name} differs");
    }
}

#[tokio::test]
async fn a_corrupt_webseed_is_refused_rather_than_written() {
    let files = fixture_files();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.clone()))
        .collect();
    let mut meta = build_torrent(&borrowed);

    // This server flips every bit of the first file.
    let addr = spawn_webseed(files.clone(), Some("sub/one.bin".to_string())).await;
    meta.webseeds = vec![format!("http://{addr}/")];

    let dir = tempfile::tempdir().unwrap();
    let mut failures = 0;
    let result = session::download(
        &meta,
        dir.path(),
        vec![],
        &DownloadConfig::default(),
        |update| {
            if matches!(update, Progress::PieceFailed { .. }) {
                failures += 1;
            }
        },
    )
    .await;

    assert!(
        result.is_err(),
        "a corrupt source must not count as success"
    );
    assert!(failures > 0, "the bad pieces should have been reported");

    // The corrupted bytes must never have reached the file.
    let path = dir.path().join("an-item/sub/one.bin");
    let on_disk = tokio::fs::read(&path).await.unwrap();
    assert_ne!(
        on_disk,
        files[0].1.iter().map(|b| b ^ 0xff).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_completed_download_resumes_as_a_no_op() {
    let files = fixture_files();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.clone()))
        .collect();
    let mut meta = build_torrent(&borrowed);
    let files_check = files.clone();
    let addr = spawn_webseed(files, None).await;
    meta.webseeds = vec![format!("http://{addr}/")];

    let dir = tempfile::tempdir().unwrap();
    let config = DownloadConfig::default();
    session::download(&meta, dir.path(), vec![], &config, |_| {})
        .await
        .unwrap();

    for (name, expected) in &files_check {
        let path = dir.path().join("an-item").join(name);
        let got = tokio::fs::read(&path).await.unwrap();
        assert_eq!(&got, expected, "after the first download, {name} differs");
    }

    let mut resumed = None;
    let mut rechecks = 0;
    let second = session::download(&meta, dir.path(), vec![], &config, |update| match update {
        Progress::Resumed {
            have,
            bytes,
            rehashed,
            ..
        } => resumed = Some((have, bytes, rehashed)),
        Progress::Verifying { .. } => rechecks += 1,
        _ => {}
    })
    .await
    .expect("a finished download re-runs cleanly");

    assert_eq!(second.pieces, meta.piece_count());
    assert_eq!(second.bytes, 0, "nothing should be fetched twice");
    assert_eq!(
        resumed,
        Some((5, 4300, false)),
        "the resume file should be trusted, with real byte counts"
    );
    assert_eq!(
        rechecks, 0,
        "a clean resume file means no re-hashing at all"
    );
}

#[tokio::test]
async fn a_resume_file_survives_being_killed_mid_download() {
    use balerion_bt::resume::ResumeState;
    use balerion_bt::session::VerifyPolicy;

    let files = fixture_files();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.clone()))
        .collect();
    let mut meta = build_torrent(&borrowed);
    let addr = spawn_webseed(files, None).await;
    meta.webseeds = vec![format!("http://{addr}/")];

    let dir = tempfile::tempdir().unwrap();
    session::download(
        &meta,
        dir.path(),
        vec![],
        &DownloadConfig::default(),
        |_| {},
    )
    .await
    .unwrap();

    let state = ResumeState::load(dir.path(), &meta)
        .await
        .expect("a resume file was written");
    assert!(state.clean, "a finished download leaves a clean state");
    assert!(state.have.is_complete());

    // Now simulate the kill: mark the state unclean, as it would be if we had
    // been killed between flushes.
    let unclean = ResumeState::new(&meta, state.have.clone(), false);
    unclean.save(dir.path()).await.unwrap();

    let mut rechecks = 0;
    let mut resumed = None;
    session::download(
        &meta,
        dir.path(),
        vec![],
        &DownloadConfig::default(),
        |update| match update {
            Progress::Verifying { .. } => rechecks += 1,
            Progress::Resumed { have, rehashed, .. } => resumed = Some((have, rehashed)),
            _ => {}
        },
    )
    .await
    .unwrap();

    assert!(
        rechecks > 0,
        "an unclean state must fall back to re-hashing"
    );
    assert_eq!(
        resumed,
        Some((5, true)),
        "re-hashing should find everything that is genuinely there"
    );

    // And an explicit --verify ignores the (now clean again) file entirely.
    let mut forced_rechecks = 0;
    let config = DownloadConfig {
        verify: VerifyPolicy::Always,
        ..Default::default()
    };
    session::download(&meta, dir.path(), vec![], &config, |update| {
        if matches!(update, Progress::Verifying { .. }) {
            forced_rechecks += 1;
        }
    })
    .await
    .unwrap();
    assert_eq!(forced_rechecks, meta.piece_count());
}

/// A listener that accepts a connection, records it, and hangs up.
///
/// Stands in for the bulk of a public swarm: an address a tracker will happily
/// name and that gives you nothing. What matters here is only that connecting
/// to it fails after it has been counted.
async fn spawn_dead_peer(dialled: Arc<std::sync::Mutex<Vec<SocketAddr>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            dialled.lock().unwrap().push(addr);
            drop(socket);
        }
    });
    addr
}

#[tokio::test]
async fn every_discovered_peer_is_tried_not_just_the_first_batch() {
    // The regression this exists for: the session used to take the first
    // `max_peers` addresses, spawn one task each, and never look at the rest.
    // On a real swarm most addresses are unreachable, so the live seeders sat
    // in the unread tail of the list while thirty dead slots stayed occupied
    // for the whole download.
    let files = fixture_files();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.clone()))
        .collect();
    let meta = build_torrent(&borrowed);

    let dialled = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut peers = Vec::new();
    for _ in 0..12 {
        peers.push(spawn_dead_peer(Arc::clone(&dialled)).await);
    }

    let dir = tempfile::tempdir().unwrap();
    let config = DownloadConfig {
        // Four times fewer slots than addresses, so passing this test is only
        // possible by refilling them.
        max_peers: 3,
        // Off, so that this counts dials rather than counting dials times the
        // number of handshakes we are willing to try. The fallback has its own
        // test below.
        use_encryption: false,
        ..Default::default()
    };
    // No webseed and no real peer, so it cannot finish. The failure is the
    // expected outcome; what is under test is how many addresses were tried
    // before it gave up.
    let result = session::download(&meta, dir.path(), peers.clone(), &config, |_| {}).await;
    assert!(result.is_err(), "nothing could supply a piece");

    let dialled = dialled.lock().unwrap().clone();
    for addr in &peers {
        assert!(dialled.contains(addr), "{addr} was never tried");
    }
    // Each address is worth one attempt when connecting to it fails, so a
    // number well above twelve would mean the supervisor is retrying the dead.
    assert_eq!(dialled.len(), peers.len(), "dialled: {dialled:?}");
}

#[tokio::test]
async fn a_peer_that_refuses_plaintext_is_asked_again_with_an_obfuscated_handshake() {
    // The behaviour encryption buys, and its cost, in one test. A peer that
    // accepts a socket and hangs up is indistinguishable from one configured to
    // require encryption, so both are tried twice; that second dial is what
    // reaches the peers that are otherwise invisible.
    let files = fixture_files();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.clone()))
        .collect();
    let meta = build_torrent(&borrowed);

    let dialled = Arc::new(std::sync::Mutex::new(Vec::new()));
    let peer = spawn_dead_peer(Arc::clone(&dialled)).await;

    let dir = tempfile::tempdir().unwrap();
    let config = DownloadConfig {
        max_peers: 1,
        use_encryption: true,
        ..Default::default()
    };
    let result = session::download(&meta, dir.path(), vec![peer], &config, |_| {}).await;
    assert!(result.is_err(), "nothing could supply a piece");

    let dialled = dialled.lock().unwrap().clone();
    assert_eq!(
        dialled.len(),
        2,
        "once in plaintext and once obfuscated: {dialled:?}"
    );
}

/// A seeder that dials *us* and then serves the whole torrent.
///
/// Deliberately handshakes without the BEP 10 reserved bit set, so this also
/// covers the plain BEP 3 path: a peer with no extensions at all still has to
/// work, and the accept path has its own handshake ordering to get wrong.
async fn spawn_dialling_seeder(meta: Metainfo, flat: Vec<u8>, port: u16) {
    use balerion_bt::wire::{HANDSHAKE_LEN, Handshake, Message, MessageCodec};
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::Framed;

    tokio::spawn(async move {
        // The listener drops connections for torrents nobody is running, so
        // keep trying until the session under test has claimed this infohash.
        let stream = loop {
            let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await else {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                continue;
            };
            let ours = Handshake {
                reserved: [0u8; 8],
                info_hash: meta.info_hash,
                peer_id: *b"-SEED01-000000000000",
            };
            if stream.write_all(&ours.encode()).await.is_err() {
                continue;
            }
            let mut buf = [0u8; HANDSHAKE_LEN];
            match stream.read_exact(&mut buf).await {
                Ok(_) => break stream,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    continue;
                }
            }
        };

        let mut framed = Framed::new(stream, MessageCodec);

        // We have everything, and we are not going to make them ask twice.
        let mut bits = vec![0u8; meta.piece_count().div_ceil(8)];
        for index in 0..meta.piece_count() {
            bits[index / 8] |= 0x80 >> (index % 8);
        }
        framed.send(Message::Bitfield(bits.into())).await.unwrap();
        framed.send(Message::Unchoke).await.unwrap();

        while let Some(Ok(message)) = framed.next().await {
            if let Message::Request {
                index,
                begin,
                length,
            } = message
            {
                let start = index as usize * meta.piece_length as usize + begin as usize;
                let end = (start + length as usize).min(flat.len());
                framed
                    .send(Message::Piece {
                        index,
                        begin,
                        block: bytes::Bytes::copy_from_slice(&flat[start..end]),
                    })
                    .await
                    .unwrap();
            }
        }
    });
}

#[tokio::test]
async fn a_peer_that_dials_us_can_serve_the_whole_torrent() {
    // Until the listener existed, balerion announced a port nothing was on, so
    // a peer behind a NAT could reach us and could not be reached, and was
    // therefore invisible in both directions at once.
    let files = fixture_files();
    let borrowed: Vec<(&str, Vec<u8>)> = files
        .iter()
        .map(|(name, data)| (name.as_str(), data.clone()))
        .collect();
    let meta = build_torrent(&borrowed);
    let flat: Vec<u8> = files.iter().flat_map(|(_, data)| data.clone()).collect();

    let inbound = balerion_bt::Inbound::bind(0).await.unwrap();
    spawn_dialling_seeder(meta.clone(), flat.clone(), inbound.port()).await;

    let dir = tempfile::tempdir().unwrap();
    let config = DownloadConfig {
        port: inbound.port(),
        inbound: Some(inbound),
        // Somebody has to be given time to find us. Without a grace the
        // session is entitled to give up before the first connection lands,
        // which is the documented behaviour rather than a bug.
        peer_refill_grace: std::time::Duration::from_secs(10),
        ..Default::default()
    };

    // No addresses to dial and no webseeds: every byte here arrived on a
    // connection we did not make.
    let summary = session::download(&meta, dir.path(), vec![], &config, |_| {})
        .await
        .expect("the dialling seeder supplied everything");

    assert_eq!(summary.pieces, meta.piece_count());
    assert_eq!(summary.from_webseeds, 0, "there was no webseed");
    assert_eq!(summary.from_peers, meta.piece_count());

    for (name, data) in &files {
        let path = dir.path().join("an-item").join(name);
        let written =
            std::fs::read(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        assert_eq!(&written, data, "{name} came out wrong");
    }
}
