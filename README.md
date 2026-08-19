# balerion

[![CI](https://github.com/cargopete/balerion/actions/workflows/ci.yml/badge.svg)](https://github.com/cargopete/balerion/actions/workflows/ci.yml)

A single-binary BitTorrent client that will also go and find something to watch.
Named for Balerion, who went and fetched things without asking permission
first.

The engine takes a magnet link, a `.torrent`, or an archive.org identifier and
produces verified bytes on disk. It is deliberately provenance-agnostic: below
the CLI it knows only a 20-byte infohash and has no idea where you got it.

## Install

```sh
cargo install --path crates/balerion-cli
```

## Watch things in a browser

```sh
balerion serve
```

Opens a local player at `http://127.0.0.1:8080`. Paste a magnet, an infohash,
or an archive.org identifier, and the largest playable video starts within
seconds rather than after the download finishes.

That third option matters more than it looks. archive.org's trackers reject
third-party seeding, so its swarms have no peers to ask for a file list, and a
magnet alone cannot resolve. balerion fetches the derived `.torrent` over HTTPS
instead, which is what makes the Archive's public domain film collections
usable here at all.

### Two indexes behind the one search bar

The search bar asks either archive.org or apibay, the JSON endpoint behind
thepiratebay's frontend. Both hand the resolver something it already accepts:
the Archive gives an item identifier, apibay gives a magnet assembled locally
from the infohash it returns. Neither the resolver nor the engine below it is
told which happened, which is the same provenance-agnostic split the CLI has.

apibay searches are restricted to video categories, and never to `cat=0`, which
searches everything including the adult categories and will return them for an
innocent query. Results with no seeders are dropped rather than offered, because
a magnet nobody is seeding is not a slow download, it is one that never starts,
and in the interface it is indistinguishable from one still looking for peers.
The count of what was dropped is shown next to the results rather than quietly
swallowed.

### Defending a thin connection

The search box has a "fits a thin line" toggle, and it is a size filter rather
than anything to do with the transcoder. Transcoding cannot help here: ffmpeg
reads the source back through balerion's own range endpoint, so every byte of a
2.7 GiB episode is fetched from the swarm whatever resolution comes out of the
other end. The only way to need fewer bytes is to pick a smaller release, and
that decision belongs in the search results.

The cap is derived rather than chosen: 1.5 Mbit/s, which is about what a poor
line sustains, times the category's typical runtime. That works out at 483 MiB
for an episode and 1.2 GiB for a feature. The runtime is a stated guess, because
apibay reports a size and never a duration, and nothing derived from it is shown
as though it were measured. Size is still the right thing to filter on: within
one search every result is the same programme at a different bitrate, so sizes
rank exactly as bitrates do.

Two honest limitations. The cap measures the whole torrent, so a season pack is
excluded even though balerion would only fetch the episode you played. And with
the cap on, the HD categories are usually empty, which is the true answer rather
than a bug: no 1080p release of anything streams on 1.5 Mbit/s.

What is on the other end is a public index of whatever strangers uploaded. Most
of it is copyrighted, none of it is cleared, and the category is not a licence.
balerion says so under the search box rather than in a comment nobody reads.

The trick is that the browser's `Range` requests drive which pieces the engine
fetches next. That matters more than it sounds: plenty of MP4s keep their
`moov` index box at the *end* of the file, so a player's first request is for
the tail. A front-to-back sequential picker never serves it and playback hangs
forever. Letting the requests steer the picker handles both layouts, and makes
seeking work for nothing.

### Not yet, rather than never

ffprobe has to read the head of a file before anything can be said about it, and
on a fresh torrent those bytes have not arrived. That used to be reported as
"this file cannot be played, download it and use VLC", which sends you away from
a perfectly good file about thirty seconds before it would have worked.

A probe that fails on a torrent still filling up now says so, with the piece
count, and the page asks again every few seconds instead of giving up. The
verdict only becomes "unsupported" once the torrent is complete and the file
still cannot be read, which is the only time it is true.

### One episode at a time

Three copies of the same episode do not download three times faster: they share
the peers you have between them, so each is slower than one would have been.
That is easy to do by accident and invisible while it costs you. The library now
says when it is happening, and how many peers are being split.

### Casting to a television

```sh
balerion serve --cast-port 8081
```

A television is not a browser tab. An Apple TV or a Chromecast is a separate box
on the network: it is handed a URL and fetches the media itself, so nothing can
be cast that does not exist as a resource it can reach. That rules out two
things. `127.0.0.1` is no use to another device, and the MediaSource path hands
the browser a blob with no URL behind it at all.

So `--cast-port` opens a second listener, bound to every interface, serving the
media and nothing else: byte ranges of files already being fetched, plus the HLS
playlist and segments that go with them. It cannot start a download, cannot stop
one, cannot list what is on disk and will not serve the page. The worst anyone on
your network can do with it is watch something you are already watching. Binding
the whole player to the LAN instead would hand them `/api/resolve`, which
downloads whatever magnet it is given.

Transcoded files are served as HLS over the same fragmented MP4 segments the
browser already uses, so one representation feeds the browser, AirPlay and
Chromecast alike. Safari plays that playlist natively, which is what makes its
AirPlay button work; other browsers keep using MediaSource for now. The player
shows the address to hand a receiver, since working out your own is nobody's idea
of a feature.

Off unless asked for, because it is the only setting that exposes nothing.

### Containers browsers will not open

`.mp4`, `.m4v`, `.webm` and the common audio formats stream directly. Anything
else is converted as it plays, if ffmpeg is on your PATH.

This matters more than it sounds for the Internet Archive, whose moving image
collections predate MP4 being the default. A typical Prelinger item is an MPEG
program stream carrying MPEG-2 video and AC-3 audio: three layers, none of
which any browser opens, and all of it public domain.

Conversion is on demand and stateless. Each six second segment is a fresh
ffmpeg reading balerion's own range endpoint over HTTP, which means the piece
picker steers for it with no extra plumbing, and seeking anywhere costs one
segment rather than restarting a session. Streams already in a browser-friendly
codec are copied rather than re-encoded, so an H.264 MKV is only rewrapped.
Hardware encoding is used where available (VideoToolbox on macOS), `libx264`
otherwise.

Without ffmpeg, balerion behaves as it always did: those files are listed with an
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

`--low-bandwidth` is worth knowing about. balerion normally keeps a quarter of a
megabyte of requests outstanding per peer, which across thirty peers is a lot
of other people's blocks queued in front of the piece your player is stalled
on. The flag winds both figures down: less peak throughput, much shorter queue.

## Search and fetch in one go

```sh
balerion get "dawn chorus birdsong"
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
balerion download "magnet:?xt=urn:btih:481b6e...071e&tr=http://bttracker.debian.org:6969/announce"

# a .torrent on disk
balerion download ./debian-13.6.0-amd64-netinst.iso.torrent -o ~/Downloads

# an archive.org item, by identifier
balerion download nasa -o ~/Downloads

# see what a magnet actually is, without downloading it
balerion resolve "magnet:?xt=urn:btih:481b6e...071e"
```

Interrupt a download and run it again: it picks up where it stopped. A resume
file next to the download records which pieces verified, so a restart does not
mean re-reading the lot. It is only trusted when it was written cleanly; after
a kill -9 balerion falls back to re-hashing, and `--verify` forces that anyway.

Engine flags: `--no-dht`, `--no-webseeds`, `--port`, `--max-peers`,
`--dht-seconds`, `--verify`, `--quiet`.

## Search archive.org

```sh
# harvest search results into a local index (fast, no network after this)
balerion harvest --torrents-only --mediatype audio "field recordings" -n 2000

# search the local catalogue
balerion search "dawn chorus" -n 10
balerion search "mediatype:audio AND has_torrent:true" --json

# or skip the catalogue and ask archive.org
balerion search --remote --torrents-only "apollo 11" -n 20

balerion info nasa --files
balerion magnet nasa
balerion torrent nasa --show
balerion index stats
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
or a webseed. Uploading is not implemented: balerion leeches, and says so.

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

## The search front end

`site/` is a small Next.js app that deploys Balerion's search half to Vercel,
password-gated. It finds things and hands over a magnet; it plays nothing,
because a serverless function cannot hold a swarm open.

archive.org is searched from there directly. apibay is not, because it serves a
Cloudflare bot challenge to datacentre addresses, so those queries go through
`balerion relay` on a machine with a domestic connection. The relay serves the
search and nothing else, and authorises callers by verifying Vercel's own OIDC
tokens, so there is no shared secret. See [site/README.md](site/README.md).

## Layout

| crate | what it does |
| --- | --- |
| `balerion-bt` | the engine: magnet, bencode, metainfo, trackers, DHT, peer wire, picker, storage, webseeds |
| `balerion-ia` | archive.org client: metadata API, search |
| `balerion-tpb` | apibay client: search, and magnets built from the infohash |
| `balerion-index` | local tantivy catalogue over harvested metadata |
| `balerion-web` | the local player: range-driven streaming, on-demand transcoding, page embedded in the binary |
| `balerion-cli` | the `balerion` binary |

## Tests

```sh
cargo test --workspace                                  # offline, fast
cargo test -p balerion-ia -- --ignored --test-threads=1   # hits archive.org
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
  small file. balerion slices the body itself.
- archive.org's trackers reject third-party seeding, so its swarms are
  effectively webseed-only. `balerion download <identifier>` gets everything over
  HTTP and never sees a peer.
- apibay never answers an empty search with `[]`. It returns a single row with
  id `0` and a name of "No results returned", which parses perfectly and renders
  as an entirely convincing fake result. It also has no browse: an empty query
  gets the same sentinel, so balerion refuses one rather than reporting that
  nothing matched.

## Licence

MIT.
