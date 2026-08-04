# dipper

A single-binary BitTorrent client that uses the Internet Archive as its
metadata and search backend. Named after the white-throated dipper, a small
brown bird that walks into rivers and comes back out with things.

Everything archive.org serves this way is public domain or openly licensed
material that the Archive itself publishes as torrents.

**Status: phase 0 complete.** Search, metadata and torrent resolution work end
to end. The BitTorrent engine itself is not written yet, so dipper currently
hands you a `.torrent` or a magnet link rather than the bytes.

## Install

```sh
cargo install --path crates/dipper-cli
```

## Use

```sh
# harvest search results into a local index (fast, no network after this)
dipper harvest --torrents-only --mediatype audio "field recordings" -n 2000

# search the local catalogue
dipper search "dawn chorus" -n 10
dipper search "mediatype:audio AND has_torrent:true" --json

# or skip the catalogue and ask archive.org directly
dipper search --remote --torrents-only "apollo 11" -n 20

# look at an item
dipper info nasa --files

# get a torrent or a magnet
dipper torrent nasa --show
dipper torrent nasa -o nasa.torrent
dipper magnet nasa

dipper index stats
```

Global flags: `--index-dir` (where the catalogue lives), `--min-interval`
(milliseconds between archive.org requests, default 350), `--polite` (ask
archive.org for reduced priority), `-v` for more logging.

Set `IA_ACCESS_KEY` and `IA_SECRET_KEY` if you have archive.org S3 credentials.
Reads do not need them, but they can raise your priority when the site is busy.

## Layout

| crate | what it does |
| --- | --- |
| `dipper-ia` | archive.org client: metadata API, search, `.torrent` parsing, infohash, magnets |
| `dipper-index` | local tantivy catalogue over harvested metadata |
| `dipper-cli` | the `dipper` binary |

## Tests

```sh
cargo test --workspace                                  # offline, fast
cargo test -p dipper-ia -- --ignored --test-threads=1   # hits archive.org
```

The live tests are the ones that catch archive.org changing under us. The best
of them checks our computed infohash against the `btih` archive.org publishes
for its own derived torrent, which independently validates the bencode scanner
and the SHA-1 path.

## Things archive.org does that you should know about

See [docs/archive-org-notes.md](docs/archive-org-notes.md). The short version:
the scrape API's cursor pagination does not work anonymously, so dipper pages
with `advancedsearch.php` and lives with its 10,000 result ceiling. Also,
derived `.torrent` files can go stale relative to item contents, which is why
the roadmap has an HTTP fallback in it rather than as an afterthought.

## Roadmap

- [x] **Phase 0** archive.org metadata and search, local index, torrent and magnet resolution
- [ ] **Phase 1** BEP 19 webseed download, piece verification, resume. For archive.org items this alone downloads nearly everything, since almost all bytes come over HTTP
- [ ] **Phase 2** BEP 3 peer wire, HTTP and UDP trackers (BEP 15, 23), rarest-first, endgame, choking
- [ ] **Phase 3** DHT (BEP 5), magnet resolution via BEP 9 and 10, PEX (BEP 11)
- [ ] **Phase 4** uTP, UPnP/NAT-PMP, hybrid v2 read support

## Licence

MIT.
