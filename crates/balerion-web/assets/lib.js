/* The parts of the page that are only arithmetic.
 *
 * app.js is fifteen hundred lines and had no tests at all, which was defensible
 * while it was mostly DOM: a test of "does this element end up hidden" costs
 * more than it catches. The functions in here are not that. They are the ones
 * where the subtle bugs live, they take values and return values, and there was
 * no way to run one without a browser.
 *
 * So they live in their own file, loaded before app.js by the page and imported
 * directly by `node --test`. Nothing here touches the DOM, the network, or any
 * state above it; anything that does belongs in app.js.
 *
 * Exported two ways on purpose, because this file has two callers: a browser
 * that has no module loader here, and a test runner that wants one.
 *
 * Wrapped in a function so that nothing but `BalerionLib` escapes. A bare
 * `function bytes()` at the top level of a classic script becomes a global, and
 * app.js's `const { bytes } = ...` then fails with "Identifier 'bytes' has
 * already been declared", which kills the entire script, leaves the page with
 * no event handlers at all, and cannot be caught by checking either file on its
 * own.
 */

(function () {
const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

/** A byte count, in the units a person would say it in. */
function bytes(n) {
  if (!n || !isFinite(n) || n < 0) return "0 B";
  let size = n;
  let unit = 0;
  while (size >= 1024 && unit < UNITS.length - 1) {
    size /= 1024;
    unit += 1;
  }
  // One decimal below ten, none above: "1.4 GiB" is useful, "1.4 KiB" is
  // noise, and "1434 MiB" is a number nobody reads.
  return `${size < 10 && unit > 0 ? size.toFixed(1) : Math.round(size)} ${UNITS[unit]}`;
}

/** A duration, short form. */
function seconds(n) {
  if (!isFinite(n) || n <= 0) return "0 s";
  if (n < 90) return `${Math.round(n)} s`;
  return `${Math.floor(n / 60)}m ${Math.round(n % 60)}s`;
}

/** A duration, deliberately vague, for an estimate that does not deserve
 * precision. Rounded to five minutes because a download estimate is a guess
 * and "about 35 minutes" claims less than "34m 12s" while being just as
 * useful. */
function roughly(secs) {
  if (!isFinite(secs)) return "a very long time";
  if (secs < 90) return "under a minute";

  const mins = Math.round(secs / 60);
  // Under ten minutes the exact figure is worth having and rounding to five
  // would be absurd. It also fixes an "about 1 minutes" that the old guard
  // produced for anything between ninety seconds and two and a half minutes.
  if (mins < 10) return `about ${mins} minute${mins === 1 ? "" : "s"}`;
  if (mins < 60) return `about ${Math.round(mins / 5) * 5} minutes`;

  const hours = secs / 3600;
  return hours < 1.5 ? "about an hour" : `about ${Math.round(hours)} hours`;
}

/** The season and episode a filename claims, as `S01E02`, or null.
 *
 * Two spellings, because two occur: `S01E02` in any case with any separator or
 * none, and the older `1x02`. Deliberately not a general parser; anything
 * cleverer starts matching resolutions and years. */
function episodeTagOf(name) {
  const m =
    /(?:^|[^a-z0-9])s(\d{1,2})[^a-z0-9]?e(\d{1,3})(?![0-9])/i.exec(name) ||
    /(?:^|[^a-z0-9])(\d{1,2})x(\d{1,3})(?![0-9])/i.exec(name);
  if (!m) return null;
  const pad = (n) => String(Number(n)).padStart(2, "0");
  return `S${pad(m[1])}E${pad(m[2])}`;
}

/* Which programme, as well as which episode.
 *
 * `episodeTagOf` answers "S01E01" and nothing else, which is fine for labelling
 * and wrong for grouping: Better Call Saul S01E01 and Breaking Bad S01E01 are
 * not two copies of anything, and saying they are while offering to delete one
 * is worse than saying nothing. So the title in front of the marker is taken
 * too, with the index's own prefix and the usual dots and underscores rubbed
 * off it.
 *
 * Approximate on purpose. It only has to be right often enough to stop a false
 * accusation, and two releases of the same episode that spell the title
 * differently are a missed warning rather than a wrong one. */
function seriesOf(name) {
  const tag = episodeTagOf(name);
  if (!tag) return null;
  const marker = /(?:^|[^a-z0-9])(?:s\d{1,2}[^a-z0-9]?e\d{1,3}|\d{1,2}x\d{1,3})(?![0-9])/i;
  const at = marker.exec(name);
  const before = at ? name.slice(0, at.index) : "";
  const series = before
    // "www.SomeIndex.org - " and friends, which say nothing about the show.
    .replace(/^\s*(?:www\.)?[a-z0-9-]+\.(?:org|com|net|to|me|se|info)\s*[-–—:]?\s*/i, "")
    // Scene names use dots and underscores where a person would use spaces.
    .replace(/[._]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .toLowerCase();
  return { series, tag, key: `${series}|${tag}` };
}

/** Can this be watched as it downloads, at the rate the swarm is managing?
 *
 * `needed` and `rate` are both bytes per second. The margin exists because a
 * download that exactly matches the bitrate stalls constantly: every dip below
 * the average empties the buffer, and the average is where it spends half its
 * time.
 *
 * Returns null when there is not enough to say, which is a different answer
 * from "no" and must not be shown as one. */
function feasible(needed, rate, { margin = 1.15 } = {}) {
  if (!isFinite(needed) || needed <= 0) return null;
  if (!isFinite(rate) || rate <= 0) return null;
  return {
    ok: rate >= needed * margin,
    ratio: rate / needed,
    // How long the whole thing would take at this rate, for the case where
    // downloading it first is the honest advice.
    shortfall: needed * margin - rate,
  };
}

/** Whether to change a verdict that is already on screen.
 *
 * Hysteresis, and the reason for it is the whole point: a rate hovering around
 * the threshold flips the advisory on and off every second, which reads as a
 * page that is broken rather than as a connection that is marginal. A verdict
 * has to be clearly wrong before it is replaced. */
function shouldChangeVerdict(previous, next, ratio) {
  if (previous === null || previous === undefined) return true;
  if (previous === next) return false;
  // Going from "this is fine" to "this will stall" needs the rate to be
  // properly short, not a fraction short; the other way needs proper headroom.
  return next ? ratio > 1.3 : ratio < 0.9;
}

/* Which address the media should be fetched from.
 *
 * `castBase` is a LAN address serving the same bytes, and exists so that a
 * television handed a URL can fetch it: AirPlay does not mirror a video
 * element, it passes the receiver a URL. Pointing playback at it too was free
 * *while the player and the browser were the same machine*, because then the
 * player's LAN address is this machine's own.
 *
 * Once the player moved to a box that stays on, that stopped being true. A
 * phone on a tunnel cannot reach `192.168.0.13`, so the video element fetched
 * nothing and sat at 0:00 with no error to show for it.
 *
 * `hostname` is what decides: served from loopback, the player is here and its
 * LAN address is ours. Served from anywhere else, use the path as given, so
 * the media comes from the origin that served the page. That is the one
 * address the viewer is definitely able to reach, since they are looking at it.
 */
const LOOPBACK = ["localhost", "127.0.0.1", "[::1]", "::1"];

function mediaUrl(path, castBase, hostname) {
  if (!path) return path;
  // Already absolute: whoever built it has decided.
  if (path.startsWith("http")) return path;
  if (!castBase) return path;
  if (!LOOPBACK.includes(hostname)) return path;
  return `${castBase}${path}`;
}

/* Which of four states a thing in the library is in, and what to say about it.
 *
 * The whole point of the download-first path is that a person can look at a row
 * and know whether to put the kettle on. That means one function deciding, and
 * it means being honest when the answer is "nobody knows": a swarm with no
 * peers has no rate, and inventing an estimate from a seeder count would be
 * making something up.
 *
 * The four:
 *
 *   downloading  still arriving. Says how much and how long.
 *   preparing    all here, being converted. Says how far.
 *   ready        converted. Starts at once and seeks to the frame.
 *   playable     all here and needing no conversion, which is what an MP4
 *                that browsers already open looks like.
 */
function stateOf(item, { roughly: say = roughly } = {}) {
  const total = item.total_length || 0;
  const held = item.bytes_on_disk || 0;

  if (!item.complete) {
    const fraction = item.pieces_total ? item.pieces_have / item.pieces_total : 0;
    const rate = item.rate || 0;
    const left = Math.max(total - held, 0);
    // No rate means no estimate. Saying "a very long time" is true and useless;
    // saying nothing at all is at least not a guess dressed as a fact.
    /* An estimate is only worth quoting while it means something.
     *
     * A trickle of a few hundred bytes a second is arithmetically a rate, and
     * dividing by it produced "about 7551 hours left" on screen, which is true,
     * useless, and reads as the page having broken. Past a day the honest thing
     * is to say it is barely moving and let somebody decide, so the threshold
     * is a day rather than a number chosen to look tidy. */
    const seconds = rate > 0 ? left / rate : null;
    const usable = seconds !== null && seconds <= 24 * 3600;
    const percent = Math.floor(fraction * 100);
    return {
      stage: "downloading",
      fraction,
      seconds: usable ? seconds : null,
      label: usable
        ? `${percent}%, ${say(seconds)} left`
        : seconds === null
          ? `${percent}%, looking for peers`
          : `${percent}%, barely moving`,
    };
  }

  if (item.preparing !== null && item.preparing !== undefined) {
    return {
      stage: "preparing",
      fraction: item.preparing,
      seconds: null,
      label: `downloaded, preparing ${Math.floor(item.preparing * 100)}%`,
    };
  }

  if (item.ready) {
    return { stage: "ready", fraction: 1, seconds: 0, label: "ready" };
  }

  return { stage: "playable", fraction: 1, seconds: 0, label: "downloaded" };
}

const BalerionLib = {
  bytes,
  seconds,
  roughly,
  episodeTagOf,
  seriesOf,
  feasible,
  shouldChangeVerdict,
  mediaUrl,
  stateOf,
};

// A browser: hang it on the window for app.js to use, and nothing else.
if (typeof window !== "undefined") {
  window.BalerionLib = BalerionLib;
}
// Node, for the tests.
if (typeof module !== "undefined" && module.exports) {
  module.exports = BalerionLib;
}
})();
