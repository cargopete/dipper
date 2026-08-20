//! The whole client against a server, offline.
//!
//! The unit tests cover the URL building and the parsing separately, which
//! leaves the join between them untested: whether the request we actually send
//! is the one we think we send, and whether what comes back over a socket
//! parses the same as a byte slice does. That join is where a client is usually
//! wrong.
//!
//! So this is a real HTTP server answering a real request, with no network
//! beyond loopback and no indexer to be up.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use balerion_torznab::{Indexer, Query, TorznabClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const HASH: &str = "30f15834bd5cb994bec71635455691acd64875e4";

fn feed() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <title>Fixture</title>
    <item>
      <title>Tom &amp; Jerry S01E02 1080p</title>
      <size>1073741824</size>
      <torznab:attr name="seeders" value="17"/>
      <torznab:attr name="peers" value="20"/>
      <torznab:attr name="infohash" value="{HASH}"/>
    </item>
    <item>
      <title>Nothing We Can Open</title>
      <link>https://example.invalid/download/x.torrent</link>
    </item>
  </channel>
</rss>"#
    )
}

/// The smallest thing that can be a Torznab indexer: answers one GET, and
/// records the path it was asked for.
async fn spawn_indexer(
    status: &'static str,
    body: String,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));

    let recorded = Arc::clone(&asked);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let recorded = Arc::clone(&recorded);
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(read) = stream.read(&mut buf).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                if let Some(path) = request.split_whitespace().nth(1) {
                    recorded.lock().unwrap().push(path.to_string());
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/rss+xml\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (addr, asked)
}

#[tokio::test]
async fn a_search_reaches_the_indexer_and_comes_back_usable() {
    let (addr, asked) = spawn_indexer("200 OK", feed()).await;
    let indexer = Indexer::parse(&format!("fixture=http://{addr}")).unwrap();
    let client = TorznabClient::new(vec![indexer.clone()], "secret-key").unwrap();

    let answer = client
        .ask(
            &indexer,
            &Query::new("tom and jerry").episode(1, 2).limit(25),
        )
        .await
        .expect("the fixture answers");

    // What we sent.
    let path = asked.lock().unwrap().first().cloned().expect("one request");
    assert!(path.starts_with("/api?t=search"), "{path}");
    assert!(path.contains("apikey=secret-key"), "{path}");
    assert!(path.contains("q=tom%20and%20jerry"), "{path}");
    assert!(path.contains("season=1"), "{path}");
    assert!(path.contains("ep=2"), "{path}");
    assert!(path.contains("limit=25"), "{path}");

    // What came back.
    assert_eq!(answer.hits.len(), 1, "{answer:?}");
    assert_eq!(
        answer.without_magnet, 1,
        "the .torrent-only item is counted, not hidden"
    );

    let hit = &answer.hits[0];
    assert_eq!(hit.title, "Tom & Jerry S01E02 1080p");
    assert_eq!(hit.seeders, 17);
    assert_eq!(hit.leechers, 3);
    assert_eq!(hit.size_bytes, 1_073_741_824);
    assert_eq!(hit.info_hash, HASH);
    assert_eq!(hit.indexer, "fixture");
    assert!(
        hit.magnet
            .starts_with(&format!("magnet:?xt=urn:btih:{HASH}"))
    );
}

#[tokio::test]
async fn a_refused_key_is_reported_as_such() {
    let (addr, _) = spawn_indexer("401 Unauthorized", "nope".into()).await;
    let indexer = Indexer::parse(&format!("http://{addr}")).unwrap();
    let client = TorznabClient::new(vec![indexer.clone()], "wrong").unwrap();

    let err = client.ask(&indexer, &Query::new("x")).await.unwrap_err();
    assert!(
        matches!(err, balerion_torznab::Error::BadKey),
        "expected a key problem, got {err:?}"
    );
}

#[tokio::test]
async fn one_indexer_being_down_does_not_take_the_search_with_it() {
    // The point of asking several: two good answers beat one error.
    let (good, _) = spawn_indexer("200 OK", feed()).await;
    let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();

    let client = TorznabClient::new(
        vec![
            Indexer::parse(&format!("good=http://{good}")).unwrap(),
            Indexer::parse(&format!("dead=http://{dead}")).unwrap(),
        ],
        "key",
    )
    .unwrap();

    let answer = client.search(&Query::new("x")).await;
    assert_eq!(answer.hits.len(), 1, "the good one still answered");
    assert_eq!(answer.hits[0].indexer, "good");
}
