//! Tests that talk to the real archive.org. Ignored by default so `cargo test`
//! stays offline and fast:
//!
//! ```sh
//! cargo test -p dipper-ia -- --ignored --test-threads=1
//! ```
//!
//! These are the tests that catch archive.org changing under us, which it does.

use dipper_ia::{AdvancedQuery, ClientConfig, IaClient, advanced, metadata, torrent};

fn client() -> IaClient {
    IaClient::with_config(ClientConfig {
        // Be a good citizen when running against the live site.
        reduced_priority: true,
        ..Default::default()
    })
    .expect("client builds")
}

/// The strongest check we have: archive.org publishes the infohash of its own
/// derived torrent as the `btih` of the `.torrent` file record. If our bencode
/// span finder or SHA-1 were wrong, these would differ.
#[tokio::test]
#[ignore = "hits archive.org"]
async fn computed_infohash_matches_the_one_archive_org_publishes() {
    let client = client();
    let item = metadata::fetch(&client, "nasa").await.expect("metadata");
    let declared = item
        .torrent_file()
        .expect("nasa has a derived torrent")
        .btih
        .clone()
        .expect("archive.org publishes a btih");

    let meta = torrent::fetch(&client, &item).await.expect("torrent");
    assert_eq!(meta.info_hash_hex(), declared.to_lowercase());
    assert!(meta.magnet().contains(&declared.to_lowercase()));
}

/// archive.org torrents should carry both trackers and at least one webseed,
/// since webseeds are where the bytes actually come from.
#[tokio::test]
#[ignore = "hits archive.org"]
async fn derived_torrents_carry_trackers_and_webseeds() {
    let client = client();
    let item = metadata::fetch(&client, "nasa").await.expect("metadata");
    let meta = torrent::fetch(&client, &item).await.expect("torrent");

    assert!(
        torrent::IA_TRACKERS
            .iter()
            .all(|t| meta.announce.contains(&t.to_string())),
        "expected both archive.org trackers, got {:?}",
        meta.announce
    );
    assert!(!meta.webseeds.is_empty(), "expected webseeds");
    assert!(
        meta.webseeds.iter().all(|w| w.starts_with("https://")),
        "webseeds should be upgraded to https: {:?}",
        meta.webseeds
    );
    assert_eq!(meta.total_length, meta.files.iter().map(|f| f.length).sum::<u64>());
}

#[tokio::test]
#[ignore = "hits archive.org"]
async fn missing_items_are_reported_as_not_found() {
    let client = client();
    let err = metadata::fetch(&client, "definitely-not-a-real-item-xyzzy-1234")
        .await
        .expect_err("should not exist");
    assert!(matches!(err, dipper_ia::Error::ItemNotFound(_)), "{err:?}");
}

/// Page-based paging must actually advance. This is the failure that pushed us
/// off the scrape API: identical pages forever.
#[tokio::test]
#[ignore = "hits archive.org"]
async fn paging_returns_distinct_items() {
    let client = client();
    let query = AdvancedQuery::new("mediatype:audio AND birdsong")
        .fields(["identifier"])
        .rows(100);

    let first = advanced::page(&client, &query, 1).await.expect("page 1");
    let second = advanced::page(&client, &query, 2).await.expect("page 2");
    assert!(first.num_found > 200, "need a big enough result set to page");

    let ids: std::collections::HashSet<_> = first.hits.iter().map(|h| &h.identifier).collect();
    let overlap = second
        .hits
        .iter()
        .filter(|h| ids.contains(&h.identifier))
        .count();
    assert_eq!(overlap, 0, "pages 1 and 2 overlapped by {overlap} items");
}

/// The `format:"Archive BitTorrent"` filter must actually restrict results,
/// and the `format` field must come back so we can flag torrent-backed items.
#[tokio::test]
#[ignore = "hits archive.org"]
async fn torrent_filter_narrows_and_is_visible_in_results() {
    let client = client();
    let plain = AdvancedQuery::new("mediatype:audio AND birdsong");
    let filtered =
        AdvancedQuery::new(r#"mediatype:audio AND birdsong AND format:"Archive BitTorrent""#);

    let all = advanced::total(&client, &plain).await.expect("total");
    let with_torrents = advanced::total(&client, &filtered).await.expect("total");
    assert!(with_torrents > 0);
    assert!(
        with_torrents <= all,
        "filtered ({with_torrents}) should not exceed unfiltered ({all})"
    );

    let hits = advanced::collect(&client, &filtered, 20, |_, _| {})
        .await
        .expect("results");
    assert!(
        hits.iter().all(advanced::has_torrent),
        "every filtered hit should report the torrent format"
    );
}
