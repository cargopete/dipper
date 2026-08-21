/* Tests for the page's arithmetic. Run with `node --test`.
 *
 * No browser, no DOM, no server. These are the functions where the subtle bugs
 * live and they were the ones with no way to run them at all.
 */

const test = require("node:test");
const assert = require("node:assert/strict");

const { bytes, seconds, roughly, episodeTagOf, feasible, shouldChangeVerdict } =
  require("./lib.js");
const lib = require("./lib.js");
const { describe, it } = test;

test("byte counts read the way a person would say them", () => {
  assert.equal(bytes(0), "0 B");
  assert.equal(bytes(512), "512 B");
  assert.equal(bytes(1024), "1.0 KiB");
  assert.equal(bytes(1024 * 1024 * 1.5), "1.5 MiB");
  // Above ten, the decimal is noise.
  assert.equal(bytes(1024 * 1024 * 512), "512 MiB");
  assert.equal(bytes(1024 ** 3 * 2.7), "2.7 GiB");
});

test("nonsense byte counts are zero rather than NaN on the screen", () => {
  // A stats poll that arrives before anything has been measured hands back
  // undefined, and "NaN B" in a telemetry panel looks like a broken program.
  assert.equal(bytes(undefined), "0 B");
  assert.equal(bytes(NaN), "0 B");
  assert.equal(bytes(-5), "0 B");
});

test("the largest unit is used rather than running off the end of the table", () => {
  // A petabyte of anything is not going to happen here, but the loop bound is
  // one character away from indexing past the units.
  assert.ok(bytes(1024 ** 6).endsWith("TiB"), bytes(1024 ** 6));
});

test("durations switch to minutes where a count of seconds stops helping", () => {
  assert.equal(seconds(0), "0 s");
  assert.equal(seconds(42), "42 s");
  assert.equal(seconds(89), "89 s");
  assert.equal(seconds(90), "1m 30s");
  assert.equal(seconds(3671), "61m 11s");
});

test("a duration that is not a number is zero rather than NaN", () => {
  // `video.duration` is NaN until metadata arrives, which is exactly when a
  // page is most likely to ask.
  assert.equal(seconds(NaN), "0 s");
  assert.equal(seconds(Infinity), "0 s");
  assert.equal(seconds(-10), "0 s");
});

test("a rough estimate claims no more precision than it has", () => {
  assert.equal(roughly(30), "under a minute");
  assert.equal(roughly(600), "about 10 minutes");
  assert.equal(roughly(4 * 60), "about 4 minutes");
  // Rounded to five, because this is a guess and "about 34 minutes" pretends
  // otherwise.
  assert.equal(roughly(34 * 60), "about 35 minutes");
  assert.equal(roughly(3600), "about an hour");
  assert.equal(roughly(3 * 3600), "about 3 hours");
  assert.equal(roughly(Infinity), "a very long time");
});

test("a rough estimate is grammatical at one minute", () => {
  // The old guard produced "about 1 minutes" for anything between ninety
  // seconds and two and a half. Small, and the sort of thing a viewer reads
  // as the whole program being slapdash.
  assert.equal(roughly(100), "about 2 minutes");
  assert.equal(roughly(70), "under a minute");
  assert.equal(roughly(95), "about 2 minutes");
  assert.ok(!roughly(100).includes("1 minutes"));
});

test("under ten minutes the exact figure is kept", () => {
  // Rounding four minutes to five is a fifth of the answer thrown away for
  // no reason.
  assert.equal(roughly(4 * 60), "about 4 minutes");
  assert.equal(roughly(7 * 60), "about 7 minutes");
});

test("episode numbers are read out of the names that actually occur", () => {
  assert.equal(episodeTagOf("Show.S01E02.1080p.mkv"), "S01E02");
  assert.equal(episodeTagOf("Show s1e2 720p.mkv"), "S01E02");
  assert.equal(episodeTagOf("Show 1x02.avi"), "S01E02");
  assert.equal(episodeTagOf("Better Call Saul S06E13.mkv"), "S06E13");
});

test("something that merely contains an s and an e is not an episode", () => {
  assert.equal(episodeTagOf("Nosferatu.mkv"), null);
  assert.equal(episodeTagOf("A Trip to the Moon.mp4"), null);
  // The trap: a year is four digits and `1920` is not season 19 episode 20.
  assert.equal(episodeTagOf("Film.1920.1080p.mkv"), null);
});

test("a resolution is not mistaken for an episode", () => {
  // `1080p` contains no x, but `1920x1080` does, and reading it as season 19
  // is the classic version of this bug.
  assert.equal(episodeTagOf("Film 1920x1080 h264.mkv"), null);
});

test("feasibility says nothing when there is nothing to say", () => {
  // Distinct from saying no. A page that shows "this will stall" because it
  // has not measured anything yet is worse than one that stays quiet.
  assert.equal(feasible(0, 100), null);
  assert.equal(feasible(100, 0), null);
  assert.equal(feasible(NaN, 100), null);
  assert.equal(feasible(100, NaN), null);
});

test("a rate that only just matches the bitrate is not enough", () => {
  // The margin is the point: a download exactly matching the bitrate stalls
  // constantly, because the average is where it spends half its time below.
  assert.equal(feasible(1000, 1000).ok, false);
  assert.equal(feasible(1000, 1100).ok, false);
  assert.equal(feasible(1000, 1500).ok, true);
});

test("a verdict does not flap when the rate hovers at the threshold", () => {
  // Nothing said yet: say something.
  assert.equal(shouldChangeVerdict(null, false, 0.5), true);
  // Same answer: nothing to do.
  assert.equal(shouldChangeVerdict(true, true, 1.4), false);
  // Barely changed: hold the old verdict rather than blinking.
  assert.equal(shouldChangeVerdict(true, false, 1.05), false);
  assert.equal(shouldChangeVerdict(false, true, 1.1), false);
  // Properly changed: say so.
  assert.equal(shouldChangeVerdict(true, false, 0.6), true);
  assert.equal(shouldChangeVerdict(false, true, 1.8), true);
});

/* The bug this exists to prevent: a phone handed a LAN address it cannot reach,
   fetching nothing and sitting at 0:00 with no error to show for it. */
describe("mediaUrl", () => {
  const CAST = "http://192.168.0.13:8081";

  it("uses the cast address only when the page came from loopback", () => {
    assert.equal(
      lib.mediaUrl("/api/play/abc/0/index.m3u8", CAST, "127.0.0.1"),
      "http://192.168.0.13:8081/api/play/abc/0/index.m3u8",
    );
    assert.equal(lib.mediaUrl("/api/play/abc/0/index.m3u8", CAST, "localhost"),
      "http://192.168.0.13:8081/api/play/abc/0/index.m3u8");
  });

  it("leaves the path alone when the page came from anywhere else", () => {
    // The tunnel, which is how a phone reaches an always-on machine.
    assert.equal(
      lib.mediaUrl("/api/play/abc/0/index.m3u8", CAST, "pepe-thinkpad.tailb0627.ts.net"),
      "/api/play/abc/0/index.m3u8",
    );
    // And a LAN address, which is reachable but is not necessarily the same
    // LAN as the cast server's.
    assert.equal(lib.mediaUrl("/stream/abc/0", CAST, "100.83.44.63"), "/stream/abc/0");
  });

  it("does not meddle with an address somebody has already decided", () => {
    assert.equal(
      lib.mediaUrl("http://elsewhere/x.m3u8", CAST, "127.0.0.1"),
      "http://elsewhere/x.m3u8",
    );
  });

  it("copes with no cast server and with nothing to play", () => {
    assert.equal(lib.mediaUrl("/stream/abc/0", null, "127.0.0.1"), "/stream/abc/0");
    assert.equal(lib.mediaUrl("", CAST, "127.0.0.1"), "");
    assert.equal(lib.mediaUrl(null, CAST, "127.0.0.1"), null);
  });
});
