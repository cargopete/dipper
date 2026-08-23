/* balerion: the page.
 *
 * No framework and no build step. The server is a single binary and the page
 * it serves should not need a toolchain to change.
 *
 * Polling rather than websockets, deliberately: the interesting state is a few
 * numbers once a second, and a socket that has to reconnect on every network
 * hiccup is worse on exactly the bad connections this is meant to survive. */

const $ = (id) => document.getElementById(id);

const el = {
  form: $("intake-form"),
  magnet: $("magnet"),
  submit: $("intake-submit"),
  searchForm: $("search-form"),
  query: $("query"),
  source: $("source"),
  filter: $("filter"),
  filterLabel: $("filter-label"),
  filterNote: $("filter-note"),
  sourceNote: $("source-note"),
  thinRow: $("thin-row"),
  thin: $("thin"),
  thinLabel: $("thin-label"),
  searchSubmit: $("search-submit"),
  results: $("results"),
  resultList: $("result-list"),
  resultsCount: $("results-count"),
  modes: document.querySelectorAll(".modes button"),
  pending: $("pending"),
  pendingNote: $("pending-note"),
  failure: $("failure"),
  failureBody: $("failure-body"),
  torrent: $("torrent"),
  name: $("torrent-name"),
  meta: $("torrent-meta"),
  detailsToggle: $("details-toggle"),
  details: $("download-details"),
  keep: $("keep"),
  discard: $("discard"),
  backToResults: $("back-to-results"),
  viewer: $("viewer"),
  video: $("video"),
  audioRow: $("audio-row"),
  audioTracks: $("audio-tracks"),
  continuing: $("continuing"),
  continuingList: $("continuing-list"),
  viewerNote: $("viewer-note"),
  files: $("file-list"),
  map: $("piecemap"),
  mapStatus: $("piecemap-status"),
  advisory: $("advisory"),
  remote: $("remote"),
  remoteStatus: $("remote-status"),
  remoteBack: $("remote-back"),
  remoteToggle: $("remote-toggle"),
  remoteForward: $("remote-forward"),
  remoteShelf: $("remote-shelf"),
  airplay: $("airplay"),
  library: $("library"),
  libraryList: $("library-list"),
  intakeStatus: $("intake-status"),
  downloadedPanel: $("panel-downloaded"),
  startingList: $("starting-list"),
  downloadedList: $("downloaded-list"),
  downloadedEmpty: $("downloaded-empty"),
  duplicates: $("duplicates"),
  peers: $("t-peers"),
  rate: $("t-rate"),
  buffer: $("t-buffer"),
  pieces: $("t-pieces"),
  disk: $("t-disk"),
};

let current = null;   // the TorrentInfo we are showing
let playing = null;   // index of the file in the player
let poller = null;
let filters = [];     // the subsets offered by the selected index
let lastStats = null; // the most recent poll, for the feasibility advisory
let playInfo = null;  // what /api/play said about the file being played
let chosenAudio = 0;  // which audio track the viewer picked, when they did
const audioQuery = () => (chosenAudio ? `?audio=${chosenAudio}` : "");

/* Firefox on iPhone exposes WebKit's picker, but does not complete a web-video
 * receiver hand-off to an LG. Safari does. Do not offer a button that turns a
 * working phone player into a spinner. */
function canPickAirplay() {
  return (
    !!castBase &&
    typeof el.video.webkitShowPlaybackTargetPicker === "function" &&
    isSafariAirplayBrowser(navigator.userAgent)
  );
}

/* ---- formatting ---------------------------------------------------------
 *
 * These live in lib.js, which is loaded first and has tests of its own. They
 * are the parts of this file that are only arithmetic, and they were the parts
 * with no way to run them outside a browser. */
const { bytes, seconds, roughly, episodeTagOf, seriesOf, feasible, shouldChangeVerdict, mediaUrl, stateOf } =
  window.BalerionLib;

function show(node, visible) {
  node.hidden = !visible;
}

/* ---- talking to the server --------------------------------------------- */

// A LAN player is deliberately gated.  Cookies are convenient, but an iPhone
// browser may delay or decline persisting one for a bare IP address.  Keep the
// invitation from the opening link on same-origin requests as well, so a
// perfectly valid page cannot turn into an empty shell at its first fetch.
const accessToken = new URLSearchParams(window.location.search).get("balerion_token");

function withAccessToken(path) {
  if (!accessToken || !path) return path;
  const url = new URL(path, window.location.href);
  if (url.origin !== window.location.origin) return path;
  url.searchParams.set("balerion_token", accessToken);
  return url.href;
}

async function api(path, options) {
  const response = await fetch(withAccessToken(path), options);
  if (response.status === 204) return null;
  const body = await response.json().catch(() => null);
  if (!response.ok) {
    throw new Error((body && body.error) || `${response.status} ${response.statusText}`);
  }
  return body;
}

const json = (payload) => ({
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify(payload),
});

/* ---- the piece map -----------------------------------------------------
 *
 * Runs arrive run-length encoded, alternating missing and present, always
 * starting with missing. Drawn to a canvas because a 40 GB torrent has tens of
 * thousands of pieces and that many elements would bring the page down. */

function drawMap(runs, total, headFraction) {
  const canvas = el.map;
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = 28;

  if (canvas.width !== Math.floor(width * ratio)) {
    canvas.width = Math.floor(width * ratio);
    canvas.height = Math.floor(height * ratio);
  }

  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);

  const styles = getComputedStyle(document.documentElement);
  const want = styles.getPropertyValue("--panel").trim() || "#eee";
  const have = styles.getPropertyValue("--accent").trim() || "#1e5aa8";
  const head = styles.getPropertyValue("--ink").trim() || "#000";

  ctx.fillStyle = want;
  ctx.fillRect(0, 0, width, height);

  if (total > 0) {
    ctx.fillStyle = have;
    let piece = 0;
    let present = false; // the first run is always the missing one
    for (const run of runs) {
      if (present && run > 0) {
        const x = (piece / total) * width;
        // Never round a present run down to nothing: a single verified piece
        // in a large torrent should still leave a visible mark.
        const w = Math.max((run / total) * width, 0.75);
        ctx.fillRect(x, 0, w, height);
      }
      piece += run;
      present = !present;
    }
  }

  if (headFraction !== null && headFraction >= 0) {
    ctx.fillStyle = head;
    ctx.fillRect(Math.min(headFraction * width, width - 2), 0, 2, height);
  }
}

/* ---- playback ---------------------------------------------------------- */

function bufferedAhead() {
  const video = el.video;
  if (!video.duration || !video.buffered.length) return 0;
  const at = video.currentTime;
  for (let i = 0; i < video.buffered.length; i += 1) {
    if (video.buffered.start(i) <= at && at <= video.buffered.end(i)) {
      return video.buffered.end(i) - at;
    }
  }
  return 0;
}

/* ---- the transcoding player --------------------------------------------
 *
 * For containers browsers cannot open, Balerion converts on the fly and hands
 * back fragmented MP4 in six second pieces. Media Source Extensions is the
 * only way to feed a growing stream to a <video> element while keeping the
 * scrubber working.
 *
 * Every segment ffmpeg produces starts at timestamp zero, so each one is
 * placed on the real timeline with `timestampOffset` before it is appended.
 * Doing the offset here rather than in ffmpeg avoids a nasty interaction
 * between its duration limit and a shifted output clock. */

let mse = null; // the transcoding session, when there is one

/** Seconds of buffered video to stay ahead of the playhead. */
const TARGET_BUFFER = 24;

/** Segments to gather before playback starts, so the opening does not stutter. */
const STARTUP_SEGMENTS = 3;

function once(target, event) {
  return new Promise((resolve) => target.addEventListener(event, resolve, { once: true }));
}

/** Wait for a SourceBuffer to go idle; appending while busy throws. */
function idle(buffer) {
  return buffer.updating ? once(buffer, "updateend") : Promise.resolve();
}

function teardown() {
  if (!mse) return;
  mse.stopped = true;
  try {
    if (mse.source && mse.source.readyState === "open") mse.source.endOfStream();
  } catch {
    // Already torn down by the browser. Nothing to do.
  }
  if (mse.url) URL.revokeObjectURL(mse.url);
  mse = null;
}

async function startTranscode(info) {
  teardown();

  if (!window.MediaSource || !MediaSource.isTypeSupported(info.mime)) {
    showViewerNote(
      `This file needs converting, and your browser will not accept the result (${info.mime}). ` +
        "Try Chrome, Firefox or Safari, or download it and use VLC."
    );
    return;
  }

  const source = new MediaSource();
  const url = URL.createObjectURL(source);
  const session = { source, url, info, next: 0, stopped: false, buffer: null, primed: false };
  mse = session;

  show(el.video, true);
  show(el.viewerNote, false);
  show(el.pending, false);
  el.video.src = url;

  await once(source, "sourceopen");
  if (session.stopped) return;

  try {
    source.duration = info.duration;
    const buffer = source.addSourceBuffer(info.mime);
    // Segments carry their own timing; we place them explicitly.
    buffer.mode = "segments";
    session.buffer = buffer;

    const init = await fetch(withAccessToken(info.init));
    if (!init.ok) throw new Error(await errorText(init));
    await idle(buffer);
    buffer.appendBuffer(await init.arrayBuffer());
    await once(buffer, "updateend");

    pump();
  } catch (err) {
    if (!session.stopped) showViewerNote(`Could not start playback: ${err.message}`);
  }
}

/** Keep the buffer ahead of the playhead, one segment at a time. */
async function pump() {
  const session = mse;
  if (!session || session.stopped || session.busy) return;
  const { buffer, info } = session;
  if (!buffer || buffer.updating) return;

  if (session.next >= info.segments) {
    // Everything has been appended, so tell the element the stream is whole.
    // Without this the scrubber never reaches the end.
    if (session.source.readyState === "open") {
      try {
        session.source.endOfStream();
      } catch {
        // Racing a teardown; harmless.
      }
    }
    return;
  }

  if (bufferedAhead() > TARGET_BUFFER) return;

  session.busy = true;
  const index = session.next;
  let waiting = false;
  try {
    const response = await fetch(withAccessToken(`${info.segment_prefix}${index}${info.segment_suffix ?? ""}`));

    // 503 means the bytes have not arrived yet, not that anything is wrong.
    // Giving up here is what turns a slow torrent into a dead player, so we
    // say so and come back for the same segment shortly.
    if (response.status === 503) {
      waiting = true;
      const body = await response.json().catch(() => null);
      setStatus((body && body.error) || "waiting for this part to download");
      return;
    }
    if (!response.ok) throw new Error(await errorText(response));

    const bytes = await response.arrayBuffer();
    if (session.stopped || session !== mse) return;

    await idle(buffer);
    buffer.timestampOffset = index * info.segment_seconds;
    await appendWithEviction(buffer, bytes);
    session.next = index + 1;
    setStatus("");

    // Hold off until there is a runway. Starting the instant segment 0 lands
    // means the first hiccup is a visible stall; a few segments of headroom
    // absorbs ordinary jitter and costs a couple of seconds.
    if (!session.primed && (session.next >= STARTUP_SEGMENTS || session.next >= info.segments)) {
      session.primed = true;
      el.video.play().catch(() => {});
    }
  } catch (err) {
    if (!session.stopped) {
      showViewerNote(`Playback stopped at segment ${index}: ${err.message}`);
      session.stopped = true;
    }
  } finally {
    session.busy = false;
  }

  if (mse !== session || session.stopped) return;
  // Back off when waiting on the swarm; otherwise keep the buffer topped up.
  setTimeout(pump, waiting ? 2000 : 0);
}

/* ---- can this actually be watched? -------------------------------------
 *
 * Only one comparison decides it: the rate the swarm supplies against the rate
 * playback consumes. A percentage-downloaded threshold tells you nothing,
 * because 20% of a 25 MB cartoon and 20% of an 8 GB film are entirely
 * different propositions and neither predicts a stall.
 *
 * If the swarm is slower than the bitrate, no amount of waiting helps unless
 * you wait for the whole shortfall, which is what the arithmetic below works
 * out and says out loud. */

/* The rate has to be averaged over a decent window before it can be used for
 * this. The live figure is smoothed over about a second, which is right for a
 * speedometer and hopeless for a prediction: the formula divides by the rate,
 * so a brief dip sends the estimate to hours, and a brief spike above the
 * bitrate makes the whole notice vanish and come back. */

const RATE_WINDOW_MS = 30_000;
const MIN_SAMPLE_MS = 12_000;
let rateSamples = [];
let lastVerdictOk = null; // for hysteresis, so the verdict cannot flap

function recordRate(stats) {
  const now = performance.now();
  rateSamples.push({ at: now, bytes: stats.bytes_on_disk });
  rateSamples = rateSamples.filter((sample) => now - sample.at <= RATE_WINDOW_MS);
}

function forgetRates() {
  rateSamples = [];
  lastVerdictOk = null;
}

/** Bytes per second averaged over the window, or null if too soon to say. */
function averageRate() {
  if (rateSamples.length < 2) return null;
  const first = rateSamples[0];
  const last = rateSamples[rateSamples.length - 1];
  const span = last.at - first.at;
  if (span < MIN_SAMPLE_MS) return null;
  return Math.max(0, (last.bytes - first.bytes) / (span / 1000));
}

function feasibility() {
  if (!playInfo || !lastStats || playing === null) return null;
  if (lastStats.complete) return { ok: true };

  const length = playInfo.length;
  // Transcoding knows the duration from the probe; native playback learns it
  // from the browser once metadata has loaded.
  const duration = playInfo.duration || el.video.duration;
  if (!length || !duration || !isFinite(duration) || duration <= 0) return null;

  const rate = averageRate();
  if (rate === null) return null; // still measuring; say nothing rather than guess

  const bitrate = length / duration;
  // A dead band around the bitrate, so a swarm hovering near the threshold
  // does not switch the notice on and off every second.
  let ok = lastVerdictOk;
  if (rate >= bitrate * 1.05) ok = true;
  else if (rate <= bitrate * 0.95) ok = false;
  if (ok === null) ok = rate >= bitrate;
  lastVerdictOk = ok;
  if (ok) return { ok: true };

  // Start now and you run out at some point. To watch through uninterrupted
  // the download must finish exactly as playback does, which needs this head
  // start. Note it also equals the time the stalls would have cost anyway.
  const wait = rate > 0 ? (duration * (bitrate - rate)) / rate : Infinity;
  const total = rate > 0 ? length / rate : Infinity;
  return { ok: false, bitrate, rate, wait, total, hopeless: rate < bitrate * 0.1 };
}

/** Rounded so a jittering estimate does not twitch every second. */
function showAdvisory() {
  const verdict = feasibility();
  if (!verdict || verdict.ok) {
    show(el.advisory, false);
    return;
  }

  el.advisory.replaceChildren();
  const strong = document.createElement("strong");
  const rest = document.createElement("span");

  const supply = `${bytes(Math.round(verdict.rate))}/s`;
  const needed = `${bytes(Math.round(verdict.bitrate))}/s`;

  if (verdict.hopeless) {
    strong.textContent = "This cannot be watched live. ";
    rest.textContent =
      `The swarm is supplying ${supply} and playback needs ${needed}. Leave it ` +
      "downloading and watch when it has finished, or tick Keep offline so it survives.";
  } else {
    strong.textContent = "This will stall. ";
    // Waiting and stalling cost the same total time, because nothing can be
    // watched faster than it arrives. Saying so stops the wait reading as a
    // penalty when it is really a choice about when the pauses happen.
    rest.textContent =
      `The swarm supplies ${supply} against the ${needed} playback needs, so it ` +
      `will pause now and then. Either way it takes ${roughly(verdict.total)} to ` +
      `get through: start now and accept the interruptions, or wait ` +
      `${roughly(verdict.wait)} for a head start and then run clean. More peers ` +
      "would improve both.";
  }

  const text = strong.textContent + rest.textContent;
  if (el.advisory.dataset.text !== text) {
    el.advisory.dataset.text = text;
    el.advisory.replaceChildren(strong, rest);
  }
  show(el.advisory, true);
}

/** A line under the piece map, for when playback is waiting on the download. */
function setStatus(message) {
  if (!el.mapStatus) return;
  el.mapStatus.dataset.waiting = message ? "yes" : "";
  if (message) el.mapStatus.textContent = message;
}

/** Append, making room by dropping what is well behind the playhead. */
async function appendWithEviction(buffer, bytes) {
  try {
    buffer.appendBuffer(bytes);
    await once(buffer, "updateend");
  } catch (err) {
    if (err.name !== "QuotaExceededError") throw err;

    // The buffer is full. Everything more than a minute behind the playhead
    // has been watched and can go. This is the failure every MSE player hits
    // first, and it presents as playback simply stopping.
    const cutoff = Math.max(0, el.video.currentTime - 60);
    if (cutoff > 0) {
      await idle(buffer);
      buffer.remove(0, cutoff);
      await once(buffer, "updateend");
    }
    await idle(buffer);
    buffer.appendBuffer(bytes);
    await once(buffer, "updateend");
  }
}

/** A seek may land outside what has been appended, so start again there. */
async function reseek() {
  const session = mse;
  if (!session || session.stopped || !session.buffer) return;

  const target = el.video.currentTime;
  // Already buffered? Then the element will handle it by itself.
  for (let i = 0; i < el.video.buffered.length; i += 1) {
    if (el.video.buffered.start(i) <= target && target < el.video.buffered.end(i)) return;
  }

  const wanted = Math.floor(target / session.info.segment_seconds);
  if (wanted === session.next) return;

  session.next = wanted;
  try {
    await idle(session.buffer);
    // Drop everything: what is buffered is for a part of the film the viewer
    // has just left, and keeping it only invites a quota failure later.
    if (session.source.readyState === "open" && session.source.duration > 0) {
      session.buffer.remove(0, session.source.duration);
      await once(session.buffer, "updateend");
    }
  } catch {
    // A failed eviction is survivable; the append may still succeed.
  }
  pump();
}

async function errorText(response) {
  const body = await response.json().catch(() => null);
  return (body && body.error) || `${response.status} ${response.statusText}`;
}

function showViewerNote(message, download) {
  show(el.video, false);
  show(el.viewerNote, true);
  el.viewerNote.replaceChildren();
  const text = document.createElement("span");
  text.textContent = message;
  el.viewerNote.append(text);
  if (download) {
    const link = document.createElement("a");
    link.href = download;
    link.textContent = "Download this file";
    link.style.display = "block";
    link.style.marginTop = "0.75rem";
    link.style.color = "inherit";
    el.viewerNote.append(link);
  }
}

/** Offer any subtitle tracks the server found. */
function attachTracks(tracks) {
  for (const old of [...el.video.querySelectorAll("track")]) old.remove();
  for (const [index, track] of (tracks || []).entries()) {
    const node = document.createElement("track");
    node.kind = "subtitles";
    node.label = track.label;
    if (track.language) node.srclang = track.language;
    node.src = withAccessToken(track.url);
    if (index === 0) node.default = true;
    el.video.append(node);
  }
}

/** Play a file, whatever it turns out to be. */
async function play(index) {
  const file = current.files[index];
  if (!file) return;

  teardown();
  // A different file has different tracks, so a choice made about the last one
  // means nothing here.
  if (playing !== index) chosenAudio = 0;
  playing = index;
  describeToTheSystem(file);
  playInfo = null;
  forgetRates();
  show(el.advisory, false);
  // The loading panel belongs to resolving, not to playing. Anything that
  // puts a picture on the screen must clear it.
  show(el.pending, false);
  renderFiles();
  teardown();
  show(el.video, false);
  show(el.viewerNote, true);
  const doneWaiting = sayWhileWaiting(el.viewerNote, [
    { text: "Working out how to play this" },
    {
      after: 6_000,
      text:
        "Reading the start of the file to see what is in it. Those bytes have to " +
        "come off the swarm first, so with few peers this is the slow part.",
    },
    {
      after: 45_000,
      text:
        "Still waiting for the opening of the file. The piece map below fills in " +
        "as it arrives; nothing can be said about the video until it does.",
    },
  ], () => playing === index);

  let info;
  try {
    info = await api(`/api/play/${current.infohash}/${index}${audioQuery()}`);
  } catch (err) {
    doneWaiting();
    showViewerNote(err.message);
    return;
  }
  doneWaiting();
  if (playing !== index) return; // the viewer moved on while we asked

  playInfo = info;
  attachTracks(info.tracks);
  showAudioTracks(info.audio_tracks);
  showCastUrl();

  /* Subtitles being made from the audio take minutes, so the page has to be
   * able to say "not yet" rather than leaving a viewer to conclude there are
   * none. Asked again on a timer, since nothing pushes. */
  if (info.subtitles_pending) {
    setStatus("Transcribing the audio for subtitles. This takes a few minutes.");
    window.setTimeout(() => {
      if (playing === index) refreshTracks(index);
    }, 30_000);
  }

  if (info.mode === "unsupported") {
    showViewerNote(info.reason, info.download);
    return;
  }

  /* Not yet, rather than never. Ask again, showing the piece count so the wait
   * is visibly a wait rather than a page that has stopped.
   *
   * Guarded on `playing` so that changing file cancels it: without that, a
   * retry armed for a file you have moved on from lands in three seconds and
   * takes the player back. */
  if (info.mode === "notready") {
    const done = Math.round((info.pieces_have / Math.max(info.pieces_total, 1)) * 100);
    showViewerNote(`${info.reason}. ${info.pieces_have} of ${info.pieces_total} pieces (${done}%).`);
    window.setTimeout(() => {
      if (playing === index) play(index);
    }, 3000);
    return;
  }

  if (info.mode === "direct") {
    show(el.video, true);
    show(el.viewerNote, false);
    show(el.pending, false);
    el.video.src = reachableUrl(info.url);
    el.video.load();
    resumeIfAsked(info);
    handedToATelevision();
    el.video.play().catch(() => {
      // Autoplay refused. The controls are right there.
    });
    return;
  }

  /* Safari plays HLS itself, and doing it that way is what makes AirPlay work:
   * the television is handed a URL and fetches it, which it cannot do with a
   * MediaSource blob. Everywhere else, feed MediaSource as before. */
  if (el.video.canPlayType("application/vnd.apple.mpegurl") && info.playlist) {
    show(el.video, true);
    show(el.viewerNote, false);
    show(el.pending, false);
    el.video.src = reachableUrl(info.playlist);
    el.video.load();
    resumeIfAsked(info);
    handedToATelevision();
    el.video.play().catch(() => {
      // Autoplay refused. The controls are right there.
    });
    return;
  }

  await startTranscode(info);
  resumeIfAsked(info);
}

/* ---- what the phone shows when the screen is off -------------------------
 *
 * The Media Session API is the difference between "a web page that happens to
 * be playing audio" and something that behaves like a television. It puts the
 * title on the lock screen, in the notification shade and on a watch, and it
 * routes the hardware buttons (headphone pause, car stereo skip) at the page
 * instead of at nothing.
 *
 * Cheap, standard, and supported by every phone browser worth the name. The
 * only reason it is not everywhere is that it is easy not to know about. */

/** A programme and an episode, out of the sort of name a release has. */
function nowPlaying(file) {
  if (!file) return { title: "Balerion", subtitle: "" };
  const tag =
    file.season !== null && file.season !== undefined && file.episode !== null
      ? `S${String(file.season).padStart(2, "0")}E${String(file.episode).padStart(2, "0")}`
      : "";
  /* The file's own name, tidied: scene releases are dot-separated and carry a
     tail of tags nobody wants read out on a lock screen. */
  const bare = (file.name || "").replace(/\.[a-z0-9]{2,4}$/i, "").replace(/[._]+/g, " ");
  const cut = bare.search(/\b(?:\d{3,4}p|WEB[- ]?DL|WEBRip|BluRay|HDTV|x26[45]|HEVC|DDP?5)\b/i);
  const title = (cut > 0 ? bare.slice(0, cut) : bare).trim() || file.name || "Balerion";
  return { title, subtitle: tag };
}

/** Tell the phone what is playing, and which buttons should do what. */
function describeToTheSystem(file) {
  if (!("mediaSession" in navigator)) return;
  const { title, subtitle } = nowPlaying(file);
  try {
    navigator.mediaSession.metadata = new window.MediaMetadata({
      title,
      artist: subtitle,
      album: current ? current.name : "",
    });
  } catch {
    // An older browser with the object but not the constructor. The handlers
    // below are the useful half anyway.
  }

  const jump = (by) => {
    if (!Number.isFinite(el.video.duration)) return;
    el.video.currentTime = Math.min(
      Math.max(el.video.currentTime + by, 0),
      el.video.duration,
    );
  };
  const handlers = {
    play: () => el.video.play().catch(() => {}),
    pause: () => el.video.pause(),
    // Ten and thirty, which is what every television remote does.
    seekbackward: (details) => jump(-(details?.seekOffset || 10)),
    seekforward: (details) => jump(details?.seekOffset || 30),
    seekto: (details) => {
      if (details && Number.isFinite(details.seekTime)) el.video.currentTime = details.seekTime;
    },
    nexttrack: () => {
      const ordered = playableInOrder();
      const at = ordered.findIndex((one) => one.index === playing);
      const next = at === -1 ? null : ordered[at + 1];
      if (next) play(next.index);
    },
    previoustrack: () => {
      const ordered = playableInOrder();
      const at = ordered.findIndex((one) => one.index === playing);
      const before = at > 0 ? ordered[at - 1] : null;
      if (before) play(before.index);
    },
  };
  for (const [action, handler] of Object.entries(handlers)) {
    try {
      navigator.mediaSession.setActionHandler(action, handler);
    } catch {
      // Not every browser knows every action, and refusing one must not stop
      // the rest being registered.
    }
  }
}

/** Keep the lock screen's scrubber honest. */
function tellTheSystemWhereWeAre() {
  if (!("mediaSession" in navigator) || !navigator.mediaSession.setPositionState) return;
  const duration = el.video.duration;
  if (!Number.isFinite(duration) || duration <= 0) return;
  try {
    navigator.mediaSession.setPositionState({
      duration,
      playbackRate: el.video.playbackRate || 1,
      position: Math.min(Math.max(el.video.currentTime, 0), duration),
    });
  } catch {
    // Throws if position exceeds duration, which happens for a frame at the
    // very end. Not worth a console entry every time.
  }
}

/* ---- handing it to a television -----------------------------------------
 *
 * AirPlay does not mirror the page. Choose a receiver and the Apple TV is
 * handed the video element's URL and fetches it itself, which means the URL
 * has to be one the television can reach. This page is normally reached over
 * a tunnel, and a television is not on that tunnel, so the address that works
 * for the phone is exactly the one that does not work for the receiver.
 *
 * So the swap happens in the same tap that opens the receiver picker.  The
 * receiver captures its source while that picker is opening, not after it has
 * announced itself wireless. Coming back off AirPlay reverses it. */
let beforeAirplay = null;

function televisionUrl() {
  if (!castBase || !playInfo) return null;
  const path = playInfo.mode === "transcode" ? playInfo.playlist : playInfo.url;
  if (!path || path.startsWith("http")) return null;
  return `${castBase}${path}`;
}

function replaceSourceAt(src, at) {
  let resumed = false;
  const resume = () => {
    if (resumed) return;
    resumed = true;
    if (Number.isFinite(at) && at > 0) {
      try { el.video.currentTime = at; } catch {}
    }
    el.video.play().catch(() => {});
  };
  el.video.addEventListener("loadedmetadata", resume, { once: true });
  el.video.src = src;
  el.video.load();
  if (el.video.readyState >= HTMLMediaElement.HAVE_METADATA) resume();
}

function handedToATelevision() {
  const wireless = el.video.webkitCurrentPlaybackTargetIsWireless;
  if (wireless && televisionUrl()) {
    const url = televisionUrl();
    if (!beforeAirplay) {
      beforeAirplay = { src: el.video.src, at: el.video.currentTime };
      replaceSourceAt(url, beforeAirplay.at);
    }
    setStatus("Playing on the television. It is fetching the film itself, not from this phone.");
    if (!el.remote.hidden) el.remoteStatus.textContent = "Playing on your TV. This phone is now the remote.";
    return;
  }
  if (!wireless && beforeAirplay) {
    const { src, at } = beforeAirplay;
    beforeAirplay = null;
    const resumeAt = el.video.currentTime || at;
    replaceSourceAt(src, resumeAt);
    setStatus("");
    if (!el.remote.hidden) el.remoteStatus.textContent = "Ready to play on your TV.";
  }
}

/* ---- one episode after another ------------------------------------------
 *
 * A season pack is one torrent and twelve programmes, and having to go back to
 * the file list between each of them is the sort of thing that makes an
 * evening feel like operating software. The next one is the next *episode*,
 * not the next entry in the torrent: packs are frequently listed in whatever
 * order they were added.
 *
 * Cancellable, and cancelled by anything that suggests the viewer is still
 * there. Nobody has ever wanted a television to start the next thing while
 * they were reaching for the remote. */
const NEXT_EPISODE_DELAY_MS = 12_000;
let nextEpisodeTimer = null;

function playableInOrder() {
  if (!current) return [];
  return [...current.files]
    .filter((file) => file.kind === "video")
    .sort((a, b) => {
      const ae = a.season !== null && a.episode !== null;
      const be = b.season !== null && b.episode !== null;
      if (ae && be) return a.season - b.season || a.episode - b.episode;
      if (ae) return -1;
      if (be) return 1;
      return a.index - b.index;
    });
}

function cancelNextEpisode() {
  if (nextEpisodeTimer === null) return;
  window.clearTimeout(nextEpisodeTimer);
  nextEpisodeTimer = null;
  setStatus("");
}

function offerNextEpisode() {
  cancelNextEpisode();
  const ordered = playableInOrder();
  const at = ordered.findIndex((file) => file.index === playing);
  const next = at === -1 ? null : ordered[at + 1];
  if (!next) return;

  const label =
    next.season !== null && next.episode !== null
      ? `S${String(next.season).padStart(2, "0")}E${String(next.episode).padStart(2, "0")}`
      : next.name;
  setStatus(`Next: ${label}. Starting in ${NEXT_EPISODE_DELAY_MS / 1000} seconds; press anything to stop.`);

  nextEpisodeTimer = window.setTimeout(() => {
    nextEpisodeTimer = null;
    play(next.index);
  }, NEXT_EPISODE_DELAY_MS);
}

el.video.addEventListener("ended", () => offerNextEpisode());
// Any sign of a viewer still being there calls it off.
for (const event of ["play", "seeking", "keydown", "pointerdown"]) {
  (event === "keydown" || event === "pointerdown" ? document : el.video).addEventListener(
    event,
    () => cancelNextEpisode()
  );
}

/* ---- picking up where you left off -------------------------------------
 *
 * The server remembers a position per file. Seeking has to wait for metadata:
 * setting currentTime on an element that does not yet know its own duration is
 * silently ignored, which looks exactly like the feature not working. */
function resumeIfAsked(info) {
  const at = Number(info.resume_at);
  if (!Number.isFinite(at) || at <= 0) return;

  const seek = () => {
    // Guarded, because the viewer may have moved on or started scrubbing
    // themselves in the moment before metadata arrived.
    if (el.video.currentTime < 1) el.video.currentTime = at;
    setStatus(`Picking up at ${seconds(at)}.`);
  };
  if (el.video.readyState >= 1) seek();
  else el.video.addEventListener("loadedmetadata", seek, { once: true });
}

/* Tell the server where we are, every so often.
 *
 * Every ten seconds rather than on every timeupdate, which fires four times a
 * second and would be four hundred requests for a feature film. The server
 * batches these to disk anyway, so a finer report would buy nothing. */
const PROGRESS_EVERY_MS = 10_000;
let lastProgressAt = 0;

function reportProgress(force) {
  if (!current || playing === null) return;
  const now = Date.now();
  if (!force && now - lastProgressAt < PROGRESS_EVERY_MS) return;

  const seconds = el.video.currentTime;
  const duration = el.video.duration;
  if (!Number.isFinite(seconds) || !Number.isFinite(duration) || duration <= 0) return;
  lastProgressAt = now;

  // Deliberately unawaited and deliberately silent: losing one of these costs
  // ten seconds of accuracy and is not worth interrupting playback over.
  api(`/api/progress/${current.infohash}/${playing}`, json({ seconds, duration })).catch(
    () => {}
  );
}

el.video.addEventListener("timeupdate", () => reportProgress(false));
el.video.addEventListener("pause", () => reportProgress(true));
el.video.addEventListener("ended", () => reportProgress(true));
// The last position of a session is the one most worth keeping, and a closing
// tab gets no second chance to send it.
window.addEventListener("pagehide", () => reportProgress(true));

/* ---- audio tracks -------------------------------------------------------
 *
 * A film with a commentary or a second language has more than one, and taking
 * whichever the muxer put first is how somebody ends up listening to a director
 * talk over the picture with no way to stop it. Only shown when there is a
 * genuine choice: a menu of one is a menu that should not be there. */
function showAudioTracks(tracks) {
  el.audioTracks.replaceChildren();
  const choices = Array.isArray(tracks) ? tracks : [];
  show(el.audioRow, choices.length > 1);
  if (choices.length < 2) return;

  for (const track of choices) {
    const option = document.createElement("option");
    option.value = String(track.index);
    option.textContent = track.channels
      ? `${track.label} \u2014 ${track.channels}ch`
      : track.label;
    el.audioTracks.append(option);
  }
  el.audioTracks.value = String(playInfo?.audio ?? choices[0].index);
}

/* Switching track means starting the file again: the declared codec string
 * changes with the audio, so the MediaSource has to be rebuilt from its init
 * segment. The position is kept, because losing your place to change language
 * would be a poor trade. */
el.audioTracks.addEventListener("change", async () => {
  if (playing === null) return;
  const at = el.video.currentTime;
  chosenAudio = Number(el.audioTracks.value) || 0;
  await play(playing);
  if (Number.isFinite(at) && at > 0) {
    const seek = () => {
      el.video.currentTime = at;
    };
    if (el.video.readyState >= 1) seek();
    else el.video.addEventListener("loadedmetadata", seek, { once: true });
  }
});

/* Re-ask about a file without disturbing what is playing.
 *
 * Used while subtitles are being transcribed: the track appears when the job
 * finishes, and nothing else about the file has changed. */
async function refreshTracks(index) {
  try {
    const info = await api(`/api/play/${current.infohash}/${index}${audioQuery()}`);
    if (playing !== index) return;
    attachTracks(info.tracks);
    if (info.subtitles_pending) {
      window.setTimeout(() => {
        if (playing === index) refreshTracks(index);
      }, 30_000);
    } else if (info.tracks?.length) {
      setStatus("Subtitles are ready.");
    }
  } catch {
    // The file is still playing; a failed poll for subtitles is not worth
    // saying anything about.
  }
}

/* A container the browser opens can still hold a codec it will not decode,
 * and there is no way to know without parsing the file. So we try, and say
 * something honest when it fails rather than spinning forever. */
el.video.addEventListener("error", () => {
  if (playing === null || !current) return;
  // A transcoding session reports its own failures with more detail.
  if (mse && !mse.stopped) return;
  showViewerNote(
    "Your browser will not decode this file. The container is one it opens, so " +
      "the codec inside is probably the trouble. It has still downloaded, and " +
      "VLC will play it.",
    `/stream/${current.infohash}/${playing}?download=true`
  );
});

// Keep the transcoding buffer topped up, and follow the viewer when they seek.
el.video.addEventListener("timeupdate", () => pump());
el.video.addEventListener("waiting", () => pump());
el.video.addEventListener("seeking", () => reseek());

/* ---- rendering --------------------------------------------------------- */

/* Where a television should be pointed for the file being played.
 *
 * The player's own address is loopback and means nothing to another device, so
 * the server is asked for one the network can reach. Transcoded files get the
 * playlist; anything a television could already open gets the file itself. */
let castBase = null;

/* Where the media should be fetched from, so that a television can fetch it too.
 *
 * AirPlay does not mirror a video element, it hands the receiver the URL and
 * lets it fetch the media itself. An Apple TV given `http://127.0.0.1:8080/...`
 * reaches nothing, so the button appeared to do nothing and the whole feature
 * was a URL to copy by hand. With `--cast-port` on there is a LAN address
 * serving exactly the same bytes, so playback was pointed at that instead.
 *
 * That rested on an assumption which used to be true and is not any more: that
 * the browser and the player are on the same machine, so the player's LAN
 * address is *this* machine's LAN address and certainly reachable. Move the
 * player to a box that stays on and watch from a phone over a tunnel, and the
 * LAN address means nothing to the phone. The video element is handed
 * `http://192.168.0.13:8081/...`, fetches nothing, and sits at 0:00 with no
 * error, forever. Which is exactly what it did.
 *
 * So the substitution is made only when the page itself came from loopback,
 * which is the one case where "its own machine's LAN address" is a true
 * description. Anywhere else the media is fetched from the origin that served
 * the page, because that is the one address the viewer is known to be able to
 * reach: they are looking at it.
 *
 * Casting is unaffected. The URL offered to a television is built separately in
 * `showCastUrl`, from `castBase`, and always was. */
function reachableUrl(path) {
  return withAccessToken(mediaUrl(path, castBase, window.location.hostname));
}

async function loadCastBase() {
  try {
    const info = await api("/api/cast");
    castBase = info.base;
  } catch {
    castBase = null;
  }
}

function showCastUrl() {
  if (!current || playing === null || !playInfo) {
    show(el.remote, false);
    return;
  }
  show(el.remote, true);
  if (!canPickAirplay()) {
    el.remoteStatus.textContent = castBase
      ? "Playing on this phone. Open Balerion in Safari to send it to your TV."
      : "Playing on this phone. TV playback is not enabled on this Balerion.";
    el.airplay.hidden = true;
    return;
  }
  el.airplay.hidden = false;
  el.remoteStatus.textContent = "Ready to play on your TV.";
}

function openAirplayPicker() {
  if (!canPickAirplay()) return false;
  const picker = el.video.webkitShowPlaybackTargetPicker;
  if (typeof picker !== "function") {
    el.remoteStatus.textContent = "Use the iPhone's AirPlay control in the player to choose your TV.";
    return false;
  }
  try {
    // Give the receiver a LAN URL before it considers the target. Waiting for
    // WebKit's wireless event is too late in Firefox: the LG gets the old
    // source and never contacts the media-only listener.
    const url = televisionUrl();
    if (url && !beforeAirplay) {
      beforeAirplay = { src: el.video.src, at: el.video.currentTime };
      replaceSourceAt(url, beforeAirplay.at);
    }
    picker.call(el.video);
    el.remoteStatus.textContent = "Choose your TV in the AirPlay picker.";
    return true;
  } catch {
    // WebKit may reject a picker that has outlived the tap which requested it.
    // The visible button gives the viewer a clean second attempt, not a URL.
    el.remoteStatus.textContent = "Tap Watch on TV to choose your TV.";
    return false;
  }
}

el.airplay.addEventListener("click", () => openAirplayPicker());

function remoteJump(seconds) {
  if (!Number.isFinite(el.video.duration)) return;
  el.video.currentTime = Math.min(Math.max(el.video.currentTime + seconds, 0), el.video.duration);
}

el.remoteBack.addEventListener("click", () => remoteJump(-10));
el.remoteForward.addEventListener("click", () => remoteJump(30));
el.remoteToggle.addEventListener("click", () => {
  if (el.video.paused) el.video.play().catch(() => {});
  else el.video.pause();
});
el.remoteShelf.addEventListener("click", () => {
  show(el.torrent, false);
  setMode("downloaded");
  window.scrollTo({ top: 0, behavior: "smooth" });
});
el.video.addEventListener("play", () => { el.remoteToggle.textContent = "Pause"; });
el.video.addEventListener("pause", () => { el.remoteToggle.textContent = "Play"; });

async function watchDownloaded(item) {
  // This is already held locally. Do not hand its bare infohash back to the
  // magnet resolver, because that path is allowed to wait for swarm metadata.
  // A downloaded episode opens from Balerion's own library, immediately.
  show(el.failure, false);
  show(el.torrent, false);
  show(el.pending, true);
  el.pendingNote.textContent = "Opening downloaded episode";
  try {
    const info = await api(`/api/torrents/${item.infohash}/open`);
    renderTorrent(info);
    refresh();
  } catch (err) {
    show(el.failure, true);
    el.failureBody.textContent = err.message;
  } finally {
    show(el.pending, false);
  }
}

/** `S01E03`, when the filename said so. */
function episodeLabel(file) {
  if (file.season === null || file.episode === null) return null;
  const pad = (n) => String(n).padStart(2, "0");
  return `S${pad(file.season)}E${pad(file.episode)}`;
}

function renderFiles() {
  el.files.replaceChildren();

  /* Episodes in order, everything else after them in the order the torrent
   * lists it. A season pack's files are frequently not in episode order, and
   * "the next one" should be the next one. */
  const ordered = [...current.files].sort((a, b) => {
    const ae = episodeLabel(a);
    const be = episodeLabel(b);
    if (ae && be) return a.season - b.season || a.episode - b.episode;
    if (ae) return -1;
    if (be) return 1;
    return a.index - b.index;
  });

  for (const file of ordered) {
    const row = document.createElement("li");

    const name = document.createElement("div");
    name.className = "file-name";
    const title = document.createElement("span");
    const label = episodeLabel(file);
    /* The episode number in front, because in a pack it is the only part of the
     * name that differs and it is buried in the middle of it. */
    title.textContent = label ? `${label}  ${file.name}` : file.name;
    if (file.index === playing) {
      title.className = "playing";
      title.textContent = `${file.name} (playing)`;
    }
    name.append(title);
    if (file.reason) {
      const why = document.createElement("span");
      why.className = "file-why";
      why.textContent = file.reason;
      name.append(why);
    }

    const size = document.createElement("span");
    size.className = "file-size";
    size.textContent = bytes(file.length);

    const action = document.createElement("span");
    // Anything that is video or audio gets a Play button now, even in a
    // container browsers cannot open: the server decides whether it can be
    // converted, and says so plainly if it cannot.
    if (file.kind === "video" || file.kind === "audio") {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "button-small";
      button.textContent = file.index === playing ? "Restart" : "Play";
      button.addEventListener("click", () => play(file.index));
      action.append(button);
    } else {
      const link = document.createElement("a");
      link.className = "file-size";
      link.href = `/stream/${current.infohash}/${file.index}?download=true`;
      link.textContent = "Download";
      action.append(link);
    }

    row.append(name, size, action);
    el.files.append(row);
  }
}

function renderTorrent(info) {
  current = info;
  playing = null;

  el.name.textContent = info.name;
  el.meta.textContent = [
    bytes(info.total_length),
    `${info.files.length} file${info.files.length === 1 ? "" : "s"}`,
    `${info.piece_count} pieces of ${bytes(info.piece_length)}`,
    info.webseeds ? `${info.webseeds} webseeds` : null,
    info.infohash.slice(0, 12),
  ]
    .filter(Boolean)
    .join("  /  ");

  el.keep.checked = info.kept;
  show(el.details, false);
  el.detailsToggle.setAttribute("aria-expanded", "false");
  el.detailsToggle.textContent = "Download details";
  show(el.torrent, true);
  show(el.pending, false);
  show(el.failure, false);

  if (info.suggested !== null && info.suggested !== undefined) {
    play(info.suggested);
  } else {
    show(el.video, false);
    show(el.viewerNote, true);
    el.viewerNote.textContent =
      "Nothing in this torrent is a video or audio file a browser can open. " +
      "The files are listed below and can still be downloaded.";
    renderFiles();
  }
}

el.detailsToggle.addEventListener("click", () => {
  const opening = el.details.hidden;
  show(el.details, opening);
  el.detailsToggle.setAttribute("aria-expanded", opening ? "true" : "false");
  el.detailsToggle.textContent = opening ? "Hide details" : "Download details";
});

function renderStats(stats) {
  lastStats = stats;
  recordRate(stats);
  showAdvisory();
  el.peers.textContent = stats.peers;
  el.rate.textContent = `${bytes(Math.round(stats.rate))}/s`;
  el.pieces.textContent = `${stats.pieces_have} / ${stats.pieces_total}`;
  // What is held, not what arrived this session. A resumed torrent has fetched
  // almost nothing while holding nearly all of the file, and showing the
  // latter as progress reads as "100% of 588 MiB, but only 9.8 MiB".
  el.disk.textContent = bytes(stats.bytes_on_disk);

  const ahead = bufferedAhead();
  el.buffer.textContent = seconds(ahead);

  const headFraction =
    el.video.duration && isFinite(el.video.duration)
      ? el.video.currentTime / el.video.duration
      : null;
  drawMap(stats.runs, stats.pieces_total, headFraction);

  // The player owns this line while it is waiting on the swarm, since "42% on
  // disk" is a good deal less useful than saying why nothing is happening.
  if (el.mapStatus.dataset.waiting === "yes") return;

  const percent = stats.pieces_total
    ? Math.floor((stats.pieces_have / stats.pieces_total) * 100)
    : 0;
  if (stats.complete) {
    el.mapStatus.textContent = "complete";
  } else if (stats.peers === 0 && stats.rate < 1) {
    el.mapStatus.textContent = `${percent}% on disk, waiting for a source`;
  } else {
    el.mapStatus.textContent = `${percent}% on disk`;
  }
}

/** `S01E02` out of a torrent name, the same shapes the server reads. */
/* Three copies of the same episode do not download three times faster. They
 * split the peers you have between them, and each one is slower than one would
 * have been. That is easy to do by accident, invisible while it happens, and
 * exactly what makes an evening of this feel broken. */
function warnAboutDuplicates(all) {
  const groups = new Map();
  for (const item of all) {
    // By programme as well as by episode. Grouping on the tag alone made
    // Better Call Saul S01E01 and Breaking Bad S01E01 into "2 copies of
    // S01E01", and then offered to delete one of them.
    const which = seriesOf(item.name);
    if (!which) continue;
    groups.set(which.key, {
      label: which.series ? `${which.series} ${which.tag}` : which.tag,
      items: [...(groups.get(which.key)?.items ?? []), item],
    });
  }
  const duplicated = [...groups.values()].filter(({ items }) => items.length > 1);
  if (duplicated.length === 0) {
    show(el.duplicates, false);
    return;
  }

  const said = duplicated
    .map(({ label, items }) => `${items.length} copies of ${label}`)
    .join(", ");
  const peers = all.reduce((total, item) => total + item.peers, 0);
  el.duplicates.textContent =
    `${said} are running at once. They are not downloading in parallel: they are ` +
    `sharing the ${peers} peers between them, so each is slower than one would be. ` +
    `Delete the ones you are not watching.`;
  show(el.duplicates, true);
}

/* One row of a torrent list.
 *
 * Shared by "On this machine", which shows everything the server is holding,
 * and by the Downloaded shelf, which shows only the kept ones. Written once
 * because two copies of "what a torrent looks like in a list" is two things to
 * remember to change. `withBadge` is the only difference worth having: on the
 * shelf every row is kept, and a column that reads "kept" all the way down
 * says nothing. */
function libraryRow(item, { withBadge = true } = {}) {
  const row = document.createElement("li");

  const name = document.createElement("span");
  name.textContent = item.name;

  /* The one line somebody reads to decide whether to put the kettle on. */
  const state = stateOf(item);
  const size = document.createElement("span");
  size.className = `library-size state-${state.stage}`;
  size.textContent = state.stage === "ready" ? bytes(item.total_length) : state.label;
  if (state.stage === "preparing") {
    size.title = "Converting it so it starts at once, seeks to the frame, and plays on a television";
  }

  // A bar under the name, because a number is read and a bar is glanced at.
  const bar = document.createElement("span");
  bar.className = "library-bar";
  bar.dataset.stage = state.stage;
  const filled = document.createElement("span");
  filled.style.width = `${Math.round(state.fraction * 100)}%`;
  bar.append(filled);
  show(bar, state.stage === "downloading" || state.stage === "preparing");

  // Reopen something already on disk. Resolving by infohash finds the
  // running session rather than starting a second one, so a part-finished
  // torrent carries on downloading from where it stopped.
  const open = document.createElement("button");
  open.type = "button";

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "button-quiet";
  remove.textContent = "Delete";
  remove.addEventListener("click", async () => {
    if (!window.confirm(`Delete ${item.name} and its files from disk?`)) return;
    await api(`/api/torrents/${item.infohash}`, { method: "DELETE" });
    if (current && current.infohash === item.infohash) {
      show(el.torrent, false);
      teardown();
      el.video.removeAttribute("src");
      el.video.load();
      current = null;
      playing = null;
    }
    refresh();
  });

  /* Download means download. A half-finished episode does not offer a tempting
     second path which turns out to be a stuttering stream. Once it is ready,
     Watch takes the viewer straight to the television picker. */
  if (item.complete && (state.stage === "ready" || state.stage === "playable")) {
    open.textContent = "Watch";
    open.className = "button-small";
    open.addEventListener("click", () => watchDownloaded(item));
  } else {
    open.textContent = state.stage === "preparing" ? "Preparing" : "Downloading";
    open.className = "button-quiet";
    open.disabled = true;
  }

  const holder = document.createElement("span");
  holder.className = "library-name";
  holder.append(name, bar);

  if (withBadge) {
    const badge = document.createElement("span");
    badge.className = item.kept ? "badge badge-kept" : "badge";
    badge.textContent = item.kept ? "kept" : "temporary";
    row.append(holder, size, badge, open, remove);
  } else {
    row.append(holder, size, open, remove);
  }
  return row;
}

function renderLibrary(all) {
  // Remembered, because `setMode` needs to know whether there is anything to
  // show without going back to the server for it.
  el.library.dataset.empty = all.length === 0 ? "yes" : "";
  show(el.library, all.length > 0 && el.downloadedPanel.hidden);
  warnAboutDuplicates(all);
  el.libraryList.replaceChildren(...all.map((item) => libraryRow(item)));
}

/* What was downloaded rather than borrowed.
 *
 * Finished first, then whatever is still arriving, and alphabetical within
 * each: the point of the shelf is "what can I watch right now", and something
 * at 12% is not an answer to that however recently it was asked for. */
function renderDownloaded(all) {
  const kept = all
    .filter((item) => item.kept)
    .sort((a, b) => Number(b.complete) - Number(a.complete) || a.name.localeCompare(b.name));
  show(el.downloadedEmpty, kept.length === 0 && starting.size === 0);
  el.downloadedList.replaceChildren(
    ...kept.map((item) => libraryRow(item, { withBadge: false })),
  );
}

/* ---- carry on watching --------------------------------------------------
 *
 * On most evenings this is the answer to "what shall we watch", so it is drawn
 * above the search rather than below the library. Things watched to the end are
 * not here: the row is for what you are part way through. */
let continuingCount = 0;
let currentMode = "search";

/** Whether the continue row belongs on screen right now. */
function showContinuing() {
  show(el.continuing, currentMode !== "downloaded" && continuingCount > 0);
}

async function renderContinuing() {
  let items;
  try {
    items = await api("/api/continue");
  } catch {
    return;
  }

  /* Held on to, so `setMode` can decide whether this belongs on screen without
     asking the server again. Not shown while the shelf is open: it is drawn
     from watch history, which outlives the files, and two entries for things
     no longer on the machine sitting under an empty shelf reads as the shelf
     being broken. */
  continuingCount = items.length;
  showContinuing();
  el.continuingList.replaceChildren();

  for (const item of items) {
    const row = document.createElement("li");

    const title = document.createElement("div");
    title.className = "result-title";
    title.textContent = item.name;

    const left = Math.max(item.duration - item.seconds, 0);
    const meta = document.createElement("span");
    meta.className = "result-meta";
    meta.textContent = item.held
      ? `${seconds(left)} left`
      : `${seconds(left)} left  /  not on this machine any more`;
    title.append(meta);

    // A bar rather than a percentage: it is read at a glance from a sofa.
    const bar = document.createElement("div");
    bar.className = "progress";
    const filled = document.createElement("span");
    filled.style.width = `${Math.round(item.fraction * 100)}%`;
    bar.append(filled);

    const action = document.createElement("button");
    action.type = "button";
    action.className = "button-small";
    // Honest about what the button will do. Something swept last week has to
    // be fetched again before it can be resumed, and that takes as long as it
    // took the first time.
    action.textContent = item.held ? "Carry on" : "Fetch again";
    action.addEventListener("click", async () => {
      await openTorrent(item.infohash);
      if (current && current.infohash === item.infohash) play(item.file);
    });

    row.append(title, bar, action);
    el.continuingList.append(row);
  }
}

/* ---- polling ----------------------------------------------------------- */

async function refresh() {
  try {
    const all = await api("/api/torrents");
    renderLibrary(all);
    renderDownloaded(all);
    if (current) {
      const mine = all.find((item) => item.infohash === current.infohash);
      if (mine) renderStats(mine);
    }
  } catch (err) {
    // A failed poll is not worth tearing the page down over. The next one is
    // a second away.
    console.debug("poll failed", err);
  }
}

/* The continue row changes on the scale of an evening, not a second, so it is
 * drawn on the way in and after anything that could have changed it rather
 * than on the one-second poll. */
async function refreshContinuing() {
  await renderContinuing();
}

function startPolling() {
  if (poller) return;
  refresh();
  poller = setInterval(() => {
    // Nothing changes while the tab is hidden, and a backgrounded tab polling
    // a torrent client is just rudeness.
    if (document.visibilityState === "visible") refresh();
  }, 1000);
}

/* ---- search -------------------------------------------------------------
 *
 * Two indexes, described in one table rather than branched on in five places.
 * Each entry says what to call its subsets, where to fetch them, how to spell a
 * query, and how to turn one hit into a row. A third index would be a third
 * entry.
 *
 * Both hand back something `/api/resolve` accepts: the Archive gives an item
 * identifier the server turns into a torrent over HTTPS, apibay gives a magnet
 * assembled from an infohash. Neither the resolver nor the engine below it is
 * told which happened. */

const SOURCES = {
  ia: {
    label: "Internet Archive",
    /* What this index calls its subsets. */
    filterLabel: "Collection",
    catalogue: "/api/shelves",
    /* Both endpoints answer with a list of {key, label, note}; only the
     * envelope differs, and this is where that stops mattering. */
    options: (payload) => payload,
    /* A standing caution about the index itself, shown under the form. The
     * Archive's cautions belong to its individual collections, so it has none
     * of its own. */
    note: () => null,
    /* An empty query browses the collection, which is genuinely useful here. */
    allowsEmpty: true,
    /* The Archive's moving image library is mostly small and mostly old. There
     * is nothing here for a size cap to save anyone from, so the control does
     * not appear. */
    thinCap: () => null,
    query: (terms) => `/api/search?${new URLSearchParams({
      q: terms,
      shelf: el.filter.value,
      limit: "24",
    })}`,
    /* Nothing excluded for size, because nothing was filtered on it. */
    excluded: () => [],
    count: (data) => (data.hits.length ? `showing ${data.hits.length} of ${data.total}` : ""),
    empty: () => "Nothing in this collection matches that. Try fewer words, or a different collection.",
    row: (hit) => ({
      title: hit.title,
      meta: [hit.creator, hit.year, hit.identifier],
      size: hit.size,
      swarm: null,
      open: hit.identifier,
    }),
  },

  tpb: {
    label: "apibay",
    filterLabel: "Category",
    catalogue: "/api/tpb/categories",
    options: (payload) => payload.categories,
    note: (payload) => payload.note,
    /* apibay has no browse: an empty query returns its no-results sentinel,
     * which would surface as "nothing matches that", which is not what
     * happened. */
    allowsEmpty: false,
    /* Each category caps at what a thin line sustains over its typical
     * runtime, so the number differs between a film and an episode. Read off
     * the selected category rather than hardcoded, and shown on the control:
     * "under 506 MiB" is a claim someone can check, "small" is not. */
    thinCap: (option) => option.thin_cap,
    query: (terms) => `/api/tpb/search?${new URLSearchParams({
      q: terms,
      category: el.filter.value,
      limit: "24",
      thin: el.thin.checked ? "true" : "false",
    })}`,
    excluded: (data) => [
      data.oversize ? `${data.oversize} too large` : null,
      data.unseeded ? `${data.unseeded} unseeded` : null,
    ],
    count: (data) => {
      /* Reported even when nothing survived, because "0 shown, 41 too large"
       * is the answer to "why is this empty" and an empty list on its own is
       * not. A filter that hides most of a swarm has to say so. */
      const shown = data.hits.length ? `showing ${data.hits.length} of ${data.total}` : "nothing to show";
      const hidden = SOURCES.tpb.excluded(data).filter(Boolean);
      return hidden.length ? `${shown}, ${hidden.join(" and ")} hidden` : shown;
    },
    /* Advice, not an apology. With the cap on, an HD category is usually empty
     * for the honest reason that no 1080p release of anything fits a thin
     * line, and the useful next move is the standard definition category
     * rather than a broader one, which is mostly more HD. */
    empty: () =>
      el.thin.checked
        ? "Nothing here small enough for a thin line. The standard definition categories are " +
          "where the small releases live; failing that, untick the box and accept the stalling."
        : "Nothing in this category with anyone seeding it. Try fewer words, or a broader category.",
    row: (hit) => ({
      title: hit.name,
      meta: [
        hit.category_label,
        hit.num_files > 1 ? `${hit.num_files} files` : null,
        /* Epoch seconds from the server, formatted here, where the viewer's
         * timezone is actually known. */
        new Date(hit.added * 1000).toISOString().slice(0, 10),
        hit.username,
      ],
      size: hit.size_bytes,
      swarm: { seeders: hit.seeders, leechers: hit.leechers },
      open: hit.magnet,
    }),
  },
};

/* Anything the server can ask that is not one of the two written out above:
 * a Torznab indexer, or all of them at once. Built at startup from
 * `/api/sources`, because which indexers exist is the server's business and
 * not something this page can know.
 *
 * They all go through `/api/find`, which is the seam: one result shape, one
 * endpoint, several indexes asked at once and their answers merged. */
function findEntry({ key, label, note, keys }) {
  return {
    label,
    // No subsets. A Torznab indexer has categories, but which ones depends on
    // what is behind it, and offering the wrong list is worse than none.
    filterLabel: null,
    catalogue: null,
    options: () => [],
    note: () => note,
    allowsEmpty: false,
    thinCap: () => null,
    query: (terms) =>
      `/api/find?${new URLSearchParams({
        q: terms,
        sources: keys.join(","),
        limit: "30",
      })}`,
    excluded: () => [],
    count: (data) => {
      if (!data.hits.length && !data.failed.length) return "nothing to show";
      const parts = [`showing ${data.hits.length}`];
      if (data.duplicates) parts.push(`${data.duplicates} the same release twice`);
      // An index that did not answer must be said out loud: a short list is
      // otherwise indistinguishable from a thorough search that found little.
      if (data.failed.length) parts.push(`${data.failed.join(", ")} did not answer`);
      return parts.join(", ");
    },
    empty: () => "Nothing came back for that. Try fewer words, or another index.",
    row: (hit) => ({
      title: hit.title,
      // Which index found it, first: with several configured that is most of
      // what makes a mixed list legible.
      meta: [hit.sources.join(" + "), hit.detail],
      size: hit.size,
      // No leechers: `/api/find` reports a seeder count because that is the
      // only figure every index agrees to give, and inventing a zero for the
      // rest would make a healthy swarm look dead.
      swarm: hit.seeders === undefined ? null : { seeders: hit.seeders, leechers: null },
      open: hit.open,
    }),
    key,
  };
}

const source = () => SOURCES[el.source.value] || SOURCES.ia;

function setMode(mode) {
  for (const button of el.modes) {
    const active = button.dataset.mode === mode;
    button.setAttribute("aria-selected", active ? "true" : "false");
  }
  show(el.searchForm, mode === "search");
  show(el.form, mode === "link");
  show(el.downloadedPanel, mode === "downloaded");
  /* Not on the shelf. It is drawn from watch history, which outlives the files,
     so it happily shows two things that are no longer on the machine, directly
     under a shelf that is empty. That reads as the shelf being wrong. */
  currentMode = mode;
  showContinuing();
  /* The shelf is a strict subset of "On this machine", so drawing both at once
     shows the same row twice with a badge to tell you they are the same thing.
     The full list is a click away on either of the other tabs. */
  show(el.library, mode !== "downloaded" && !el.library.dataset.empty);
  // The shelf is drawn from the last poll, which on a cold page has not
  // happened yet. Asking now means the tab is never briefly empty when it
  // should not be.
  if (mode === "downloaded") refresh();
}

/* Ask the server which indexes it can reach, and add the ones this page does
 * not know about by name.
 *
 * The two written out above are always there. Torznab indexers are whatever
 * somebody configured, so they arrive at runtime, and an "everything" entry is
 * added once there is more than one thing to ask. */
async function loadIndexes() {
  let available;
  try {
    available = await api("/api/sources");
  } catch {
    // The two built in still work. A missing list of extras is not worth
    // saying anything about.
    return;
  }

  const extra = available.filter((entry) => entry.key.startsWith("torznab:"));
  for (const entry of extra) {
    SOURCES[entry.key] = findEntry({
      key: entry.key,
      label: entry.label,
      note: entry.note,
      keys: [entry.key],
    });
  }

  if (available.length > 1) {
    SOURCES.all = findEntry({
      key: "all",
      label: "Every index",
      note:
        "Asks all of them at once and folds results that are the same torrent " +
        "into one row. The cautions above apply to whichever index a row came from.",
      keys: available.map((entry) => entry.key),
    });
  }

  // Rebuild the menu, keeping whatever was selected.
  const selected = el.source.value;
  el.source.replaceChildren();
  for (const [key, entry] of Object.entries(SOURCES)) {
    const option = document.createElement("option");
    option.value = key;
    option.textContent = entry.label || key;
    el.source.append(option);
  }
  el.source.value = SOURCES[selected] ? selected : "ia";
}

/* Fetch the subsets the selected index offers, and fill the menu.
 *
 * Takes a ticket for the same reason `openTorrent` does: changing the index
 * twice in quick succession must not leave the first answer to arrive last and
 * fill the menu with the other index's entries. */
let loading = 0;

async function loadFilters() {
  const ticket = ++loading;
  const chosen = source();

  // A source with no subsets hides the menu rather than showing an empty one.
  if (!chosen.catalogue) {
    filters = [];
    el.filter.replaceChildren();
    show(el.filter.closest(".field") || el.filter, false);
    show(el.thinRow, false);
    el.filterNote.textContent = "";
    const note = chosen.note();
    el.sourceNote.textContent = note || "";
    show(el.sourceNote, Boolean(note));
    return;
  }
  show(el.filter.closest(".field") || el.filter, true);

  let payload;
  try {
    payload = await api(chosen.catalogue);
  } catch {
    if (ticket !== loading) return;
    /* Search is unusable without them, so say so rather than leaving an empty
     * menu the user cannot explain. */
    filters = [];
    el.filter.replaceChildren();
    el.filterNote.textContent =
      "Could not reach that index to load its categories. Paste a link instead.";
    show(el.sourceNote, false);
    return;
  }
  if (ticket !== loading) return;

  filters = chosen.options(payload) || [];
  el.filterLabel.textContent = chosen.filterLabel;
  el.filter.replaceChildren();
  for (const option of filters) {
    const node = document.createElement("option");
    node.value = option.key;
    node.textContent = option.label;
    el.filter.append(node);
  }
  showFilterNote();

  const note = chosen.note(payload);
  el.sourceNote.textContent = note || "";
  show(el.sourceNote, Boolean(note));
}

function showFilterNote() {
  const option = filters.find((entry) => entry.key === el.filter.value);
  el.filterNote.textContent = option ? option.note : "";

  const cap = option ? source().thinCap(option) : null;
  show(el.thinRow, cap !== null && cap !== undefined);
  if (cap) {
    el.thinLabel.textContent = `Fits a thin line (under ${bytes(cap)})`;
    /* The whole torrent is measured, not the file you would actually watch.
     * Balerion only fetches what the player asks for, so a season pack streams
     * one episode at a time and is judged here as though you wanted all of it.
     * Said out loud rather than quietly excluding packs, which would be the
     * clever wrong answer. */
    el.thinRow.title =
      `Hides anything larger than ${bytes(cap)}, which is about what 1.5 Mbit/s ` +
      `sustains over a typical ${el.filter.value.includes("tv") ? "episode" : "film"}. ` +
      `Measured on the whole torrent, so season packs are excluded even though ` +
      `you would only stream one episode of one.`;
  }
}

function renderResults(data) {
  const chosen = source();
  show(el.results, true);
  el.resultList.replaceChildren();
  el.resultsCount.textContent = chosen.count(data);

  if (!data.hits.length) {
    const empty = document.createElement("li");
    empty.className = "hint";
    empty.textContent = chosen.empty();
    el.resultList.append(empty);
    return;
  }

  for (const hit of data.hits) {
    const view = chosen.row(hit);
    const row = document.createElement("li");

    const title = document.createElement("div");
    title.className = "result-title";
    title.textContent = view.title;
    const meta = document.createElement("span");
    meta.className = "result-meta";
    meta.textContent = view.meta.filter(Boolean).join("  /  ");
    title.append(meta);

    /* Always appended, even when empty, so the grid columns line up between an
     * index that reports a swarm and one that does not. */
    const swarm = document.createElement("span");
    swarm.className = "result-swarm";
    if (view.swarm) {
      const seeders = document.createElement("span");
      /* Under about ten seeders a stream will probably not keep ahead of the
       * playhead, and the viewer may as well know that before clicking. */
      seeders.className = view.swarm.seeders < 10 ? "result-seeders-thin" : "result-seeders";
      seeders.textContent = `${view.swarm.seeders}`;
      swarm.append(seeders);
      /* Only shown when the index actually said. Printing "/ 0" for an index
       * that reports seeders and nothing else claims a fact nobody has, and a
       * swarm with no leechers looks like a dead one. */
      if (view.swarm.leechers !== null && view.swarm.leechers !== undefined) {
        swarm.append(` / ${view.swarm.leechers}`);
        swarm.title = `${view.swarm.seeders} seeding, ${view.swarm.leechers} leeching`;
      } else {
        swarm.title = `${view.swarm.seeders} seeding; this index does not report leechers`;
      }
    }

    const size = document.createElement("span");
    size.className = "result-size";
    size.textContent = view.size ? bytes(view.size) : "";

    /* Download first, and it is the primary one.
     *
     * The other way round was the wrong default. Watching something as it
     * arrives is only as good as the swarm: a release with one peer at
     * 200 KB/s stutters, seeking crawls, and none of that is the player's
     * doing or the player's to fix. Downloading it first takes as long as it
     * takes, says so while it does, and then plays perfectly. That is the
     * path most people want most of the time, so it is the one in front. */
    const save = document.createElement("button");
    save.type = "button";
    save.className = "button-small";
    save.textContent = "Download";
    save.addEventListener("click", () => downloadTorrent(view.open, save));

    const action = document.createElement("button");
    action.type = "button";
    action.className = "button-quiet";
    action.textContent = "Watch now";
    action.title = "Starts at once, but only goes as fast as the swarm does";
    action.addEventListener("click", () => openTorrent(view.open));

    const actions = document.createElement("span");
    actions.className = "result-actions";
    actions.append(save, action);

    row.append(title, swarm, size, actions);
    el.resultList.append(row);
  }
}

/** A line under the tabs, for things that happen while you are still browsing. */
function sayInIntake(message) {
  el.intakeStatus.textContent = message;
  show(el.intakeStatus, Boolean(message));
}

/* Start something and keep it, without taking over the page.
 *
 * Deliberately not `openTorrent` with a flag. Watching is a thing you do now,
 * so it is worth clearing the screen for; downloading is a thing you will do
 * later, and a viewer that appears over the results you were still reading is
 * an answer to a question nobody asked. The button reports on itself instead,
 * and the Downloaded tab is where it ends up.
 *
 * Resolving a cold magnet takes tens of seconds, so the button is disabled
 * throughout: two clicks would be two `/api/resolve` calls for the same
 * infohash, and while the second is harmless it looks like nothing happened. */
/* What has been asked for but has not appeared yet.
 *
 * Resolving a cold magnet means asking the swarm for a file list, which takes
 * tens of seconds and sometimes a minute and a half. Until that answers there
 * is no torrent, so the shelf has nothing to draw, so pressing Download and
 * arriving at an empty shelf is the correct behaviour and a terrible one: it
 * is indistinguishable from the button not having worked.
 *
 * So the row appears the moment you press it, and says what it is doing. */
const starting = new Map();

async function downloadTorrent(what, button) {
  const said = button?.textContent;
  if (button) {
    button.disabled = true;
    button.textContent = "Starting";
  }
  starting.set(what, { asked: what, since: Date.now(), failed: null });
  renderStarting();
  try {
    const info = await api("/api/resolve", json({ magnet: what, keep: true }));
    // Removed from the list *and* redrawn. Forgetting the second half left the
    // pending row on screen beside the shelf row for the same thing, which
    // reads as having downloaded it twice.
    starting.delete(what);
    renderStarting();
    if (button) button.textContent = "Downloading";
    sayInIntake(
      `${info.name} is downloading and will be kept. It is under Downloaded, ` +
      `and you can watch it before it finishes.`,
    );
    refresh();
  } catch (err) {
    // Left on the shelf rather than removed, carrying the reason. A row that
    // vanishes on failure is the silent version of this bug all over again.
    const held = starting.get(what);
    if (held) held.failed = err.message ?? String(err);
    renderStarting();
    if (button) {
      button.disabled = false;
      button.textContent = said ?? "Download";
    }
    sayInIntake(`That would not start: ${err.message ?? err}`);
  }
}

/** Draw the rows for things asked for but not yet arrived. */
function renderStarting() {
  el.startingList.replaceChildren();
  show(el.startingList, starting.size > 0);
  for (const [key, item] of starting) {
    const row = document.createElement("li");

    const name = document.createElement("span");
    // A magnet is not a name. Until the swarm says otherwise, the display name
    // out of the link is the best there is, and an infohash after that.
    const shown = decodeURIComponent(key).match(/[?&]dn=([^&]+)/);
    name.textContent = shown
      ? decodeURIComponent(shown[1]).replace(/\+/g, " ")
      // An archive.org identifier has no display name and is usually short
      // enough to show whole. Only trim what is actually too long.
      : key.length > 44 ? `${key.slice(0, 44)}…` : key;

    const state = document.createElement("span");
    state.className = item.failed ? "library-size state-failed" : "library-size state-downloading";
    state.textContent = item.failed
      ? item.failed
      : "asking the swarm";

    const give = document.createElement("button");
    give.type = "button";
    give.className = "button-quiet";
    give.textContent = item.failed ? "Try again" : "Give up";
    give.addEventListener("click", () => {
      starting.delete(key);
      renderStarting();
      if (item.failed) downloadTorrent(key, null);
    });

    row.append(name, state, give);
    el.startingList.append(row);
  }
}

/* Resolve anything the server understands: an identifier from a search
 * result, or whatever was pasted into the link box.
 *
 * Resolving a cold magnet can take half a minute, and a viewer who gets bored
 * and clicks something else must not have the first request finish later and
 * redraw the page underneath them. Each attempt takes a ticket, and only the
 * newest one is allowed to touch the interface. */
let opening = 0;
/** Whether there were results to return to when this torrent was opened. */
let hadResults = false;

async function openTorrent(what, alternatives = []) {
  const ticket = ++opening;
  /* A magnet resolves by asking the swarm for a file list, and a swarm can have
   * seeders that will send data but none that will answer that request. When the
   * caller has offered other releases of the same thing, trying the next one
   * beats reporting a failure the viewer can do nothing about. */
  const queue = [what, ...alternatives];

  show(el.failure, false);
  show(el.torrent, false);
  show(el.pending, true);
  const doneWaiting = sayWhileWaiting(el.pendingNote, [
    { text: "Fetching the file list" },
    {
      after: 8_000,
      text:
        "Still looking for peers. A magnet carries no file list, so one has to " +
        "be asked for, and a cold swarm can take a minute to answer.",
    },
    {
      after: 40_000,
      text:
        "This swarm is slow to answer. It may have very few peers, or none that " +
        "will serve the file list. Still trying.",
    },
  ], () => ticket === opening);

  /* The results have done their job. Leaving two dozen rows above the player
   * pushes it down the page and makes the thing you actually asked for the
   * least visible item on screen.
   *
   * Hidden rather than emptied, so going back costs nothing and does not
   * re-run the search. */
  hadResults = !el.results.hidden;
  show(el.results, false);
  show(el.backToResults, hadResults);
  window.scrollTo({ top: 0, behavior: "smooth" });

  try {
    let info = null;
    let lastError = null;
    for (const [attempt, magnet] of queue.entries()) {
      if (ticket !== opening) return; // superseded; leave the page alone
      if (attempt > 0) {
        el.pendingNote.textContent =
          `That release had no peer willing to send a file list. ` +
          `Trying another (${attempt + 1} of ${queue.length}).`;
      }
      try {
        info = await api("/api/resolve", json({ magnet }));
        break;
      } catch (err) {
        lastError = err;
      }
    }
    if (ticket !== opening) return;
    if (!info) throw lastError ?? new Error("nothing to open");
    renderTorrent(info);
    refresh();
  } catch (err) {
    if (ticket !== opening) return;
    show(el.failure, true);
    el.failureBody.textContent =
      queue.length > 1
        ? `None of the ${queue.length} releases tried would open. ${err.message}`
        : err.message;
  } finally {
    doneWaiting();
    // Whatever happened, the spinner goes. Hiding it only on the paths that
    // completed is what left it stranded above a perfectly good player.
    if (ticket === opening) show(el.pending, false);
  }
}

el.searchForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const chosen = source();
  const terms = el.query.value.trim();
  if (!terms && !chosen.allowsEmpty) {
    el.query.focus();
    return;
  }

  el.searchSubmit.disabled = true;
  show(el.failure, false);
  el.resultsCount.textContent = "searching";
  show(el.results, true);

  try {
    renderResults(await api(chosen.query(terms)));
    // A fresh list is worth returning to, whatever was open before.
    show(el.backToResults, true);
  } catch (err) {
    show(el.results, false);
    show(el.failure, true);
    el.failureBody.textContent = err.message;
  } finally {
    el.searchSubmit.disabled = false;
  }
});

el.filter.addEventListener("change", showFilterNote);

/* Ticking the box with results on screen would otherwise leave a list that no
 * longer matches the control above it. */
el.thin.addEventListener("change", () => {
  if (!el.results.hidden) el.searchForm.requestSubmit();
});

/* Changing the index invalidates whatever is on screen: those results belong
 * to the other one, and leaving them under a new menu is a fine way to click
 * the wrong thing. */
el.source.addEventListener("change", () => {
  show(el.results, false);
  el.resultList.replaceChildren();
  el.resultsCount.textContent = "";
  loadFilters();
});

for (const button of el.modes) {
  button.addEventListener("click", () => setMode(button.dataset.mode));
}

/* ---- events ------------------------------------------------------------ */

el.form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const magnet = el.magnet.value.trim();
  if (!magnet) return;

  el.submit.disabled = true;
  try {
    await openTorrent(magnet);
  } finally {
    el.submit.disabled = false;
  }
});

el.keep.addEventListener("change", async () => {
  if (!current) return;
  try {
    await api(`/api/torrents/${current.infohash}/keep`, json({ keep: el.keep.checked }));
    refresh();
  } catch (err) {
    el.keep.checked = !el.keep.checked;
    show(el.failure, true);
    el.failureBody.textContent = err.message;
  }
});

/* Back to the list, without disturbing the player.
 *
 * The torrent panel stays where it is rather than being hidden: hiding a playing
 * video does not stop it, and a page that carries on with sound from something
 * you cannot see is worse than one that shows you both. */
el.backToResults.addEventListener("click", () => {
  show(el.results, true);
  el.results.scrollIntoView({ behavior: "smooth", block: "start" });
});

el.discard.addEventListener("click", async () => {
  if (!current) return;
  if (!window.confirm(`Delete ${current.name} and its files from disk?`)) return;
  await api(`/api/torrents/${current.infohash}`, { method: "DELETE" });
  show(el.torrent, false);
  teardown();
  el.video.removeAttribute("src");
  el.video.load();
  current = null;
  playing = null;
  refresh();
});

// Redraw on resize so the map stays crisp. There is no longer a theme-change
// listener here: the page is dark-locked, so the colours the canvas reads off
// the root never change under it.
window.addEventListener("resize", () => {
  if (current) refresh();
});

/* ---- waiting -----------------------------------------------------------
 *
 * Two waits here are honest but slow, and a static line of text makes a slow
 * success look exactly like a hang. Both of them are waiting on the swarm, and
 * both can legitimately take a minute:
 *
 *   Resolving  a cold magnet has to get its file list from a peer, and peers
 *              have to be found first.
 *   Probing    ffprobe reads the head of the file, which has to arrive off the
 *              swarm before it can say what is in it.
 *
 * So say what is happening and roughly how long is reasonable. Nothing here
 * changes what the server does; it changes whether you can tell the difference
 * between working and stuck. */
function sayWhileWaiting(node, stages, stillWanted) {
  node.textContent = stages[0].text;
  const timers = stages.slice(1).map((stage) =>
    window.setTimeout(() => {
      /* Guarded, because clicking a second file while the first is still
       * waiting leaves the first call's timers armed, and they would happily
       * overwrite the new one's message with the old one's. */
      if (stillWanted()) node.textContent = stage.text;
    }, stage.after)
  );
  return () => timers.forEach(window.clearTimeout);
}

/* A magnet handed over in the fragment, so the search site can send you here
 * with something already chosen.
 *
 * The fragment rather than a query string on purpose: it never leaves the
 * browser, so the magnet does not appear in this server's request log, in any
 * proxy's, or in the Referer of anything the page later loads. It costs nothing
 * to prefer.
 *
 * Cleared from the address bar once read, so a reload does not resolve it a
 * second time and so the link is not left sitting in history. */
function openFromFragment() {
  const fragment = window.location.hash.replace(/^#/, "");
  if (!fragment) return false;

  const params = new URLSearchParams(fragment);

  /* `#downloaded` on its own: the search site's shelf link, which is a link to
   * this tab rather than to any particular thing. */
  if (!params.get("magnet") && params.has("downloaded")) {
    history.replaceState(null, "", window.location.pathname);
    setMode("downloaded");
    return true;
  }

  const magnet = params.get("magnet");
  if (!magnet) return false;
  // Other releases of the same thing, in the order whoever sent us here ranked
  // them. Only used if the first will not open.
  const alternatives = params.getAll("alt");

  history.replaceState(null, "", window.location.pathname);

  /* The two paths, arriving from elsewhere. `keep=1` is the search site's
   * Download button: fetch the whole thing, keep it, and show the shelf it
   * landed on rather than a player nobody asked for. */
  if (params.get("keep") === "1") {
    setMode("downloaded");
    downloadTorrent(magnet, null);
    return true;
  }

  setMode("link");
  el.magnet.value = magnet;
  openTorrent(magnet, alternatives);
  return true;
}

/* Open on the shelf when there is anything on it.
 *
 * A television opens on what you were watching, not on a search box. Only when
 * the shelf is empty is finding something the first thing you want, and then
 * the search tab is the obvious front door. Decided after the first poll, so it
 * is decided on fact rather than on a guess. */
setMode("search");
refresh().then(() => {
  const shelf = el.downloadedList.childElementCount;
  if (shelf > 0 && !window.location.hash) setMode("downloaded");
});
loadIndexes().then(loadFilters);
loadCastBase();
startPolling();
refreshContinuing();
openFromFragment();

// After playing something, and when the tab is looked at again, since the
// position may have moved on another device pointed at the same server.
el.video.addEventListener("pause", () => refreshContinuing());

/* The lock screen's own scrubber, kept honest. `timeupdate` fires about four
   times a second, which is more often than any lock screen redraws, so it is
   thinned to once a second. */
let toldTheSystemAt = 0;
el.video.addEventListener("timeupdate", () => {
  const now = el.video.currentTime;
  if (Math.abs(now - toldTheSystemAt) < 1) return;
  toldTheSystemAt = now;
  tellTheSystemWhereWeAre();
});
el.video.addEventListener("loadedmetadata", tellTheSystemWhereWeAre);

// Safari only. Everywhere else this event never fires and nothing changes.
el.video.addEventListener(
  "webkitcurrentplaybacktargetiswirelesschanged",
  handedToATelevision,
);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") refreshContinuing();
});

/* A second magnet arriving at an already-open tab.
 *
 * The search site aims its Watch links at a named window, so the first click
 * opens this page and every click after that lands on the tab already showing
 * it. That is a same-document navigation: the page does not reload, so reading
 * the fragment once at startup would quietly ignore every magnet after the
 * first. `replaceState` does not fire this event, so clearing the fragment above
 * cannot loop back round here. */
window.addEventListener("hashchange", openFromFragment);
