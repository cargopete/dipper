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

## Watch things in a browser

```sh
dipper serve
```

Opens a local player at `http://127.0.0.1:8080`. Paste a magnet, an infohash,
or an archive.org identifier, and the largest playable video starts within
seconds rather than after the download finishes.

That third option matters more than it looks. archive.org's trackers reject
third-party seeding, so its swarms have no peers to ask for a file list, and a
magnet alone cannot resolve. dipper fetches the derived `.torrent` over HTTPS
instead, which is what makes the Archive's public domain film collections
usable here at all.

The trick is that the browser's `Range` requests drive which pieces the engine
fetches next. That matters more than it sounds: plenty of MP4s keep their
`moov` index box at the *end* of the file, so a player's first request is for
the tail. A front-to-back sequential picker never serves it and playback hangs
forever. Letting the requests steer the picker handles both layouts, and makes
seeking work for nothing.

### Containers browsers will not open

`.mp4`, `.m4v`, `.webm` and the common audio formats stream directly. Anything
else is converted as it plays, if ffmpeg is on your PATH.

This matters more than it sounds for the Internet Archive, whose moving image
collections predate MP4 being the default. A typical Prelinger item is an MPEG
program stream carrying MPEG-2 video and AC-3 audio: three layers, none of
which any browser opens, and all of it public domain.

Conversion is on demand and stateless. Each six second segment is a fresh
ffmpeg reading dipper's own range endpoint over HTTP, which means the piece
picker steers for it with no extra plumbing, and seeking anywhere costs one
segment rather than restarting a session. Streams already in a browser-friendly
codec are copied rather than re-encoded, so an H.264 MKV is only rewrapped.
Hardware encoding is used where available (VideoToolbox on macOS), `libx264`
otherwise.

Without ffmpeg, dipper behaves as it always did: those files are listed with an
explanation and a download link. Transcoding is an enhancement, not a
requirement, and the binary still works on its own.

Subtitles are picked up too, both `.srt` files sitting beside the video and
tracks embedded in it, converted to WebVTT. SubRip files are frequently not
UTF-8, so they fall back to a Windows-1252 decode rather than filling the
screen with replacement characters.

Streaming *is* downloading: every piece is SHA-1 verified and written to disk,
so anything you watch is also saved. Torrents live in the user cache directory
and are swept once nobody has watched them for fifteen minutes. Tick **Keep
offline** to stop that and fetch the whole thing.

Serving flags: `--port`, `--host`, `-o <dir>`, `--low-bandwidth`.

`--low-bandwidth` is worth knowing about. dipper normally keeps a quarter of a
megabyte of requests outstanding per peer, which across thirty peers is a lot
of other people's blocks queued in front of the piece your player is stalled
on. The flag winds both figures down: less peak throughput, much shorter queue.

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
Streaming swaps that for a priority list driven by the player: what the browser
asked for, then a readahead scaled to the measured download rate, then the tail
of the file, then the rest of that file. The rest of the torrent comes last, and
only for torrents you asked to keep, so a 900 MB extras track cannot compete
with the film you are watching.
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
| `dipper-web` | the local player: range-driven streaming, on-demand transcoding, page embedded in the binary |
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
