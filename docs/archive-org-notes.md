# archive.org: verified behaviour

Everything here was checked against the live site on 2026-08-04. Where the
documentation and the server disagree, this file records the server.

## Metadata API

`GET https://archive.org/metadata/{identifier}` needs no auth and returns one
JSON document.

- **A missing item answers `[]`**, an empty array, not an error object. Pass
  `extended_err=1` for a machine-readable `errcode`. `ItemMetadata::parse` maps
  the empty array to `Error::ItemNotFound`.
- **Numbers are sometimes strings.** `"size": "419170"` and `"size": 4523` both
  occur, in the same response. Same for `mtime`, `item_size`, `downloads`.
- **Metadata values are scalar-or-array** depending on cardinality: `creator`
  may be a string or a list of strings. `Meta::get_str` and `Meta::get_all`
  flatten both shapes.
- The derived torrent appears as a file with `"format": "Archive BitTorrent"`,
  named `{identifier}_archive.torrent`.
- **The torrent file record carries a `btih` field**, which is archive.org's
  own infohash for that torrent. Free cross-check for any client that computes
  its own; `live_archive.rs` asserts they match.
- `server`, `d1`, `d2` and `workable_servers` name data nodes; `dir` is the
  item path on them. Direct file URL is `https://{server}{dir}/{name}`, and
  `https://archive.org/download/{id}/{name}` redirects to a node.

## Search

Two APIs, only one of which pages.

### scrape API (`/services/search/v1/scrape`) — cursor pagination is broken

Documented as the deep-pagination route. Anonymously, as of 2026-08-04:

- The first page works and returns a `cursor`.
- **Sending that cursor back returns page one again**, with a cursor identical
  to the one you sent. Verified with byte-exact percent-encoding: 100 items,
  100/100 overlap with page one, cursor unchanged.
- If you set `sorts`, the same round trip fails outright:
  `400 {"error":"Bad cursor: ...","errorType":"InvalidArgumentException"}`,
  for a cursor the server itself issued a second earlier.
- POSTing the query, or sending the cursor alone, both give `400`.

`scrape_all` therefore stops when it sees an unchanged cursor rather than
looping forever. `total_only=true` still works and is a cheap match count.

### advancedsearch.php — what balerin actually uses

`GET /advancedsearch.php?q=...&output=json&rows=N&page=P&fl[]=...&sort[]=...`

- Paging works, up to **10,000 results**, after which you get
  `{"error": "[DEEP_PAGING] ..."}` **with HTTP 200**, so check the body, not
  the status.
- `rows=1000` is accepted. balerin defaults to 500.
- **Sort explicitly or pages overlap.** Under the default relevance ordering,
  pages 1 and 2 of the same query shared an item. `sort[]=identifier asc` gives
  clean, disjoint pages.
- `fl[]` is repeated once per field. `format` is a valid field and comes back
  as an array, which is how balerin flags torrent-backed items.
- `format:"Archive BitTorrent"` genuinely filters: 1489 of 1547 for one audio
  query, and a nonsense format returns 1.

### Rate limits and flakiness

- ~300-500 ms between requests keeps 429s away. balerin defaults to 350 ms and
  retries 429/5xx with jittered exponential backoff, honouring `Retry-After`.
- **Under load the search API silently returns a truncated result set** rather
  than an error. One query that matches 1547 items returned 3 items and
  `"total": 3`, with HTTP 200, then returned 1547 again on retry a minute
  later. There is no way to distinguish this from a genuinely small result set
  in a single response, so treat suspiciously small totals with suspicion.

## Torrents

- Trackers are `http://bt1.archive.org:6969/announce` and `bt2`, and they
  reject seeding by third parties. You can leech; you cannot give back.
- **Webseeds (BEP 19) are where the bytes come from.** `url-list` points at
  `ia*.us.archive.org` data nodes plus `https://archive.org/download/`.
  archive.org writes `http://` URLs for hosts that speak HTTPS, so balerin
  upgrades the scheme.
- Derived torrents can go stale relative to item contents, stalling downloads
  near 100% on piece hash mismatches. The documented workaround is to trigger
  re-derivation by modifying the item, for example by posting a review. A
  client should detect the stall and fall back to direct HTTP file download,
  which the metadata API fully supports.
- The infohash is the SHA-1 of the `info` dictionary **as it appears on the
  wire**. balerin locates the byte span with a small bencode scanner rather than
  re-encoding a parsed structure, because re-encoding would drop unknown keys
  and change the hash.
