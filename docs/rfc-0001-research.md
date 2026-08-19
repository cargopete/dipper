# RFC-0001: Research dossier

The founding research for balerin, preserved as written. Anything in it that we
have since checked against the live site is corrected in
[archive-org-notes.md](archive-org-notes.md); in particular **the scrape API's
cursor pagination does not work anonymously**, so the "cursor-based, no depth
limit" plan below was replaced by page-based `advancedsearch.php` paging with a
stable sort and a 10,000 result ceiling.

---

## TL;DR

- **The architecture is sound and buildable on today's Rust ecosystem.** IA exposes clean JSON metadata/search APIs, publishes per-item `.torrent` files with predictable webseed/tracker structure, and the mainline DHT + BEP 9/10 path lets you resolve any item to a swarm from an infohash alone. The pragmatic stack is `librqbit`-as-reference (or as a library), the pubky `mainline` crate for DHT (exposes `get_peers`/`announce_peer`), `serde_bencode`/`bendy` for bencode, `tantivy` for the local index, `reqwest` for HTTP, and `sha1`/`sha2` for hashing.
- **For a 2026 client, implement BEP 3/5/9/10/23/15/19 as the core; treat BEP 52 (v2) as read-support-nice-to-have, and BEP 29 (uTP) as optional.** IA torrents in particular lean heavily on webseeds (BEP 19), so a robust HTTP/GetRight webseed implementation is arguably more valuable than a perfect choke algorithm for this use case.
- **Do NOT try to reuse `libp2p-kad` for the BitTorrent DHT** — it is a different, wire-incompatible Kademlia variant (SHA-256 keyspace, Protobuf-over-stream messages, PeerId-based records). Use a mainline-specific crate.

## Key findings

1. **IA metadata API** (`https://archive.org/metadata/{identifier}`) returns a single JSON document with top-level `files[]`, `dir`, `server`, `d1`/`d2` (data nodes), `workable_servers`, `item_size`, and a nested `metadata{}` object (mediatype, collection, title). No auth required for reads.
2. **IA torrents** are auto-derived, named `{identifier}_archive.torrent`, use trackers `http://bt1.archive.org:6969/announce` and `http://bt2.archive.org:6969/announce`, and carry HTTP webseeds (BEP 19 `url-list`) pointing at `ia*.us.archive.org` data nodes and `https://archive.org/download/`. IA does not persistently seed; most data comes via webseed.
3. **The `mainline` crate** is the right DHT dependency: it implements BEP 5/42/43/44 and exposes `get_peers(info_hash) -> ... Vec<SocketAddrV4>` and `announce_peer(info_hash, port)`. Sync methods are deprecated in favor of `as_async()`/`AsyncDht`.
4. **`librqbit`** is the strongest Rust reference/library — production-grade, modular (`librqbit-dht`, `peer_binary_protocol`, `bencode`, `upnp`, `librqbit-utp`), and can be used directly as a crate.
5. **BEP 52 (v2) has low real-world adoption** — support hybrid-torrent *reading* if cheap, but v1 SHA-1 remains mandatory and sufficient.

## 1. Internet Archive APIs

**Metadata read API.** `GET https://archive.org/metadata/{identifier}` returns JSON:

```json
{
  "created": 1616004182,
  "d1": "ia600308.us.archive.org",
  "d2": "ia800308.us.archive.org",
  "dir": "/21/items/xfetch",
  "files": [
    {"name":"xfetch.pdf","source":"original","format":"Text PDF","mtime":"1479169618",
     "size":"419170","md5":"...","crc32":"...","sha1":"..."}
  ],
  "files_count": 13,
  "item_last_updated": 1613804036,
  "item_size": 10682344,
  "metadata": {"mediatype":"texts","collection":["opensource","community"],"title":"..."},
  "server": "ia800308.us.archive.org",
  "workable_servers": ["ia800308.us.archive.org","ia600308.us.archive.org"]
}
```

- **Per-file records** carry `name`, `size` (string), `md5`, `crc32`, `sha1`, `format`, `source` (`original`/`derivative`/`metadata`). The `.torrent` appears as a file with `"format":"Archive BitTorrent"` and name `{identifier}_archive.torrent`.
- **Partial reads**: address sub-paths, e.g. `/metadata/{id}/server`, `/metadata/{id}/files/0`, and array slicing via `?start=100&count=5`. Returns `{"result": ...}`.
- **Errors**: nonexistent item returns an *empty array*, not an error object, unless you pass `extended_err=1`.
- **Compression**: send `Accept-Encoding: deflate, gzip`.
- The canonical backing files are `{identifier}_meta.xml` and `{identifier}_files.xml`; the JSON API is the derived, preferred interface.

**Search / scrape APIs.**

- **Advanced search**: `https://archive.org/advancedsearch.php?q=...&output=json&rows=N&page=P&fl[]=identifier&fl[]=title&sort[]=...`. Lucene-like query syntax. **Hard limit: paged sorted results only to the 10,000th result.**
- **Scrape API**: `https://archive.org/services/search/v1/scrape` — cursor-based. Params: `q`, `fields`, `sorts` (if `identifier` is included it must be last), `count` (min 100), `cursor`, `total_only`.
- **Filtering**: `mediatype:(movies|audio|texts|software|image|data|web)`, `collection:(...)`, and `format:"Archive BitTorrent"` to select torrent-backed items.
- **Rate limits**: IA rate-limits aggressively; third-party scrapers report 429s after a dozen or so requests at concurrency 5. Community best practice is ~300-500 ms between requests. Honour `X-Accept-Reduced-Priority` and be ready for HTTP 429.

**Authentication & terms.** Reads are anonymous. **IA-S3 keys** (access + secret) are needed for uploads/metadata writes and for searching at scale; obtain at `https://archive.org/account/s3.php`. Sent as `Authorization: LOW <access>:<secret>`. For a download-only client, no keys are required, but sending them and a descriptive `User-Agent` is polite.

**IA torrent mechanics & quirks.**

- IA auto-derives a `.torrent` for most public items. IA's help page states "over 1.4 million Archive Items are available via the BitTorrent protocol, comprising almost a petabyte of public domain materials."
- Trackers: `bt1`/`bt2.archive.org:6969`. **IA's trackers reject seeding by third parties** — you can leech, but the swarm is effectively IA's servers via webseed.
- **Webseeds (BEP 19, GetRight-style)** are the backbone. Often *all* bytes come from webseeds. Note the historical HTTP/HTTPS mixed-content quirk: IA writes `http://` webseed URLs; prefer upgrading where the host supports it.
- Magnets IA generates look like `magnet:?xt=urn:btih:<sha1>&dn=<identifier>&tr=...&ws=...`.
- **Torrent staleness**: IA `.torrent` files can go out of date relative to item contents, causing downloads to stall at ~99% on piece hash mismatches. Workaround: posting a review or otherwise modifying the item triggers re-derivation. A production client should detect hash-mismatch stalls and fall back to direct HTTP download of individual files.

## 2. BitTorrent BEPs

**BEP 3 (core).**

- **Bencoding**: strings `<len>:<bytes>`; ints `i<n>e`; lists `l...e`; dicts `d<key><val>...e` with keys as sorted byte strings.
- **Metainfo**: top-level `announce`, optional `announce-list`, `info` dict. `info` = `name`, `piece length`, `pieces` (concatenated 20-byte SHA-1 piece hashes), and either `length` (single-file) or `files[]` (each `{length, path[]}`).
- **Infohash = SHA-1 of the bencoded `info` dictionary**, the substring exactly as it appears on the wire.
- **Tracker HTTP**: GET with `info_hash`, `peer_id`, `port`, `uploaded`, `downloaded`, `left`, `event`. Bencoded response: `interval`, `peers` (dict form or compact per BEP 23), `failure reason` on error.
- **Peer wire**: handshake = `\x13` + `"BitTorrent protocol"` + 8 reserved bytes + 20-byte infohash + 20-byte peer_id, then length-prefixed messages: `keep-alive`, `choke`(0), `unchoke`(1), `interested`(2), `not interested`(3), `have`(4), `bitfield`(5), `request`(6), `piece`(7), `cancel`(8). Blocks are typically 16 KiB.

**BEP 5 (DHT / mainline Kademlia).**

- UDP, KRPC (bencoded) messages, conventionally on port 6881. 160-bit node IDs, XOR distance, k-buckets (k=8 typical).
- Four queries: `ping`, `find_node`, `get_peers` (returns `values` or `nodes`, plus a `token`), `announce_peer` (must echo a recent `token`, which binds to querier IP).
- Bootstrap nodes: `router.utorrent.com:6881`, `router.bittorrent.com:6881`, `dht.transmissionbt.com:6881`.

**BEP 9 (ut_metadata) + BEP 10 (extension protocol).**

- BEP 10 negotiates named extensions via an `m` dict in an extended handshake (reserved bit `0x100000`).
- BEP 9 `ut_metadata` fetches the `info` dict from peers in 16 KiB blocks. Messages: `request`, `data` (with `total_size`), `reject`. Fetched metadata is validated against the infohash. This is what makes magnet-only downloads work: DHT → peers → BEP 10 handshake → BEP 9 fetch → normal BEP 3 download.

**Other BEPs.**

- **BEP 11 (PEX)**: peer exchange as a BEP 10 extension (`ut_pex`); cheap, high value once connected.
- **BEP 15 (UDP tracker)**: connect (64-bit connection ID) then announce. Much lower overhead than HTTP trackers, though IA uses HTTP.
- **BEP 23 (compact peers)**: 6-byte peer encoding. Mandatory in practice.
- **BEP 19 (webseed / GetRight)**: `url-list` in metainfo; HTTP range requests map pieces to file offsets. **Essential for IA.**
- **BEP 29 (uTP)**: LEDBAT over UDP. Optional; `librqbit-utp` exists.
- **BEP 52 (v2)**: SHA-256, merkle `file tree`, hybrid v1+v2. Low adoption. v1 mandatory; hybrid *read* support only if cheap.
- **Magnet URI**: `xt=urn:btih:<sha1>` (or `urn:btmh:` for v2), `dn`, `tr` (repeatable), `ws` (repeatable), `x.pe`. `xt` is the only mandatory parameter.

## 3. Rust crate ecosystem

**Bencode.**

- `bendy` — enforces canonical encoding, `no_std` capable, nesting-depth limit against decompression bombs. Strongest correctness posture.
- `serde_bencode` — ergonomic serde integration, widely used; good default for metainfo structs.
- `serde_bencoded`, `bende`, `bt_bencode`, `torrust-serde-bencode` — viable alternatives.
- **Recommendation**: `serde_bencode` for typed metainfo; a hand-rolled zero-copy scanner for the untrusted/hot path and exact infohash recomputation.

**DHT.**

- **`mainline`** (pubky) — implements BEP 5/42/43/44, exposes `get_peers` and `announce_peer`, plus BEP 44 put/get. Sync methods deprecated in favour of `as_async()`/`AsyncDht`. Includes vertical Sybil mitigation and adaptive client/server mode. Pin the major version explicitly: crates.io and docs.rs disagreed at time of writing.
- **`librqbit-dht`** — rqbit's own BEP 5 DHT, battle-tested inside rqbit, streaming peer-discovery API.
- **libp2p-kad is NOT usable.** Different keyspace (SHA-256 vs 160-bit infohash), different encoding (Protobuf over libp2p streams vs bencoded KRPC over UDP), different records (PeerId provider records vs raw IP:port peer lists with tokens). Inspired-by, not compatible-with.

**Full implementations / references.**

- **`librqbit` / `rqbit`** (ikatson) — the flagship. Modular workspace: `librqbit-core`, `librqbit-dht`, `peer_binary_protocol`, `bencode`, `buffers`, `sha1w`, `upnp`, `librqbit-utp`, `dualstack-sockets`. Sequential/streaming download, DHT, magnet, UPnP, HTTP API, web UI. Best both as study reference and as a usable library.
- **`cratetorrent`** (vimpunk) — excellent pedagogical engine with an extensively documented DESIGN.md. v1 only, Linux-only (`pwritev`/`preadv`), tokio async.
- **`transmission_rs`**, **libtorrent-rasterbar bindings** — reintroduce C/C++ dependencies; against the single-binary Rust ethos.
- **Newer entrants**: `lambdaclass/libtorrent-rs` (`dtorrent`) and assorted `bittorrent-*` crates; instructive but less mature.

**Peer wire / async.** `tokio`; one task per peer with a framed codec (`tokio_util::codec`); an actor-style torrent coordinator over `tokio::sync::mpsc` (commands) and `watch`/`broadcast` (state); backpressure via bounded channels. rqbit's `peer_binary_protocol` is a clean reference.

**Full-text search.** `tantivy` — Lucene-inspired, BM25, sub-10 ms startup (good for a CLI), multithreaded indexing, mmap with madvise, faceting and range queries. Ideal for harvested IA metadata.

**HTTP.** `reqwest` for both metadata/search calls and BEP 19 webseed range requests. Enable streaming for large range GETs; set a descriptive `User-Agent`; support gzip/deflate.

**Hashing.** `sha1` (RustCrypto) for v1, `sha2` for v2/hybrid. Piece verification is CPU-bound at high throughput: use hardware-accelerated backends and verify on `spawn_blocking`/rayon rather than the async reactor.

## 4. Implementation design concerns

**Piece selection.** *Rarest-first* by default; *random-first-piece* for the very first piece; *sequential* for streaming at the cost of swarm health; *endgame mode* near completion (request remaining blocks from every peer that has them, `cancel` as each arrives). For IA, where webseeds dominate and the swarm is essentially HTTP, sequential plus aggressive webseed range parallelism is often optimal.

**Choke/unchoke.** Tit-for-tat: every 10 s order interested peers by download rate they give you and unchoke the top ~4; every 30 s optimistically unchoke one random peer. In seed state, order by upload rate. References: Legout et al., "Rarest First and Choke Algorithms Are Enough" (IMC 2006), and Cohen's BitTorrent Economics Paper.

**Verification, partial files, disk I/O.** Verify each completed piece against SHA-1 before marking it `have`; on mismatch re-download and consider banning the contributor. Support selective file download (map piece↔file offsets). Pre-allocate and use sparse files for out-of-order writes; mmap simplifies random writes but complicates error handling and stresses the page cache on huge items, while buffered `pwritev`/`preadv` is more predictable. Batch small block writes into piece-sized vectored writes. Make fsync policy tunable.

**Concurrency architecture.** Task-per-peer with an actor-model torrent manager. A central `Torrent`/`Session` actor owns the piece picker and bitfield; bounded channels give backpressure. Keep hashing and disk I/O off the reactor. DHT (its own UDP task) and webseed (HTTP client pool) are independent subsystems feeding the peer/piece layer.

**NAT traversal.** Without a public port you can still leech fine; you just will not accept inbound peers. Attempt UPnP-IGD and NAT-PMP/PCP opportunistically, degrade gracefully to outbound-only. For IA specifically, webseeds are pure outbound HTTP, so NAT is a non-issue. Hole punching (BEP 55) is complex and low ROI here.

**Resume / state persistence.** Persist the metainfo (or raw `.torrent`), the piece bitfield, partial-piece block state, file selection, DHT routing-table cache and tracker state. On resume, re-hash existing data or trust a fsync-checkpointed bitfield with a fast-resume flag. A small bencoded or JSON sidecar next to the download does the job.

## 5. Reference implementations & learning resources

- **rqbit / librqbit** (Rust) — most complete modern Rust reference.
- **cratetorrent** (Rust) — best-documented design for learning engine internals.
- **Transmission** (C) and **libtorrent-rasterbar** (C++) — canonical production references.
- **webtorrent/bittorrent-protocol**, **ut_metadata**, **bittorrent-dht** (JS) — clean, readable per-BEP modules.
- **Specs**: bittorrent.org/beps (BEP 3/5/9/10/15/19/23/52), the BitTorrent Economics Paper, Legout et al. IMC 2006, "Understanding BitTorrent: An Experimental Perspective" (EURECOM).
- **magnetico** wiki — DHT crawling and `get_peers`/`announce_peer` internals.

## Recommendations

1. **Phase 0 — metadata/search spike.** Build the IA layer first: metadata client (gzip, partial reads, 429 backoff at ~300-500 ms spacing), paginated search, and a `format:"Archive BitTorrent"` filter. Harvest into tantivy. Deliverable: `search → identifier → .torrent URL / magnet`. De-risks the whole IA dependency cheaply.
2. **Phase 1 — .torrent + HTTP/webseed download.** Parse metainfo, compute the infohash from raw bytes, and implement **BEP 19 webseeds first** (range GETs against `ia*.us.archive.org`), plus direct per-file HTTP fallback via `dir`/`server`. Because IA serves nearly all bytes over HTTP, a webseed-only client already downloads most IA content. Add piece verification and resume here.
3. **Phase 2 — peer wire + trackers.** BEP 3 peer protocol (framed tokio codec), HTTP (BEP 3/23) and UDP (BEP 15) trackers, task-per-peer plus actor manager, rarest-first, endgame, tit-for-tat choking.
4. **Phase 3 — DHT + magnet.** Integrate `mainline` or `librqbit-dht`; add BEP 10 + BEP 9 to resolve magnets to info dicts; add BEP 11 PEX.
5. **Phase 4 — polish.** Optional uTP, UPnP/NAT-PMP, hybrid v2 read support, richer index (facets by mediatype/collection/creator).

**Decision thresholds.** If SHA-1 verification bottlenecks multi-Gbps throughput, move hashing to a rayon pool. If 429s appear, widen spacing, add jittered backoff, consider IA-S3 keys and reduced-priority headers. If a download stalls near 100% with hash mismatches, fall back to direct file HTTP and surface the re-derive workaround. If real v2/hybrid IA torrents turn up in the wild, prioritise BEP 52 read support; otherwise defer.

## Caveats

- **IA torrent staleness is a real, recurring operational problem.** Do not assume the `.torrent` matches current item bytes.
- **IA trackers reject third-party seeding**, so you cannot contribute upload back to IA swarms. This shapes the whole design toward HTTP.
- **Pin dependency versions explicitly** and verify API signatures at build time.
- **BEP 52 adoption is low**; do not over-invest. v1 SHA-1 infohashes remain the interoperability baseline.
- Some quoted figures are secondary sources, not independently verified.
- Confirm IA's current Terms of Use and bot policy at build time, and set a descriptive `User-Agent`.

*Scope note: this dossier deliberately covers only the Internet Archive as a legitimate, largely public-domain content source and the BitTorrent protocol engineering needed to build the client.*
