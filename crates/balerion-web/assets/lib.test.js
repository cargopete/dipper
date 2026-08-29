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


/* The bug this exists to prevent: a browser that says it might play HLS, is
   handed a playlist, cannot play it, and never fires an error - so the page
   shows a player that sits at 0:00 for ever with nothing to react to. */
describe("playsHlsNatively", () => {
  const SAFARI_MAC =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15";
  const SAFARI_IOS =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 26_6 like Mac OS X) AppleWebKit/605.1.15 Version/26.0 Mobile/15E148 Safari/604.1";
  const CHROME_MAC =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";
  const FIREFOX_MAC =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:154.0) Gecko/20100101 Firefox/154.0";
  const CHROME_IOS =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/140.0 Mobile/15E148 Safari/604.1";

  it("believes WebKit", () => {
    assert.equal(lib.playsHlsNatively(SAFARI_MAC, "maybe"), true);
    assert.equal(lib.playsHlsNatively(SAFARI_IOS, "maybe"), true);
    // Every iPhone browser is WebKit underneath, whatever the badge says.
    assert.equal(lib.playsHlsNatively(CHROME_IOS, "maybe"), true);
  });

  it("does not believe desktop Chrome, which says maybe and cannot", () => {
    assert.equal(lib.playsHlsNatively(CHROME_MAC, "maybe"), false);
  });

  it("does not believe a browser that did not even claim it", () => {
    // Firefox is honest and answers "", which settles it on its own.
    assert.equal(lib.playsHlsNatively(FIREFOX_MAC, ""), false);
    assert.equal(lib.playsHlsNatively(SAFARI_MAC, ""), false);
  });

  it("copes with no user agent at all", () => {
    assert.equal(lib.playsHlsNatively(undefined, "maybe"), false);
    assert.equal(lib.playsHlsNatively("", "maybe"), false);
  });
});

describe("isSafariAirplayBrowser", () => {
  it("keeps the receiver hand-off in Safari", () => {
    assert.equal(
      lib.isSafariAirplayBrowser(
        "Mozilla/5.0 (iPhone; CPU iPhone OS 26_6 like Mac OS X) AppleWebKit/605.1.15 Version/26.0 Mobile/15E148 Safari/604.1",
      ),
      true,
    );
  });

  it("does not mistake iPhone Firefox for Safari", () => {
    assert.equal(
      lib.isSafariAirplayBrowser(
        "Mozilla/5.0 (iPhone; CPU iPhone OS 26_6 like Mac OS X) AppleWebKit/605.1.15 FxiOS/146.0 Mobile/15E148 Safari/605.1.15",
      ),
      false,
    );
  });
});

/* The false accusation this replaces: two different programmes whose first
   episodes share a number, reported as duplicates, with an offer to delete
   one of them. */
describe("seriesOf", () => {
  it("tells two programmes apart by more than the episode number", () => {
    const saul = lib.seriesOf("www.UIndex.org    -    Better Call Saul S01E01 Uno 1080p WEB-DL");
    const bad = lib.seriesOf("www.UIndex.org - Breaking.Bad.S01E01.Pilot.720p.HEVC.x265-MeGusta");
    assert.equal(saul.tag, "S01E01");
    assert.equal(bad.tag, "S01E01");
    assert.notEqual(saul.key, bad.key, "different shows must not share a key");
    assert.equal(saul.series, "better call saul");
    assert.equal(bad.series, "breaking bad");
  });

  it("still groups two releases of the same episode", () => {
    // Which is the case the warning exists for, and it must survive the fix.
    const a = lib.seriesOf("www.UIndex.org    -    Better Call Saul S01E01 Uno 1080p WEB-DL");
    const b = lib.seriesOf("Better.Call.Saul.S01E01.iNTERNAL.1080p.WEB.x264-GROUP");
    assert.equal(a.key, b.key);
  });

  it("reads the other way of writing an episode number", () => {
    assert.deepEqual(lib.seriesOf("The Wire 1x03 something"), {
      series: "the wire",
      tag: "S01E03",
      key: "the wire|S01E03",
    });
  });

  it("says nothing about a name with no episode in it", () => {
    assert.equal(lib.seriesOf("0707_Atomic_Bomb_Blast_Effects"), null);
    assert.equal(lib.seriesOf("Some Film 2024 1080p"), null);
  });
});

/* The download-first path lives or dies on this row of text: somebody looks at
   it and decides whether to wait. */
describe("stateOf", () => {
  const base = {
    total_length: 1000,
    bytes_on_disk: 0,
    pieces_have: 0,
    pieces_total: 100,
    rate: 0,
    complete: false,
    preparing: null,
    ready: false,
  };

  it("says how long is left, when there is a rate to say it from", () => {
    const got = lib.stateOf({ ...base, pieces_have: 25, bytes_on_disk: 250, rate: 10 });
    assert.equal(got.stage, "downloading");
    assert.equal(got.fraction, 0.25);
    assert.equal(got.seconds, 75);
    assert.match(got.label, /^25%, .* left$/);
  });

  it("will not quote an estimate that means nothing", () => {
    // 7551 hours was on screen. True, useless, and reads as a broken page.
    const got = lib.stateOf({ ...base, total_length: 800e6, bytes_on_disk: 0, pieces_have: 0, rate: 30 });
    assert.equal(got.seconds, null, "not offered as a number anybody should act on");
    assert.equal(got.label, "0%, barely moving");
  });

  it("still quotes an estimate right up to the threshold", () => {
    // A day is slow and worth waiting out; the figure should survive.
    const got = lib.stateOf({ ...base, total_length: 86400, bytes_on_disk: 0, pieces_have: 0, rate: 1 });
    assert.equal(got.seconds, 86400);
    assert.match(got.label, /left$/);
  });

  it("refuses to guess when nothing is arriving", () => {
    // A seeder count is not a rate, and an estimate made from one is a lie
    // that looks like a fact.
    const got = lib.stateOf({ ...base, pieces_have: 25, bytes_on_disk: 250, rate: 0 });
    assert.equal(got.seconds, null);
    assert.equal(got.label, "25%, looking for peers");
  });

  it("moves to preparing once it is all here", () => {
    const got = lib.stateOf({ ...base, complete: true, bytes_on_disk: 1000, preparing: 0.6 });
    assert.equal(got.stage, "preparing");
    assert.equal(got.label, "downloaded, preparing 60%");
  });

  it("calls it ready only when it has been converted", () => {
    const got = lib.stateOf({ ...base, complete: true, ready: true });
    assert.equal(got.stage, "ready");
    assert.equal(got.label, "ready");
  });

  it("something needing no conversion is downloaded rather than ready", () => {
    // An MP4 browsers already open is finished without anything being done to
    // it, and claiming it was prepared would be untrue.
    const got = lib.stateOf({ ...base, complete: true, ready: false, preparing: null });
    assert.equal(got.stage, "playable");
    assert.equal(got.label, "downloaded");
  });

  it("copes with a torrent nothing is known about yet", () => {
    const got = lib.stateOf({ ...base, pieces_total: 0, total_length: 0 });
    assert.equal(got.fraction, 0);
    assert.equal(got.seconds, null);
  });

  it("never reports more held than there is", () => {
    // Resume counts bytes from an earlier run and can overshoot slightly.
    const got = lib.stateOf({ ...base, bytes_on_disk: 1200, pieces_have: 100, rate: 10 });
    assert.equal(got.seconds, 0);
  });
});
