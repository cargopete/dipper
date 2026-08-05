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
  shelf: $("shelf"),
  shelfNote: $("shelf-note"),
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
let shelves = [];

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
  const session = { source, url, info, next: 0, stopped: false, buffer: null };
  mse = session;

  show(el.video, true);
  show(el.viewerNote, false);
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
  try {
    const response = await fetch(`${info.segment_prefix}${index}`);
    if (!response.ok) throw new Error(await errorText(response));
    const bytes = await response.arrayBuffer();
    if (session.stopped || session !== mse) return;

    await idle(buffer);
    buffer.timestampOffset = index * info.segment_seconds;
    await appendWithEviction(buffer, bytes);
    session.next = index + 1;
  } catch (err) {
    if (!session.stopped) {
      showViewerNote(`Playback stopped at segment ${index}: ${err.message}`);
      session.stopped = true;
    }
  } finally {
    session.busy = false;
  }
  // Keep going until the buffer is comfortable.
  if (mse === session && !session.stopped) setTimeout(pump, 0);
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

  attachTracks(info.tracks);

  if (info.mode === "unsupported") {
    showViewerNote(info.reason, info.download);
    return;
  }

  if (info.mode === "direct") {
    show(el.video, true);
    show(el.viewerNote, false);
    el.video.src = info.url;
    el.video.load();
    el.video.play().catch(() => {
      // Autoplay refused. The controls are right there.
    });
    return;
  }

  await startTranscode(info);
  el.video.play().catch(() => {});
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

/* ---- search ------------------------------------------------------------ */

function setMode(mode) {
  for (const button of el.modes) {
    const active = button.dataset.mode === mode;
    button.setAttribute("aria-selected", active ? "true" : "false");
  }
  show(el.searchForm, mode === "search");
  show(el.form, mode === "link");
}

async function loadShelves() {
  try {
    shelves = await api("/api/shelves");
  } catch {
    // Search is unusable without them, so say so rather than leaving an
    // empty menu the user cannot explain.
    el.shelfNote.textContent =
      "Could not reach archive.org to load the collections. Paste a link instead.";
    return;
  }

  el.shelf.replaceChildren();
  for (const shelf of shelves) {
    const option = document.createElement("option");
    option.value = shelf.key;
    option.textContent = shelf.label;
    el.shelf.append(option);
  }
  showShelfNote();
}

function showShelfNote() {
  const shelf = shelves.find((s) => s.key === el.shelf.value);
  el.shelfNote.textContent = shelf ? shelf.note : "";
}

function renderResults(data) {
  show(el.results, true);
  el.resultList.replaceChildren();

  el.resultsCount.textContent = data.hits.length
    ? `showing ${data.hits.length} of ${data.total}`
    : "";

  if (!data.hits.length) {
    const empty = document.createElement("li");
    empty.className = "hint";
    empty.textContent =
      "Nothing in this collection matches that. Try fewer words, or a different collection.";
    el.resultList.append(empty);
    return;
  }

  for (const hit of data.hits) {
    const row = document.createElement("li");

    const title = document.createElement("div");
    title.className = "result-title";
    title.textContent = hit.title;
    const meta = document.createElement("span");
    meta.className = "result-meta";
    meta.textContent = [hit.creator, hit.year, hit.identifier]
      .filter(Boolean)
      .join("  /  ");
    title.append(meta);

    const size = document.createElement("span");
    size.className = "result-size";
    size.textContent = hit.size ? bytes(hit.size) : "";

    const action = document.createElement("button");
    action.type = "button";
    action.className = "button-small";
    action.textContent = "Watch";
    action.addEventListener("click", () => openTorrent(hit.identifier));

    row.append(title, size, action);
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
  el.searchSubmit.disabled = true;
  show(el.failure, false);
  el.resultsCount.textContent = "searching";
  show(el.results, true);

  try {
    const params = new URLSearchParams({
      q: el.query.value.trim(),
      shelf: el.shelf.value,
      limit: "24",
    });
    renderResults(await api(`/api/search?${params}`));
  } catch (err) {
    show(el.results, false);
    show(el.failure, true);
    el.failureBody.textContent = err.message;
  } finally {
    el.searchSubmit.disabled = false;
  }
});

el.shelf.addEventListener("change", showShelfNote);

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
loadShelves();
startPolling();
