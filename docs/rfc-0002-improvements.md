# RFC-0002: What balerion needs next

**Status:** in progress
**Written against:** `ddf1eef`, 2026-08-20
**Supersedes:** nothing. [RFC-0001](rfc-0001-research.md) is the founding research and still stands.

## Progress

**Phase 1 is done.** Peer exchange is read and fed to the peer queue (1.1). The
announced port is now a port we listen on, one socket shared by every torrent
in the process (1.2). Kept torrents survive a restart and abandoned directories
are collected rather than leaked (3). The site is built and type-checked in CI
(8.1). The player is gated by a token whenever it is bound off loopback (9).

Two things turned up while doing it and were fixed in passing. `Picker::remove_peer`
existed, was unit-tested, and was never called by the session, so availability
counts only ever grew and rarest-first quietly stopped being rarest-first on any
swarm that had churned. And the guard added for item 9 originally required
`ConnectInfo`, which answers 500 when the service is not built with one; it now
treats an unknown peer address as "not local" so a wiring mistake asks for the
token rather than refusing everybody.

One correction to section 9 as written: a `tracing::warn!` for non-loopback
binding did already exist. It went to the log rather than to stdout, and warning
was all it did.

**Phase 2 is done.** Every torrent carries a timeline now (resolve, first peer,
first piece, first byte out), reported in its stats, so the performance claims
in this document can start being measured rather than reasoned about. The
transcoder reports how it is coping as a realtime ratio, which is the number
that decides whether a file can be watched as it converts. The torrent file
written for item 3 doubles as a metainfo cache, so replaying something skips
discovery and BEP 9 entirely, and a torrent already complete on disk opens
without touching the network at all. Connect and read timeouts are now separate
(1.4), so a dead address holds a connection slot for three seconds rather than
twenty.

Not done from phase 2: dialling more addresses than there are slots. With a
three second connect timeout the churn is fast enough that it stopped looking
worth the cancellation machinery.

**Phase 3 is done.** Section 4's three layers are all in.

The alignment engine (`subsync.rs`) is the ffsubsync method ported to Rust:
speech-activity masks at 10 ms, cross-correlated by FFT, with a framerate ratio
search over the top. It refuses below a confidence threshold rather than
guessing. Two things there were wrong on the first attempt and are worth
recording: the confidence measure originally divided by the smaller of the two
masks, which scored a perfect 1.0 for three short cues placed anywhere in a film
full of dialogue, and the annotated output originally carried two `WEBVTT`
headers, which a browser answers with an empty track and no error at all.

OpenSubtitles is a new crate (`balerion-osdb`), matching by their file hash
first, which is the case that needs no correcting afterwards. The quota shaped
the design rather than being discovered later: the search runs when a file is
opened, the download only when a viewer turns the track on, and anything fetched
is written beside the video and never fetched twice.

whisper.cpp is wired in as the last resort, in translate mode so that "English
subtitles" means English even when the film is not, running in the background
and landing in the same cache file.

Neither the OpenSubtitles client nor the whisper path has been exercised against
the live services: there is no API key and no model on this machine. The parsing,
the hash, the URL building, the quota classification and the alignment are all
tested offline; the network calls are not.

**Phase 4 is mostly done.** The encode bitrate is derived from the source
resolution and never exceeds what the source has (5.1), so a 480p Prelinger
short no longer gets three megabits it cannot use and a 1080p feature no longer
gets three when it wants six. Hardware encoding now finds NVENC and QuickSync as
well as VideoToolbox; VAAPI is deliberately left out, because it needs a render
device and a filter chain rather than a name, and it would fail in ways that
cannot be tested on this machine. Concurrent transcodes are derived from the
core count. The segment cache is bounded by bytes rather than by a count, which
had meant anywhere between fifty and three hundred megabytes depending on what
was playing. Audio track selection is in, with the chosen track part of the
cache key, because otherwise switching tracks is answered from the cache with
the old audio and the feature simply appears not to work.

**5.2, keyframe-aligned segments, is deliberately not done.** Building the
keyframe index needs to read the whole file, which is the one thing a streaming
torrent cannot do up front; doing it incrementally is a large piece of work with
a real risk of regressing playback that currently works. The encoder ratio added
in phase 2 is precisely the measurement that says whether it is worth it, and it
should be read before the work is scheduled rather than after. That is what
phase 2 was for.

Not done from 5.3: a disk segment cache. Keeping every segment of a transcode
beside the torrent would roughly double what a converted film costs to hold,
which is a poor trade for making a scrub backwards instant.

**Phase 5 is under way.** Continue watching is in end to end: positions are kept
per file in one JSON file that outlives the torrent it refers to, the player
reports where it has got to and resumes there, and the page has a row for what
you are part way through. The source seam is built (6.1) with fan-out and
infohash deduplication, and Torznab is in as a crate of its own (6.2), verified
against a fixture server rather than only unit-tested.

Two measurements worth recording, both from the instrumentation added in phase
2. A cached resolve of a magnet that had been played before took **1.1 seconds
against 115 for the first**, without touching a swarm at all. And a first
diagnosis of mine was **wrong**: I attributed a fifty second archive.org resolve
to peer discovery, and the trace showed the Archive's own latency plus a retry
was most of it. The discovery skip for webseeded torrents is still right, since
looking for peers in a swarm that refuses third-party seeding is pointless work,
but it was not the fifty seconds.

Release-name ranking is in (6.4), as a pure scoring function that can be
argued with: resolution first, provenance next, a camcorder recording
disqualified rather than merely ranked low, and a logarithmic swarm term so a
popular bad release cannot beat a good one with a healthy swarm. Next-episode
autoplay is in, cancelled by any sign of a viewer still being in the room.

**6.1 is now finished on both sides.** The relay serves `/find` and `/sources`,
so the hosted site has stopped carrying its own TypeScript translation of the
archive.org and apibay rules: there is one answer to "what does a search mean",
it is in Rust, and the site asks it. Torznab indexers appear in the hosted
menu, along with an *Every index* entry.

The relay was given a `Sources` struct rather than the player's `AppState`. It
was tempting to hand it the whole thing and rely on the routing table to keep
the download machinery unreachable, but the relay's guarantee is about what it
*holds* rather than what it routes: three HTTP clients cannot start a download
whatever anybody wires up later.

archive.org stays a direct call from Vercel, deliberately. It answers a
datacentre address perfectly happily, and the one index whose rights are
actually clear should not stop working because somebody's laptop is shut.

Three faults found by opening the built page rather than by reading it: the
results count read "showing 24 of undefined", because the seam reports no grand
total and there is no such number once duplicates have been folded; the
collection menu was drawn empty for an index that has no collections; and every
row said "Copy identifier" including the ones where the button copies a magnet,
which in an *Every index* search is most of them.

Still open in phase 5: the artwork decision (6.3), which needs a TMDB key and
therefore needs Chief.

**Phase 8, the stability work, is largely done.** The site and the player's own
JavaScript are both in CI (8.1). The long-lived tasks are supervised, so a panic
in the sweeper is logged loudly and the loop restarted rather than absorbed by
the runtime and reported nowhere (8.4). A nightly workflow runs the live
archive.org tests, which are the only thing that would notice the Archive
changing its behaviour under us again (8.6). The player's arithmetic now lives
in `assets/lib.js` with `node --test` beside it (8.2).

Two things that came out of doing 8.2 and are the reason it was worth doing.
The first test written against the extracted code found that `roughly` emitted
**"about 1 minutes"** for anything between ninety seconds and two and a half.
And extracting the functions at all broke the entire player: a bare
`function bytes()` at the top level of a classic script is a global, so app.js's
`const { bytes } = ...` failed with "Identifier 'bytes' has already been
declared", which kills the whole script and leaves a page with no event handlers
whatsoever. `node --check` passes on both files, the Rust suite passes, and the
player is dead. It was found by opening the page in a browser and looking, which
is the only way it could have been found.

**The unwrap audit (8.5) is done, and came out clean.** Twenty-four
`unwrap`/`expect` sites outside tests in the engine, in three groups: lock
poisoning, which is fine because a poisoned mutex means another thread has
already panicked; conversions that are infallible by construction, such as a
twenty byte infohash into a twenty byte DHT id; and fixed-offset slices in the
decoders, every one of which is preceded by an explicit length check.

An audit is a claim about a moment, so it has been written down as a test
instead. `tests/truncation.rs` feeds **every prefix** of a valid peer message,
handshake, UDP tracker reply, compact peer list, resume file, info dict, torrent
file and magnet to its decoder, and requires only that none of them panics.
Roughly 640 prefixes, and truncation rather than random bytes because truncation
is what actually happens: a short read on a socket is ordinary, and so is a
resume file from a process that was killed mid-write.

Still open from phase 8: a metrics dump beyond the per-torrent timeline (8.3).

**Phase 6, protocol encryption, is done for outgoing connections.** MSE/PE with
RC4, in a new `mse` module, with the primitives checked against published test
vectors rather than against our own arithmetic and the conversation checked
against a fixture that implements the other half from the specification and
refuses plaintext outright.

Plaintext is tried first and the obfuscated handshake is the retry, which is the
cheap order: most peers take plaintext, so the common case pays nothing. The
cost is one extra dial to every address that accepts a socket and then hangs up,
which on a public swarm is a great many of them, so `--no-encryption` turns it
off and the peer-supervisor test that counts dials sets it.

Two honest limits. **Incoming** encrypted connections are still refused: routing
one needs the Diffie-Hellman exchange to happen in the listener before the
obfuscated infohash can be matched against the running torrents, which
restructures `inbound.rs` and has not been done. And none of this has been
exercised against a real client — the fixture is one reading of one
specification on both sides. The fallback structure bounds the risk: a broken
handshake costs nothing, because plaintext had already failed.

## Casting, which was asked about separately

**AirPlay works now and did not before**, for a reason worth recording. AirPlay
does not mirror a video element: it hands the receiver a URL and the receiver
fetches the media itself. The player was setting the video's `src` to a relative
path, so what Safari handed an Apple TV was `http://127.0.0.1:8080/...`, which
reaches nothing. The button appeared to do nothing at all, and the `--cast-port`
address existed only to be copied out by hand.

Playback is now pointed at the cast listener's LAN address whenever there is
one. It serves exactly the same bytes, the browser fetching from its own
machine's LAN address costs nothing, and it is the only URL a receiver can act
on.

**Chromecast is still not properly supported**, and that one is a decision
rather than missing code. A Chromecast needs the page to speak to it through
Google's Cast SDK, loaded from `gstatic.com`. This page currently loads nothing
from anywhere, which is a promise in its own footer. Adding the SDK breaks that
promise for everyone, including the people who never cast anything, so it is
Chief's call and not mine.

## What this is

A single list of everything worth doing to balerion, ordered by what a viewer
notices rather than by which crate it lands in. Every claim about the current
code was checked against the tree at `ddf1eef`; where something is a guess it
says so, and everything unverified is collected at the end rather than smuggled
into the argument.

The stated goal is to rival Netflix and an Apple TV. It is worth being precise
about which parts of that are achievable, because two of them are not.

**Not achievable:** licensed catalogue, and the certainty that comes with it.
Netflix knows the file is there, knows its bitrate, and knows it will still be
there tomorrow. balerion is asking strangers, and some of them will say no.

**Already better:** it plays anything, from anywhere, and the thing you watch is
also the thing you keep. No client on that list will open an MPEG program stream
carrying AC-3 from 1953, and balerion does it without being asked.

**Winnable, and where the work is:** the sofa experience. Time to first frame,
never hitting a dead end, subtitles that match the speech, resuming where you
left off, and a page that looks like somewhere to choose a film rather than a
directory listing. Everything below serves one of those five.

---

## 1. Getting more peers, and getting them sooner

This is the foundation, because everything else in the document is downstream of
whether bytes arrive. Three of the four items here are genuine defects rather
than missing features.

### 1.1 PEX is advertised and then thrown away

`extended.rs:49` puts `ut_pex` in our extended handshake, which tells every peer
we connect to that it may send us peer exchange messages. Nothing anywhere
parses one. `Source::Pex` exists in `discovery.rs:46` and has no construction
site in the codebase.

This is worse than not advertising it. We ask peers to spend bandwidth telling
us about other peers, and drop every message on the floor. It is also the single
cheapest fix to the problem the last commit was about: *"keep looking for a peer
with the file list"*. On a public swarm, the peer that will answer a metadata
request is very often one the tracker never named, and PEX is how mature clients
find it.

**Do:** parse incoming `ut_pex` (the `added` and `added6` compact fields), feed
the addresses to `SessionHandle::add_peers`, which already exists and already
does the deduplication. Perhaps eighty lines including tests.

### 1.2 Nothing listens on the port we announce

`DownloadConfig::port` defaults to 6881 and is sent to trackers as the announce
port and to peers in the extended handshake as `listen_port`. Nothing binds it.
The only `TcpListener` in `balerion-bt` is inside a test.

So balerion is outbound-only, and tells the swarm otherwise. Every peer that
learns about us from a tracker or from someone else's PEX tries to connect,
fails, and gives up. On a well-seeded torrent this costs little. On the thin,
awkward swarms where balerion currently struggles, refusing half the available
connections is exactly the wrong economy.

**Do:** bind `peer_port`, accept connections, run the same handshake in reverse,
and hand the connection to the existing peer loop. Leech-only is unaffected: an
inbound peer is still a peer we can download from. Add UPnP-IGD and NAT-PMP
opportunistically, degrading silently when the router refuses, which most will.

### 1.3 No protocol encryption

balerion speaks plaintext BitTorrent only. A meaningful fraction of public-swarm
peers are configured to require encrypted connections and will refuse us
outright, and some consumer ISPs still shape or block plaintext BitTorrent
outright. MSE/PE is not cryptographically serious and is not meant to be; it is
the handshake everybody else speaks.

**Do:** implement MSE with RC4 as the obfuscation layer, negotiated per
connection, preferring encrypted and falling back to plaintext. This is the
largest item in section 1 and the one I would schedule last of the four.

### 1.4 Dead addresses hold live connection slots

`max_peers` defaults to 30 and `peer_timeout` to 20 seconds, and the same
timeout covers both the initial connect and every subsequent read. A tracker
naming sixty peers of which forty are unreachable means forty of those slots are
occupied for twenty seconds each doing nothing.

**Do:** split the connect timeout from the read timeout, at something like three
seconds against twenty, and let the supervisor dial more addresses than there
are slots, keeping the first ones home. Time to first byte on a cold magnet is
dominated by this and not by anything clever in the picker.

### 1.5 A small correctness nit in the piece loop

`fetch_piece_from_peer` decides a piece is complete when
`blocks.values().map(len).sum() >= piece_size`. `blocks` is a `HashMap` keyed by
offset, so an exactly repeated block overwrites itself harmlessly, but blocks at
unaligned or overlapping offsets can push that sum past `piece_size` while
leaving a hole in the middle. The hole is zeroes, the SHA-1 check catches it,
and the piece is refetched from someone else, so this is wasted bandwidth rather
than a hazard. Worth replacing with an actual coverage check, which is no longer
than the current version.

### 1.6 The decision that is not mine: seeding

balerion leeches and says so, in the README and in the interface. Uploading
would help swarm health and would make us a better citizen of every swarm we
join. It also changes the legal posture materially, from receiving to
distributing, which is a different thing in most jurisdictions and is Chief's
call rather than a default anyone should quietly flip.

**Recommendation:** stay leech-only by default. Sections 1.1 and 1.2 recover
most of the peer-discovery benefit without touching that question at all. If
seeding is ever wanted, make it an explicit flag with an explicit warning, and
note that archive.org's trackers will refuse it regardless.

---

## 2. Time to first frame

There is currently no instrumentation for this, which means every claim in this
section including my own is a guess. That is the first thing to fix.

**Do first:** a timing trace through the whole path, emitted at info level and
readable from the CLI: resolve started, metadata acquired, first peer
handshaked, first piece verified, first byte served to the browser, first frame
decoded. Until that exists there is no way to tell a slow swarm from a slow disk
from a slow encoder, and we would be optimising by anecdote.

**Then:** cache resolved metainfo on disk, keyed by infohash. Today every
`/api/resolve` of a magnet redoes the entire swarm dance: discovery, connect,
BEP 9, hash check. Playing the same thing twice pays twice. A `.torrent` cache
directory makes the second play instant and, more usefully, makes a restart
cheap. This pairs with section 3 and shares its storage.

**Then:** the metadata bootstrap itself. `METADATA_PEERS` is 8 and the loop in
`torrent.rs` re-runs discovery when they all refuse. With PEX feeding it, that
loop gets much better candidates much sooner. Measure before widening the fan-out
any further.

---

## 3. The library that forgets

This is the highest usability-per-line item in the document and it is a
half-day's work.

`AppState::torrents` is a `HashMap` in memory. Nothing scans `data_dir` at
startup. Two consequences, both verified:

**Kept torrents vanish.** Tick "Keep offline", restart balerion, and the library
is empty. The bytes are still on disk and `.balerion-keep` is still sitting
next to them, but nothing reads it until the same magnet is resolved again, at
which point `routes.rs:171` picks the marker up. Until then, "On this machine"
is telling the viewer something false.

**Unkept directories leak forever.** `sweep` at `routes.rs:360` iterates
`state.all()`, which is the in-memory map. A directory whose torrent was
evicted by a restart is invisible to it. Browse five films, restart, and twenty
gigabytes are stranded with nothing in the process aware they exist. The README
promises they are swept after fifteen minutes. They are not, across a restart.

**Do:** write a small sidecar per torrent directory (the `.torrent` itself,
which we want anyway for section 2, plus the display name). On startup, scan
`data_dir`: adopt anything marked kept into the library without starting a
session, and sweep anything unkept whose mtime is older than the grace period.
Nothing needs a database; the directory is already the record.

---

## 4. Subtitles that match the speech

Requested explicitly, and the largest single piece of new work here.

### Where we are

`subtitles.rs` handles two sources and no more: `.srt`/`.vtt` sitting in the
same torrent, matched on filename stem by `belongs_to`, and tracks embedded in
the container, extracted by ffmpeg to WebVTT. The Windows-1252 fallback is
right and the SubRip conversion is right. Language is guessed from a two-letter
suffix in the filename and is cosmetic when wrong.

Nothing fetches subtitles, nothing checks whether they are in sync, and nothing
can produce them when they do not exist.

### The three failure modes, which are different problems

**Absence.** Most apibay releases carry no subtitles at all, and a great many
archive.org items carry none either.

**Constant offset.** A subtitle file made for a different release of the same
film starts at a different point, because the release has different leaders,
different logos, or a different cut. Typically a few seconds, occasionally
thirty.

**Framerate drift.** The one that ruins an evening. A subtitle timed against a
25 fps PAL transfer, played against a 23.976 fps source, drifts by 4.3%. Over
ninety minutes that is nearly four minutes adrift by the end, starting from
perfectly correct at the beginning. No constant offset fixes it, and a viewer
who nudges the offset at minute ten will have to nudge it again at minute
twenty.

### The plan, in layers

**Layer 1: source.** Add an OpenSubtitles client. Match by their file hash
first, which is computed from the file size plus the first and last 64 KiB. That
detail is a gift: the streaming picker already fetches the head, and
`TAIL_PIECES` already keeps the last two pieces warm for the index box, so the
bytes needed to compute the hash are the bytes we already have first. A hash
match means subtitles made for *this exact release*, which resolves both timing
problems at once. Fall back to a title/season/episode search using what
`media::episode_of` already parses.

The honest constraint: OpenSubtitles requires an API key, and the free tier is
five downloads per day anonymously, ten per day authenticated. That is a
household's evening, not a service. It makes the local cache mandatory (key
subtitles by infohash and file index, keep them forever, they are kilobytes) and
it means layer 3 is not a luxury.

**Layer 2: synchronisation.** Port the ffsubsync approach to Rust. It is a
signal alignment problem and it is language-agnostic, which is what makes it
robust:

1. Decode the audio to 16 kHz mono through ffmpeg, which is already a
   dependency and already reads our own range endpoint.
2. Reduce it to a binary speech-activity mask at 10 ms resolution, via a voice
   activity detector or, to start with, a smoothed energy threshold.
3. Build the same 10 ms mask from the subtitle cue timings: 1 where a cue is on
   screen, 0 elsewhere.
4. Cross-correlate the two with an FFT (`rustfft`) and take the offset that
   maximises agreement.
5. Repeat across candidate framerate ratios (24/23.976, 25/23.976, 30/29.97 and
   their inverses), keep the best, and reject any scale factor deviating from
   1.0 by more than about 10%, which is ffsubsync's own default and rejects
   nothing legitimate.

Report a confidence score and **refuse to apply a shift we are not confident
about**. A subtitle track that is slightly wrong is annoying; one that has been
confidently moved to somewhere worse is the sort of thing that makes people stop
trusting a feature entirely.

Run this on sidecar tracks too, not only fetched ones. A `.srt` bundled in a
torrent is very frequently for a different release than the video sitting next
to it, and the current code trusts it completely.

**Layer 3: generation.** When nothing exists, transcribe. whisper.cpp as an
optional dependency detected the same way ffmpeg is, absent meaning the feature
politely does not appear. Transcription is in sync by construction, which side-
steps layer 2 entirely, and its translate task produces English from foreign
audio, which is the other half of "automatic English subtitles". It is expensive
and it can run segment by segment behind the playhead rather than needing the
whole file, which fits how the rest of the player already works.

**Order of preference:** embedded English track, then a sidecar (sync-checked),
then OpenSubtitles by hash, then OpenSubtitles by title (sync-checked), then
generated. Show the viewer which one they got, because "these are machine
transcribed" is worth knowing before judging the punctuation.

---

## 5. The picture

### 5.1 Every transcode gets 3 Mbit/s regardless of what it is

`ffmpeg.rs` hardcodes `-b:v 3M`. A 480p Prelinger short is handed three
megabits it cannot use, and a 1080p feature is handed three megabits when it
wants eight. Both directions are wrong: one wastes swarm bandwidth on encoder
output nobody can see, the other is the reason a good release looks soft.

**Do:** derive the target from the source resolution and bitrate, capped by the
measured swarm rate the front end already computes for its feasibility
advisory. The information is all present, it is simply not being used.

### 5.2 Video is always re-encoded, even when it is already H.264

`plan()` sets `copy_video = false` unconditionally, and the comment explaining
why is correct: a copied stream cannot be cut anywhere but a keyframe, so a
fixed six-second segment boundary produces a segment longer than it claims,
which the player places at the wrong time, which looks like drifting audio.

The consequence is that the majority of what apibay serves, which is already
H.264 in an MKV, is decoded and re-encoded in full, when the only thing actually
needed was a different wrapper. That is the single largest CPU cost in the
program and it is spent producing a slightly worse picture than the input.

**Do:** build a keyframe index lazily from `ffprobe -show_packets` over the
head of the file, extend it as playback advances, and cut segments on keyframe
boundaries rather than on six-second ones. Variable-length segments are what
HLS was designed for and the playlist already exists. Then `-c:v copy` becomes
correct for H.264 sources and the encoder only runs for the things that
genuinely need it.

RFC-0001's phrasing was that this is "far too much machinery for this one". I
think that judgement was right at the time and is wrong now: the player has
grown into something people watch films on, HLS is already in the codebase for
AirPlay, and the CPU saved is what makes running this on a small always-on box
viable at all.

### 5.3 Smaller things in the same area

- The segment cache is 24 segments in memory (`state.rs`), discarded on
  restart. Put it on disk beside the torrent. Scrubbing back over something
  already encoded should never re-encode it.
- `MAX_TRANSCODES` is 3. Fine on a laptop, arbitrary on anything else. Derive
  it from the core count.
- Hardware encoding detects VideoToolbox and otherwise falls to `libx264`.
  Add VAAPI and NVENC detection, which is the same three lines of `-encoders`
  grepping, and matters if this ever runs on a Linux box.
- Audio takes the first stream and forces stereo. Films with a commentary track
  or a second language get whichever the muxer happened to put first, with no
  way to change it. Offer track selection in the player, using the stream list
  ffprobe already returns.

---

## 6. Knowing what to watch

This is the section where the product either feels like a service or feels like
a file manager.

### 6.1 One search, several indexes

Today there are two disjoint indexes behind one dropdown, with two near-parallel
handlers in Rust (`search.rs`, `tpb.rs`) and a port of each in TypeScript. A
third source added by copy makes five things that must agree about what a search
means.

**Do:** a source seam. One `Hit` type, one shortlist and seeder floor
generalised over it, and a `/api/sources` endpoint feeding the dropdown the way
shelves and categories already do. Then fan out across the selected sources with
a deadline, render what came back, **dedupe by infohash** and union the seeder
counts. Deduplication is not optional once there is more than one index: the
pick-and-play work depends on there being one obvious row per episode, and four
copies of the same release from four indexes destroys exactly that.

### 6.2 Torznab, which is one client and many indexes

Prowlarr, Jackett, Zilean and bitmagnet all speak Torznab. One client
implementation gets all of them, configured by the user rather than by us, and
running on the user's own machine, which incidentally sidesteps the Cloudflare
datacentre problem that `relay.rs` exists to work around. This is the highest
leverage addition to the search half by a wide margin.

### 6.3 Artwork, and the thing that makes a browse feel like a product

The site already uses TVmaze for show titles, deliberately chosen because it
needs no API key. Films have no equivalent, and a grid of posters is most of
what separates "choose something to watch" from "here is a list of filenames".

TMDB is the obvious source and it does require a key, which cuts directly
against the reasoning in `shows.ts`. I do not think that reasoning is wrong,
so this is a decision rather than a recommendation: either accept one key for
artwork, or accept that the browse stays textual. If artwork is wanted, make it
degrade silently to the current text listing when no key is configured, so a
fresh clone still works.

### 6.4 Ranking by what the release actually is

Results are currently ranked by seeders, with a size cap for thin lines. Seeders
tell you whether it will download, not whether it is the one you want. Parsing
the release name for resolution, source, codec and group is well-trodden and
mostly mechanical, and it is what lets "pick and play" pick correctly rather
than pick the most popular. It also feeds section 5.1, because knowing a release
is 1080p x264 tells the transcoder what to aim at before ffprobe has seen a
byte.

---

## 7. Continue watching, and the rest of the sofa

None of these are hard. All of them are the difference between a tool and
something a household uses without being taught.

- **Playback position.** Persist `(infohash, file) → seconds` and offer to
  resume. Enables a "Continue" row, which is the first thing anybody looks at on
  a television.
- **Next episode.** Season packs are already parsed by `episode_of` and already
  ordered. Autoplay the next one, with the usual countdown and a way to stop it.
- **Watched marks.** Same store, one boolean. Cheap, and it is what stops a
  season pack becoming a memory test.
- **A real library view.** Posters where section 6.3 allows, progress bars,
  sorted by last watched rather than alphabetically.
- **Mobile.** The player at 375 pixels is untested as far as I can tell, and a
  web app manifest costs nothing and makes it open fullscreen from a home
  screen. Given the site already exists to be reachable away from home, this is
  the natural client for it.

---

## 8. Stability, and the things that will bite

### 8.1 The site is not built or type-checked in CI

`.github/workflows/ci.yml` runs `cargo fmt`, `cargo clippy` and `cargo test`,
across two platforms, properly. It does not touch `site/` at all. There is
TypeScript in there talking to a relay over OIDC and it can be broken by a
rename with nothing failing until Vercel builds it.

**Do:** add `tsc --noEmit`, `next build` and the lint script to CI. Half an hour.

### 8.2 `app.js` is 1474 lines with no tests

Not proposing a framework, which would cost more than it returns for a page
embedded in a binary. Proposing that the pure functions be extracted and tested:
the feasibility calculation, `episodeTagOf`, the byte and duration formatting,
the MSE buffer arithmetic in `appendWithEviction` and `reseek`. Those are where
the subtle bugs live, they are all pure, and a plain node test runner will do.

### 8.3 There is no way to see what is happening

Related to section 2. There are no metrics beyond what the page polls: no piece
failure rate over time, no peer churn, no time-to-first-byte, no encoder
throughput. When somebody reports "it was slow last night" the honest answer is
currently a shrug.

**Do:** a `--metrics` line-oriented dump, or an unlisted debug endpoint. It does
not need Prometheus, it needs numbers.

### 8.4 Panics disappear quietly

A panic in a peer task is absorbed by the `JoinSet` and reported as a lost peer.
A panic in the sweeper task ends sweeping for the lifetime of the process with
nothing said. Wrap the long-lived tasks so a panic is logged loudly and the task
restarted.

### 8.5 The 321 unwraps

`grep` finds 321 `unwrap()`/`expect()` outside tests in the Rust crates. Most
are lock poisoning and static header parses and are entirely fine. The audit
worth doing is narrow: confirm that none sit on a path reachable from
peer-supplied bytes or from a filename inside a torrent. That is a couple of
hours with a list, and it is the difference between a hostile torrent being
rejected and a hostile torrent taking the process down.

### 8.6 One live test, nightly

The offline suite is good, including the lying-peer test, which is the right
test to have written. What is missing is anything that proves the whole pipeline
still works against a real swarm. A nightly job that fetches a known-good, small,
public-domain archive.org item end to end would catch the class of breakage that
only shows up against the live internet, and archive.org is the right target for
it because it is stable, legal and webseed-backed.

---

## 9. Security posture, briefly

Three things are already right and should be recorded as deliberate rather than
accidental: the cast listener serves media and nothing else, the relay serves
search and nothing else and refuses to start without a token, and the site's
middleware fails closed when no password is configured.

The one gap: `balerion serve --host 0.0.0.0` binds the *whole* player to the
LAN, including `/api/resolve`, which downloads whatever magnet it is handed, and
`DELETE /api/torrents/{hash}`, which deletes. There is no gate on it. That is
precisely the exposure `cast.rs` was written to avoid, still reachable through a
flag.

**Do:** when `--host` is anything but loopback, either require a token or print
a loud warning at startup saying exactly what is now reachable. I would do both.

---

## 10. What not to do

Recorded so the argument does not have to be had twice.

- **uTP (BEP 29).** Real benefit, large surface, and the problems balerion has
  are discovery problems rather than congestion problems. Revisit if PEX,
  inbound connections and encryption all land and swarms are still thin.
- **Our own DHT crawler.** bitmagnet exists, is maintained, and speaks Torznab.
  Section 6.2 gets it for free.
- **BitTorrent v2.** Adoption is still low and v1 remains the interoperability
  baseline. The current behaviour, refusing v2-only magnets with an explanation,
  is correct.
- **Accounts and profiles.** One household, one shared password on the site,
  nothing on the local player because it is on loopback. Adding accounts adds a
  user store, a password reset path and a session store, to solve a problem
  nobody has.
- **A native app.** The web player already casts to televisions and already
  works in Safari. A native shell buys an app icon.

---

## 11. Suggested order

Sequenced by value over effort. Phase 1 is deliberately unglamorous; it is the
set of things that currently lose viewers.

**Phase 1, the defects.** PEX consumption (1.1). Inbound connections (1.2).
Library rehydration and the disk leak (3). Site in CI (8.1). The `--host`
warning (9). All small, all fixing something that is currently untrue rather
than merely absent.

**Phase 2, the instruments.** Timing trace and metrics (2, 8.3). Metainfo cache
(2). Split connect timeout (1.4). This is what makes phase 4 arguable rather
than guessed.

**Phase 3, subtitles.** OpenSubtitles by hash, then the sync engine, then
whisper (4). The sync engine is the interesting one and is self-contained enough
to build and test entirely offline against known-offset fixtures.

**Phase 4, the picture.** Keyframe-aligned segments and `-c:v copy` (5.2),
derived bitrate (5.1), disk segment cache (5.3).

**Phase 5, the product.** Source seam and Torznab (6.1, 6.2). Continue watching
and next episode (7). Ranking (6.4). Artwork, if the key question goes that way
(6.3).

**Phase 6, the long one.** Protocol encryption (1.3).

---

## 12. What I have not verified

In the spirit of [archive-org-notes.md](archive-org-notes.md), the things
asserted above that I have not personally measured:

- **Time to first frame, at all.** Every performance claim in section 2 is
  reasoning from the code, not from a stopwatch. Section 2's first
  recommendation exists because of this.
- **That re-encoding dominates CPU.** Plausible from the ffmpeg arguments and
  from `MAX_TRANSCODES` being 3, but not profiled.
- **The OpenSubtitles hash claim.** I am confident the OSDb hash is size plus
  first and last 64 KiB, and confident the picker fetches the head and tail
  first. I have not confirmed that the tail pieces we keep warm cover the last
  64 KiB in every torrent layout; a file that is not last in a multi-file
  torrent may not have its own tail prioritised.
- **OpenSubtitles quotas.** Five per day anonymous and ten per day free-tier
  authenticated, from their documentation and community reports as of writing,
  not from an account of ours. Worth confirming before building on it.
- **Whisper timing accuracy.** Asserted as "in sync by construction", which is
  true of the audio it transcribed but says nothing about how well its segment
  boundaries land on sentence boundaries. Needs a listen before it is offered as
  the default for anything.
- **That MSE/PE materially increases reachable peers.** Widely repeated, not
  measured by me on the swarms balerion actually touches.
- **The mobile layout.** I have not rendered the page at any width. It may be
  entirely fine.
- **apibay category codes.** Ported into `site/lib/apibay.ts` by hand with a
  comment saying the two copies must not disagree. Nothing enforces that, and
  nothing has checked recently that the codes are still what apibay uses.
