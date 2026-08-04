# dipper

A single-binary BitTorrent client that will also search the Internet Archive
for you. Named after the white-throated dipper, a small brown bird that walks
into rivers and comes back out with things.

The engine takes a magnet link, a `.torrent`, or an archive.org identifier and
produces verified bytes on disk. It is deliberately provenance-agnostic: below
the CLI it knows only a 20-byte infohash and has no idea where you got it.

## Install

```sh
cargo install --path crates/dipper-cli
```

## Search and fetch in one go

```sh
dipper get "dawn chorus birdsong"
```

```
1.   68.7 MiB  audio      Megaheadphoneboy - A Plane Flew Overhead…  (MegaHeadPhoneBoy)
2.  101.4 MiB  audio      Dawn Chorus  (Robert White)
3.  141.0 MiB  audio      2024.06.09 framework radio  (framework)
4.   13.5 MiB  audio      Birdsong in a Kent garden  (Ruth G)

which one? [1-4, enter for 1, q to quit]
```

`--list` shows the results and stops, `--pick <n>` and `--first` choose without
asking, and `-o <dir>` says where to put it. Only torrent-backed items are
offered, since there is nothing to fetch otherwise.

## Download things directly

```sh
# an arbitrary magnet link
dipper download "magnet:?xt=urn:btih:481b6e...071e&tr=http://bttracker.debian.org:6969/announce"

# a .torrent on disk
dipper download ./debian-13.6.0-amd64-netinst.iso.torrent -o ~/Downloads

# an archive.org item, by identifier
dipper download nasa -o ~/Downloads

# see what a magnet actually is, without downloading it
dipper resolve "magnet:?xt=urn:btih:481b6e...071e"
```

Interrupt a download and run it again: it picks up where it stopped. A resume
file next to the download records which pieces verified, so a restart does not
mean re-reading the lot. It is only trusted when it was written cleanly; after
a kill -9 dipper falls back to re-hashing, and `--verify` forces that anyway.

Engine flags: `--no-dht`, `--no-webseeds`, `--port`, `--max-peers`,
`--dht-seconds`, `--verify`, `--quiet`.

## Search archive.org

```sh
# harvest search results into a local index (fast, no network after this)
dipper harvest --torrents-only --mediatype audio "field recordings" -n 2000

# search the local catalogue
dipper search "dawn chorus" -n 10
dipper search "mediatype:audio AND has_torrent:true" --json

# or skip the catalogue and ask archive.org
dipper search --remote --torrents-only "apollo 11" -n 20

dipper info nasa --files
dipper magnet nasa
dipper torrent nasa --show
dipper index stats
```

Global flags: `--index-dir`, `--min-interval` (ms between archive.org
requests, default 350), `--polite`, `-v`. Set `IA_ACCESS_KEY` and
`IA_SECRET_KEY` if you have archive.org S3 credentials; reads do not need them.

## What is implemented

| BEP | What | Status |
| --- | --- | --- |
| 3 | Core protocol, peer wire, HTTP trackers | yes |
| 5 | Mainline DHT peer discovery | yes (via `mainline`) |
| 9 | `ut_metadata`: fetch the info dict from peers | yes |
| 10 | Extension protocol | yes |
| 15 | UDP trackers | yes |
| 19 | HTTP/GetRight webseeds | yes |
| 23 | Compact peer lists | yes |
| 11 | PEX | not yet |
| 29 | uTP | no |
| 52 | BitTorrent v2 | no (v2-only magnets are refused with an explanation) |

Piece selection is random-first then rarest-first, with an endgame at the tail.
Every piece is SHA-1 verified before it is written, whether it came from a peer
or a webseed. Uploading is not implemented: dipper leeches, and says so.

## The magnet bootstrap

A magnet is not a torrent. It carries an infohash and some hints, and nothing
about piece length, piece hashes or file layout. Those come out of the swarm:

```
magnet → infohash → trackers + DHT → peers → BEP 3 handshake
       → BEP 10 extended handshake → BEP 9 metadata blocks
       → SHA-1(info dict) == infohash   ← the only thing making this safe
       → ordinary download
```

That hash check is load-bearing. A peer can answer a metadata request with any
bytes it likes; without the comparison it would be choosing our file layout and
our piece hashes. `Metainfo::from_verified_info_dict` is the only way the
engine will build a torrent from peer-supplied bytes, and there is a test that
feeds it a lying peer.

## Layout

| crate | what it does |
| --- | --- |
| `dipper-bt` | the engine: magnet, bencode, metainfo, trackers, DHT, peer wire, picker, storage, webseeds |
| `dipper-ia` | archive.org client: metadata API, search |
| `dipper-index` | local tantivy catalogue over harvested metadata |
| `dipper-cli` | the `dipper` binary |

## Tests

```sh
cargo test --workspace                                  # offline, fast
cargo test -p dipper-ia -- --ignored --test-threads=1   # hits archive.org
```

The offline suite includes a fake peer that serves metadata over BEP 9, and a
second one that serves the *wrong* metadata to prove we reject it. The live
suite checks our computed infohash against the `btih` archive.org publishes for
its own derived torrents, which independently validates the bencode scanner and
the SHA-1 path.

## Things the internet does that you should know about

- [docs/archive-org-notes.md](docs/archive-org-notes.md) — the scrape API's
  cursor pagination does not work anonymously, search silently truncates under
  load, and derived `.torrent` files go stale.
- Webseeds answer `200` with the whole file when a range covers all of it,
  rather than `206`. Refusing anything but `206` means never downloading a
  small file. dipper slices the body itself.
- archive.org's trackers reject third-party seeding, so its swarms are
  effectively webseed-only. `dipper download <identifier>` gets everything over
  HTTP and never sees a peer.

## Licence

MIT.
