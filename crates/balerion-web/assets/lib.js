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
 * already been declared" — which kills the entire script, leaves the page with
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

const BalerionLib = {
  bytes,
  seconds,
  roughly,
  episodeTagOf,
  feasible,
  shouldChangeVerdict,
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
