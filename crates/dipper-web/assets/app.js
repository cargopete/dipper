/* dipper: the page.
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
  keep: $("keep"),
  discard: $("discard"),
  viewer: $("viewer"),
  video: $("video"),
  viewerNote: $("viewer-note"),
  files: $("file-list"),
  map: $("piecemap"),
  mapStatus: $("piecemap-status"),
  advisory: $("advisory"),
  library: $("library"),
  libraryList: $("library-list"),
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

/* ---- formatting -------------------------------------------------------- */

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

function bytes(n) {
  if (!n) return "0 B";
  let size = n;
  let unit = 0;
  while (size >= 1024 && unit < UNITS.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size < 10 && unit > 0 ? size.toFixed(1) : Math.round(size)} ${UNITS[unit]}`;
}

function seconds(n) {
  if (!isFinite(n) || n <= 0) return "0 s";
  if (n < 90) return `${Math.round(n)} s`;
  return `${Math.floor(n / 60)}m ${Math.round(n % 60)}s`;
}

function show(node, visible) {
  node.hidden = !visible;
}

/* ---- talking to the server --------------------------------------------- */

async function api(path, options) {
  const response = await fetch(path, options);
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
 * For containers browsers cannot open, dipper converts on the fly and hands
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

    const init = await fetch(info.init);
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
    const response = await fetch(`${info.segment_prefix}${index}`);

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
function roughly(secs) {
  if (!isFinite(secs)) return "a very long time";
  if (secs < 90) return "under a minute";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `about ${Math.max(1, Math.round(mins / 5) * 5)} minutes`;
  const hours = secs / 3600;
  return hours < 1.5 ? "about an hour" : `about ${Math.round(hours)} hours`;
}

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
    node.src = track.url;
    if (index === 0) node.default = true;
    el.video.append(node);
  }
}

/** Play a file, whatever it turns out to be. */
async function play(index) {
  const file = current.files[index];
  if (!file) return;

  teardown();
  playing = index;
  playInfo = null;
  forgetRates();
  show(el.advisory, false);
  // The loading panel belongs to resolving, not to playing. Anything that
  // puts a picture on the screen must clear it.
  show(el.pending, false);
  renderFiles();
  teardown();
  el.video.removeAttribute("src");
  el.video.load();
  show(el.video, false);
  show(el.viewerNote, true);
  el.viewerNote.textContent = "Working out how to play this";

  let info;
  try {
    info = await api(`/api/play/${current.infohash}/${index}`);
  } catch (err) {
    showViewerNote(err.message);
    return;
  }
  if (playing !== index) return; // the viewer moved on while we asked

  playInfo = info;
  attachTracks(info.tracks);

  if (info.mode === "unsupported") {
    showViewerNote(info.reason, info.download);
    return;
  }

  if (info.mode === "direct") {
    show(el.video, true);
    show(el.viewerNote, false);
    show(el.pending, false);
    el.video.src = info.url;
    el.video.load();
    el.video.play().catch(() => {
      // Autoplay refused. The controls are right there.
    });
    return;
  }

  await startTranscode(info);
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

function renderFiles() {
  el.files.replaceChildren();

  for (const file of current.files) {
    const row = document.createElement("li");

    const name = document.createElement("div");
    name.className = "file-name";
    const title = document.createElement("span");
    title.textContent = file.name;
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

function renderLibrary(all) {
  show(el.library, all.length > 0);
  el.libraryList.replaceChildren();

  for (const item of all) {
    const row = document.createElement("li");

    const name = document.createElement("span");
    name.textContent = item.name;

    const size = document.createElement("span");
    size.className = "library-size";
    const percent = item.pieces_total
      ? Math.floor((item.pieces_have / item.pieces_total) * 100)
      : 0;
    size.textContent = item.complete
      ? bytes(item.total_length)
      : `${bytes(item.bytes_on_disk)} of ${bytes(item.total_length)} (${percent}%)`;

    const badge = document.createElement("span");
    badge.className = item.kept ? "badge badge-kept" : "badge";
    badge.textContent = item.kept ? "kept" : "temporary";

    // Reopen something already on disk. Resolving by infohash finds the
    // running session rather than starting a second one, so a part-finished
    // torrent carries on downloading from where it stopped.
    const open = document.createElement("button");
    open.type = "button";
    open.className = "button-small";
    open.textContent = item.complete ? "Watch" : "Resume";
    open.addEventListener("click", () => openTorrent(item.infohash));

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

    row.append(name, size, badge, open, remove);
    el.libraryList.append(row);
  }
}

/* ---- polling ----------------------------------------------------------- */

async function refresh() {
  try {
    const all = await api("/api/torrents");
    renderLibrary(all);
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

const source = () => SOURCES[el.source.value] || SOURCES.ia;

function setMode(mode) {
  for (const button of el.modes) {
    const active = button.dataset.mode === mode;
    button.setAttribute("aria-selected", active ? "true" : "false");
  }
  show(el.searchForm, mode === "search");
  show(el.form, mode === "link");
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
     * dipper only fetches what the player asks for, so a season pack streams
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
      swarm.append(seeders, ` / ${view.swarm.leechers}`);
      swarm.title = `${view.swarm.seeders} seeding, ${view.swarm.leechers} leeching`;
    }

    const size = document.createElement("span");
    size.className = "result-size";
    size.textContent = view.size ? bytes(view.size) : "";

    const action = document.createElement("button");
    action.type = "button";
    action.className = "button-small";
    action.textContent = "Watch";
    action.addEventListener("click", () => openTorrent(view.open));

    row.append(title, swarm, size, action);
    el.resultList.append(row);
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

async function openTorrent(what) {
  const ticket = ++opening;

  show(el.failure, false);
  show(el.torrent, false);
  show(el.pending, true);
  el.pendingNote.textContent = "Fetching the file list";
  window.scrollTo({ top: 0, behavior: "smooth" });

  try {
    const info = await api("/api/resolve", json({ magnet: what }));
    if (ticket !== opening) return; // superseded; leave the page alone
    renderTorrent(info);
    refresh();
  } catch (err) {
    if (ticket !== opening) return;
    show(el.failure, true);
    el.failureBody.textContent = err.message;
  } finally {
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

// Redraw on resize so the map stays crisp, and on theme change so it stays
// the right colour.
window.addEventListener("resize", () => {
  if (current) refresh();
});
window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (current) refresh();
});

setMode("search");
loadFilters();
startPolling();
