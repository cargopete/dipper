"use client";

import { useCallback, useEffect, useState } from "react";

type Option = { key: string; label: string; note: string; thinCap: number | null };

type Show = {
  id: number;
  name: string;
  premiered: string | null;
  ended: string | null;
  status: string | null;
  genres: string[];
  summary: string | null;
  poster: string | null;
};

type Episode = {
  id: number;
  season: number;
  number: number;
  name: string;
  airdate: string | null;
  runtime: number | null;
  tag: string;
};

type Pick = {
  magnet: string;
  alternatives: string[];
  name: string;
  sizeBytes: number;
  seeders: number;
  rate: string;
  overBudget: boolean;
  considered: number;
};

/* Punctuation dropped rather than escaped, the same rule the server uses:
   release names put dots where the title had spaces, and a colon in
   "Dune: Prophecy" matches nothing at all. */
function apibayQuery(show: Show, suffix: string): string {
  const title = show.name.replace(/[^\p{L}\p{N}]+/gu, " ").trim();
  return `${title} ${suffix}`;
}

type IndexInfo = {
  key: "ia" | "tpb";
  label: string;
  reachable: boolean;
  filterLabel: string;
  options: Option[];
  note: string | null;
};

/* One shape per index rather than a branch in five places, the same as the local
 * player. An index's entry says how to read a hit and what to do with it. */
type Row = {
  key: string;
  title: string;
  meta: (string | null)[];
  sizeBytes: number | null;
  /** Leechers are nullable because not every index reports them, and
   * printing a zero for one that does not makes a healthy swarm look dead. */
  swarm: { seeders: number; leechers: number | null } | null;
  /** What gets copied: a magnet, or an archive.org identifier. */
  copy: string | null;
  /** Where to read more about it, when the index offers such a place. */
  href: string | null;
};

/** Where a local Balerion is expected to be listening. */
const DEFAULT_LOCAL = "http://127.0.0.1:8080";

/**
 * A link that opens whatever Balerion is running on the machine you are reading
 * this on, with the thing already chosen.
 *
 * A top-level navigation from an https page to 127.0.0.1 is allowed, which is
 * what makes this possible at all; a fetch to the same address would not be.
 * The magnet goes in the fragment rather than the query so it never leaves the
 * browser: not into that server's log, not into a proxy's, not into a Referer.
 */
function watchHere(
  base: string,
  open: string,
  alternatives: string[] = [],
  { keep = false }: { keep?: boolean } = {},
): string {
  /* Guarded, because an unusable base produces a link that looks fine and does
     something quite different. With an empty base this built "/#magnet=...",
     which is a fragment on *this* page: clicking it scrolls to the top and
     nothing else happens, and there is nothing on screen to say why. */
  const trimmed = base.trim().replace(/\/+$/, "");
  const usable = /^https?:\/\/[^/]+$/.test(trimmed) ? trimmed : DEFAULT_LOCAL;
  const alt = alternatives.map((m) => `&alt=${encodeURIComponent(m)}`).join("");
  /* The persistent path. Watching borrows a copy and lets Balerion's sweep take
     it back once nobody is looking; keeping fetches the whole thing and holds it
     until it is deleted by hand. Same link, one flag apart, because they are the
     same journey with a different ending. */
  const intent = keep ? "&keep=1" : "";
  return `${usable}/#magnet=${encodeURIComponent(open)}${alt}${intent}`;
}

/* Claim a window while the tap is still a tap, and put something in it.
 *
 * An empty window is only marginally better than no window: it reads as a page
 * that failed to load. It gets a line of text in the same palette, so the
 * seconds spent asking apibay which release to fetch look like waiting rather
 * than like breakage. */
function openWaitingWindow(): Window | null {
  let claimed: Window | null = null;
  try {
    claimed = window.open("", "balerion");
  } catch {
    return null;
  }
  try {
    claimed?.document.write(
      '<!doctype html><meta charset="utf-8">' +
        '<meta name="viewport" content="width=device-width, initial-scale=1">' +
        "<title>Balerion</title>" +
        '<body style="margin:0;display:grid;place-items:center;min-height:100vh;' +
        "background:#171614;color:#9c978c;font:15px/1.5 ui-sans-serif,system-ui,sans-serif\">" +
        '<p style="font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.7rem;' +
        'letter-spacing:.14em;text-transform:uppercase;color:#cd8560">Finding a release</p>',
    );
  } catch {
    // Cross-origin or a browser that will not have it written into. The window
    // is still claimed, which is the part that matters.
  }
  return claimed;
}

/* Send the claimed window somewhere, or fall back to this one.
 *
 * The fallback matters. If the window was blocked anyway, or the browser has
 * no notion of named windows, navigating here is worse than a new tab but
 * infinitely better than the button doing nothing, which is what it did. */
function handOver(url: string, waiting: Window | null) {
  if (waiting && !waiting.closed) {
    waiting.location.href = url;
    return;
  }
  window.location.href = url;
}

/** The shelf of what has been kept, which only Balerion itself knows about. */
function shelfHere(base: string): string {
  const trimmed = base.trim().replace(/\/+$/, "");
  const usable = /^https?:\/\/[^/]+$/.test(trimmed) ? trimmed : DEFAULT_LOCAL;
  return `${usable}/#downloaded`;
}

/* A synopsis cut to length without cutting a word in half.
 *
 * A hard `slice` left Better Call Saul's description ending "hustling to make
 * ends meet. Wor", which reads as a bug rather than as an abridgement. Back up
 * to the last space and say plainly that there is more. */
function trimmed(text: string, limit: number): string {
  const clean = text.trim();
  if (clean.length <= limit) return clean;
  const cut = clean.slice(0, limit);
  const space = cut.lastIndexOf(" ");
  // No space at all in 320 characters is not a sentence; leave it be rather
  // than returning nothing.
  const kept = space > limit * 0.6 ? cut.slice(0, space) : cut;
  return `${kept.replace(/[\s.,;:]+$/, "")}\u2026`;
}

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

function bytes(n: number | null): string {
  if (!n) return "";
  let size = n;
  let unit = 0;
  while (size >= 1024 && unit < UNITS.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size < 10 && unit > 0 ? size.toFixed(1) : Math.round(size)} ${UNITS[unit]}`;
}

/** Under about ten seeders a stream will not keep ahead of the playhead. */
const THIN_SWARM = 10;

function toRows(index: string, data: Record<string, unknown>): Row[] {
  const hits = (data.hits ?? []) as Record<string, never>[];

  /* Anything that came through the relay's seam. One shape for every index
   * behind it, which is the point: the rules about what a search means live in
   * Rust and this only has to draw the answer. */
  if (index !== "ia" && index !== "tpb") {
    return hits.map((hit, at) => ({
      key: String(hit.info_hash ?? `${index}-${at}`),
      title: String(hit.title),
      meta: [
        // Which index found it, first: with several configured that is most of
        // what makes a mixed list legible.
        (hit.sources as unknown as string[]).join(" + "),
        hit.detail ? String(hit.detail) : null,
      ],
      sizeBytes: hit.size ? Number(hit.size) : null,
      /* Seeders only. `/find` reports the one figure every index agrees to
       * give, and inventing a zero for leechers would make a healthy swarm
       * look dead. */
      swarm:
        hit.seeders === undefined ? null : { seeders: Number(hit.seeders), leechers: null },
      copy: String(hit.open),
      href: null,
    }));
  }

  if (index === "tpb") {
    return hits.map((hit) => ({
      key: String(hit.id),
      title: String(hit.name),
      meta: [
        String(hit.categoryLabel),
        Number(hit.numFiles) > 1 ? `${hit.numFiles} files` : null,
        new Date(Number(hit.added) * 1000).toISOString().slice(0, 10),
        String(hit.username),
      ],
      sizeBytes: Number(hit.sizeBytes),
      swarm: { seeders: Number(hit.seeders), leechers: Number(hit.leechers) },
      copy: String(hit.magnet),
      href: null,
    }));
  }
  return hits.map((hit) => ({
    key: String(hit.identifier),
    title: String(hit.title),
    meta: [
      hit.creator ? String(hit.creator) : null,
      hit.year ? String(hit.year) : null,
      String(hit.identifier),
    ],
    sizeBytes: hit.sizeBytes ? Number(hit.sizeBytes) : null,
    swarm: null,
    /* The Archive needs no magnet: Balerion turns an identifier into a torrent
     * over HTTPS, which is what makes those items work at all. So the identifier
     * is the thing to copy. */
    copy: String(hit.identifier),
    href: String(hit.detailsUrl),
  }));
}

export default function Page() {
  const [indexes, setIndexes] = useState<IndexInfo[]>([]);
  /* apibay first: it is what this is actually used for. The Archive is the
     safer index and the second choice, which is a different claim from being the
     default one. */
  const [index, setIndex] = useState<string>("tpb");
  const [filter, setFilter] = useState("video");
  const [terms, setTerms] = useState("");
  const [thin, setThin] = useState(false);
  const [rows, setRows] = useState<Row[] | null>(null);
  const [count, setCount] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  /* The catalogue. Browsing is a separate mode rather than a third index,
     because what it lists is titles rather than things you can fetch: a show is
     not a torrent until an episode of it has been searched for. */
  const [browsing, setBrowsing] = useState(true);
  const [shows, setShows] = useState<Show[] | null>(null);
  const [showTerms, setShowTerms] = useState("");
  const [openShow, setOpenShow] = useState<Show | null>(null);
  const [episodeList, setEpisodeList] = useState<Episode[] | null>(null);
  const [season, setSeason] = useState(1);
  /* Pick and play: the episode being fetched, and what was chosen for it. Kept
     visible rather than silent, because something that chooses for you and will
     not say what it chose is worse than a list. */
  const [picking, setPicking] = useState<number | null>(null);
  /* Which of the two buttons started the pick, so the note underneath says
     what is about to happen rather than guessing. */
  const [keeping, setKeeping] = useState(false);
  const [picked, setPicked] = useState<{ episode: Episode; choice: Pick } | null>(null);
  /* Remembered per browser rather than configured on the server: it describes
     the machine you are sitting at, and the server has no business knowing it. */
  const [local, setLocal] = useState(DEFAULT_LOCAL);
  const [showLocal, setShowLocal] = useState(false);

  /* The deployment's own answer wins over the built-in default, and a value set
     in this browser wins over both. Most people should never touch the third:
     the point of the first is that one address serves every device. */
  useEffect(() => {
    const saved = window.localStorage.getItem("balerion.local");
    if (saved) setLocal(saved);
  }, []);

  function rememberLocal(value: string) {
    setLocal(value);
    /* A blank or half-typed value is not worth remembering, and remembering one
       would outlive the moment it was typed in. */
    if (/^https?:\/\/[^/]+$/.test(value.trim())) {
      window.localStorage.setItem("balerion.local", value.trim());
    } else {
      window.localStorage.removeItem("balerion.local");
    }
  }

  const chosenIndex = indexes.find((entry) => entry.key === index);
  const chosenOption = chosenIndex?.options.find((option) => option.key === filter);

  useEffect(() => {
    fetch("/api/search?catalogue")
      .then((response) => (response.ok ? response.json() : Promise.reject(new Error())))
      .then((body: { indexes: IndexInfo[]; local: string | null }) => {
        setIndexes(body.indexes);
        // Only when this browser has not been given one of its own.
        if (body.local && !window.localStorage.getItem("balerion.local")) {
          setLocal(body.local);
        }
      })
      .catch(() => setError("Could not load the indexes. Nothing will search until that does."));
  }, []);

  /* The indexes that live on the relay's machine, asked for separately and
     appended when they arrive.
     
     Separate because this one can be slow or never answer: it depends on a
     laptop being awake, and the menu must not. Silent on failure for the same
     reason: a machine being asleep is not an error worth putting in front of
     somebody, it just means there is less to choose from. */
  useEffect(() => {
    fetch("/api/search?sources")
      .then((response) => (response.ok ? response.json() : Promise.reject(new Error())))
      .then((body: { indexes: IndexInfo[] }) => {
        if (!body.indexes?.length) return;
        setIndexes((held) => [
          ...held,
          ...body.indexes.filter((extra) => !held.some((one) => one.key === extra.key)),
        ]);
      })
      .catch(() => {});
  }, []);

  /* The opening shelf, once. A hand-picked list rather than a chart, because
     TVmaze has no popularity endpoint and inventing a ranking would be a lie
     told in a nice grid. */
  useEffect(() => {
    if (!browsing || shows !== null || showTerms) return;
    fetch("/api/shows")
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error())))
      .then((body: { shows: Show[] }) => setShows(body.shows))
      .catch(() => setError("The catalogue is not answering."));
  }, [browsing, shows, showTerms]);

  async function findShows(event?: React.FormEvent) {
    event?.preventDefault();
    setError(null);
    setOpenShow(null);
    setEpisodeList(null);
    try {
      const query = showTerms.trim() ? `?q=${encodeURIComponent(showTerms.trim())}` : "";
      const response = await fetch(`/api/shows${query}`);
      const body = await response.json();
      if (!response.ok) throw new Error(body?.error ?? `${response.status}`);
      setShows(body.shows);
    } catch (err) {
      setError(err instanceof Error ? err.message : "could not reach the catalogue");
    }
  }

  async function openShowDetail(show: Show) {
    setOpenShow(show);
    setEpisodeList(null);
    setRows(null);
    setError(null);
    try {
      const response = await fetch(`/api/shows?show=${show.id}`);
      const body = await response.json();
      if (!response.ok) throw new Error(body?.error ?? `${response.status}`);
      setEpisodeList(body.episodes);
      setSeason(body.episodes[0]?.season ?? 1);
    } catch (err) {
      setError(err instanceof Error ? err.message : "could not read that show");
    }
  }

  /* Pick and play. One tap: search, choose, and hand the magnet straight to the
     player, without anyone reading a list of releases.
     
     The choosing is arithmetic rather than taste: a release streams if the swarm
     can carry its bitrate, and its bitrate is its size over the episode's
     runtime, which the catalogue knows. */
  async function playEpisode(episode: Episode, { keep = false } = {}) {
    if (!openShow) return;
    setKeeping(keep);
    setPicking(episode.id);
    setPicked(null);
    setError(null);
    /* Claimed now, while the click is still a click.
     *
     * Which release to fetch is not known until apibay has been asked, and by
     * the time that answers the user gesture has expired. A browser will not
     * open a window for code that is no longer plainly acting on a tap, and a
     * phone will not even say so: the button flickers, nothing opens, and
     * nothing arrives. Opening it empty first and pointing it somewhere later
     * is the only version of this that works on a phone. */
    const waiting = openWaitingWindow();
    try {
      const query = new URLSearchParams({
        q: apibayQuery(openShow, episode.tag),
        runtime: String(episode.runtime ?? 45),
      });
      const response = await fetch(`/api/pick?${query}`);
      const body = await response.json();
      if (!response.ok) throw new Error(body?.error ?? `${response.status}`);
      setPicked({ episode, choice: body as Pick });
      /* Straight to the player, which resolves and starts on its own. The
         same journey either way: `keep` is the difference between borrowing a
         copy and being given one. */
      handOver(watchHere(local, body.magnet, body.alternatives ?? [], { keep }), waiting);
    } catch (err) {
      waiting?.close();
      setError(err instanceof Error ? err.message : "could not find anything to play");
    } finally {
      setPicking(null);
    }
  }

  /* The bridge: a title becomes a query, and from here on it is the ordinary
     apibay search with everything that already does. Season packs are searched
     for separately because they are named quite differently from episodes. */
  function watchQuery(suffix: string) {
    if (!openShow) return;
    const query = apibayQuery(openShow, suffix);
    setTerms(query);
    setIndex("tpb");
    setBrowsing(false);
    void runWith(query, "tpb");
  }

  /* Takes its terms rather than reading them from state, because the catalogue
     calls it in the same tick as setting them and state has not caught up. That
     was a search for the empty string the first time round. */
  const runWith = useCallback(
    async (searchFor: string, forIndex: string = index) => {
      const entry = indexes.find((i) => i.key === forIndex);
      if (!entry) return;
      // The Archive browses on an empty query; apibay has no browse.
      // Only the Archive has a browse: everything else answers an empty
      // query with a sentinel that reads as "nothing matched", which is not
      // what happened.
      if (!searchFor.trim() && forIndex !== "ia") return;
      const terms = searchFor;
      const index = forIndex;

      setBusy(true);
      setError(null);
      try {
        const query = new URLSearchParams({
          q: terms.trim(),
          index,
          filter,
          limit: "24",
          thin: thin ? "true" : "false",
        });
        const response = await fetch(`/api/search?${query}`);
        const body = await response.json();
        if (!response.ok) throw new Error(body?.error ?? `${response.status}`);

        const produced = toRows(index, body);
        setRows(produced);

        /* The seam reports no grand total, because there is no such number
         * once several indexes have been asked and their duplicates folded
         * together. It reports what it did instead, which is more use: how
         * many rows were the same release twice, and which index failed to
         * answer. A short list is otherwise indistinguishable from a thorough
         * search that found little. */
        const shown = produced.length
          ? body.total === undefined
            ? `showing ${produced.length}`
            : `showing ${produced.length} of ${body.total}`
          : "nothing to show";
        const hidden = [
          body.oversize ? `${body.oversize} too large` : null,
          body.unseeded ? `${body.unseeded} unseeded` : null,
          body.duplicates ? `${body.duplicates} the same release twice` : null,
          Array.isArray(body.failed) && body.failed.length
            ? `${(body.failed as string[]).join(", ")} did not answer`
            : null,
        ].filter(Boolean);
        setCount(hidden.length ? `${shown}, ${hidden.join(" and ")}` : shown);
      } catch (err) {
        setRows(null);
        setCount("");
        setError(err instanceof Error ? err.message : "the search failed");
      } finally {
        setBusy(false);
      }
    },
    [filter, index, indexes, thin],
  );

  const run = useCallback(
    async (event?: React.FormEvent) => {
      event?.preventDefault();
      await runWith(terms, index);
    },
    [runWith, terms, index],
  );

  /* Changing the index invalidates what is on screen: those results belong to
   * the other one, and leaving them under a new menu invites a wrong click. */
  function switchIndex(next: string) {
    const entry = indexes.find((i) => i.key === next);
    setIndex(next);
    setFilter(entry?.options[0]?.key ?? "");
    setRows(null);
    setCount("");
    setError(null);
  }

  async function copy(row: Row) {
    if (!row.copy) return;
    try {
      await navigator.clipboard.writeText(row.copy);
      setCopied(row.key);
      window.setTimeout(() => setCopied((was) => (was === row.key ? null : was)), 1600);
    } catch {
      setError("The browser would not let the page write to the clipboard.");
    }
  }

  /* What the button actually puts on the clipboard, read off the row rather
   * than off the index. The Archive needs no magnet: balerion turns an
   * identifier into a torrent over HTTPS, which is what makes those items work
   * at all. Every other index hands over a magnet.
   *
   * Per row because "every index" mixes them in one list, and labelling a
   * magnet "identifier" is a small lie that makes somebody paste the wrong
   * thing somewhere else. */
  const copyLabel = (row: Row) =>
    row.copy?.startsWith("magnet:") ? "Copy magnet" : "Copy identifier";

  return (
    <>
      <header className="masthead">
        <p className="brand">BALERION</p>
        <p className="masthead-note">search only</p>
        {/* Not a third tab: the other two switch panels on this page, and this
            one leaves it. Everything downloaded lives on the machine running
            Balerion, which is the only thing that knows what is on the shelf. */}
        <a className="masthead-link" href={shelfHere(local)} target="balerion">
          Downloaded
        </a>
      </header>

      <main>
        <div className="modes" role="tablist" aria-label="How to find something">
          <button
            type="button"
            role="tab"
            aria-selected={browsing ? "true" : "false"}
            onClick={() => setBrowsing(true)}
          >
            Browse shows
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={browsing ? "false" : "true"}
            onClick={() => setBrowsing(false)}
          >
            Search
          </button>
        </div>

        {browsing ? (
          <>
            <section className="console">
              <form onSubmit={findShows}>
                <div className="search-row browse-row">
                  <div className="field">
                    <label htmlFor="showq">Find a show</label>
                    <input
                      id="showq"
                      type="search"
                      spellCheck={false}
                      autoComplete="off"
                      placeholder="a title, or leave it empty for the shelf"
                      value={showTerms}
                      onChange={(event) => setShowTerms(event.target.value)}
                    />
                  </div>
                  <button type="submit">Find</button>
                </div>
              </form>
              <p className="hint">
                Titles come from TVmaze. Picking an episode searches apibay for it, which is
                where everything below this point already goes.
              </p>
            </section>

            {openShow ? (
              <section className="show-detail">
                {openShow.poster ? (
                  <img src={openShow.poster} alt="" />
                ) : (
                  <div className="show-poster missing">no poster</div>
                )}
                <div>
                  <h2>{openShow.name}</h2>
                  <p className="show-meta">
                    {[
                      openShow.premiered?.slice(0, 4),
                      openShow.status,
                      openShow.genres.join(", "),
                    ]
                      .filter(Boolean)
                      .join("  /  ")}
                  </p>
                  {openShow.summary ? (
                    <p className="show-summary">{trimmed(openShow.summary, 320)}</p>
                  ) : null}

                  {episodeList === null ? (
                    <p className="hint">Reading the episode list</p>
                  ) : (
                    <>
                      <div className="season-row">
                        {[...new Set(episodeList.map((e) => e.season))].map((n) => (
                          <button
                            key={n}
                            type="button"
                            className={n === season ? "button-small" : "button-small quiet"}
                            onClick={() => setSeason(n)}
                          >
                            Season {n}
                          </button>
                        ))}
                        {/* Packs are named quite differently from episodes, so
                            they get their own search rather than being hoped for. */}
                        <button
                          type="button"
                          className="button-small quiet"
                          onClick={() => watchQuery(`season ${season}`)}
                        >
                          Whole season
                        </button>
                      </div>

                      {picked ? (
                        <p className={picked.choice.overBudget ? "hint hint-caution" : "hint"}>
                          <strong>{picked.episode.tag}</strong>: picked{" "}
                          <code>{picked.choice.name}</code> from {picked.choice.considered}{" "}
                          releases. {bytes(picked.choice.sizeBytes)}, {picked.choice.seeders}{" "}
                          seeders, needs {picked.choice.rate}.
                          {picked.choice.overBudget
                            ? " Nothing found fits a thin line, so this is the smallest there was and it may stall."
                            : keeping
                              ? " Downloading it and keeping it."
                              : " Opening it in the player."}
                        </p>
                      ) : null}

                      <ul className="episodes">
                        {episodeList
                          .filter((e) => e.season === season)
                          .map((e) => (
                            <li key={e.id}>
                              <span className="episode-tag">{e.tag}</span>
                              <span>{e.name}</span>
                              <span className="episode-date">{e.airdate ?? ""}</span>
                              <span className="row-actions">
                                <button
                                  type="button"
                                  className="button-small"
                                  disabled={picking === e.id}
                                  onClick={() => playEpisode(e)}
                                >
                                  {picking === e.id ? "Finding" : "Play"}
                                </button>
                                {/* The persistent half of the pair, same as on
                                    a results row: pick the best release, fetch
                                    all of it, and keep it. */}
                                <button
                                  type="button"
                                  className="button-small quiet"
                                  disabled={picking === e.id}
                                  onClick={() => playEpisode(e, { keep: true })}
                                >
                                  Download
                                </button>
                                {/* For when you want to choose yourself. */}
                                <button
                                  type="button"
                                  className="button-small quiet"
                                  onClick={() => watchQuery(e.tag)}
                                >
                                  Releases
                                </button>
                              </span>
                            </li>
                          ))}
                      </ul>
                    </>
                  )}
                </div>
              </section>
            ) : null}

            {shows && !openShow ? (
              <section aria-label="Shows">
                <ul className="shows">
                  {shows.map((show) => (
                    <li key={show.id}>
                      <button type="button" className="show" onClick={() => openShowDetail(show)}>
                        {show.poster ? (
                          <img className="show-poster" src={show.poster} alt="" loading="lazy" />
                        ) : (
                          <span className="show-poster missing">{show.name}</span>
                        )}
                        <span className="show-name">{show.name}</span>
                        <span className="show-meta">
                          {[show.premiered?.slice(0, 4), show.genres[0]]
                            .filter(Boolean)
                            .join("  /  ")}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              </section>
            ) : null}
          </>
        ) : null}

        <section className="console" hidden={browsing}>
          <form onSubmit={run}>
            <div className="search-row">
              <div className="field">
                <label htmlFor="q">Search for</label>
                <input
                  id="q"
                  type="search"
                  spellCheck={false}
                  autoComplete="off"
                  placeholder="a film, a series, an episode"
                  value={terms}
                  onChange={(event) => setTerms(event.target.value)}
                />
              </div>
              <div className="field">
                <label htmlFor="index">Index</label>
                <select
                  id="index"
                  value={index}
                  onChange={(event) => switchIndex(event.target.value as "ia" | "tpb")}
                >
                  {indexes.map((entry) => (
                    <option key={entry.key} value={entry.key}>
                      {entry.label}
                    </option>
                  ))}
                </select>
              </div>
              {/* Hidden for an index with no subsets. A Torznab indexer has
                  categories, but which ones depends on what is behind it, and
                  an empty menu is worse than none. */}
              <div className="field" hidden={!chosenIndex?.filterLabel}>
                <label htmlFor="filter">{chosenIndex?.filterLabel ?? "Collection"}</label>
                <select
                  id="filter"
                  value={filter}
                  onChange={(event) => setFilter(event.target.value)}
                >
                  {(chosenIndex?.options ?? []).map((option) => (
                    <option key={option.key} value={option.key}>
                      {option.label}
                    </option>
                  ))}
                </select>
              </div>
              <button type="submit" disabled={busy || (index === "tpb" && !terms.trim())}>
                {busy ? "Searching" : "Search"}
              </button>
            </div>

            {chosenOption?.thinCap ? (
              <label className="toggle" htmlFor="thin">
                <input
                  id="thin"
                  type="checkbox"
                  checked={thin}
                  onChange={(event) => setThin(event.target.checked)}
                />
                <span>Fits a thin line (under {bytes(chosenOption.thinCap)})</span>
              </label>
            ) : null}
          </form>

          {chosenOption ? <p className="hint">{chosenOption.note}</p> : null}
          {chosenIndex?.note ? (
            <p className="hint hint-caution">{chosenIndex.note}</p>
          ) : null}
          {chosenIndex && !chosenIndex.reachable ? (
            <p className="hint hint-caution">
              This index is searched through the Balerion relay on your own machine, and this
              deployment has not been told where that is. Searches here will fail until it has.
            </p>
          ) : null}
        </section>

        {error ? (
          <section className="notice" role="alert">
            <p className="notice-title">That did not work</p>
            <p>{error}</p>
          </section>
        ) : null}

        {busy && !rows ? (
          <section aria-busy="true" aria-label="Searching">
            {[0, 1, 2, 3, 4].map((row) => (
              <div className="skeleton-row" key={row} />
            ))}
          </section>
        ) : null}

        {rows ? (
          <section aria-labelledby="results-heading">
            <div className="results-head">
              <h2 id="results-heading">Results</h2>
              <p className="results-count">{count}</p>
            </div>

            {rows.length === 0 ? (
              <p className="empty">
                {index === "tpb" && thin
                  ? "Nothing here small enough for a thin line. The standard definition categories are where the small releases live; failing that, untick the box."
                  : "Nothing here matches that. Try fewer words, or a different collection."}
              </p>
            ) : (
              <ul className="results">
                {rows.map((row) => (
                  <li key={row.key}>
                    <div className="result-title">
                      {row.href ? (
                        <a href={row.href} target="_blank" rel="noreferrer noopener">
                          {row.title}
                        </a>
                      ) : (
                        row.title
                      )}
                      <span className="result-meta">
                        {row.meta.filter(Boolean).join("  /  ")}
                      </span>
                    </div>
                    <span
                      className="result-swarm"
                      title={
                        row.swarm
                          ? row.swarm.leechers === null
                            ? `${row.swarm.seeders} seeding; this index does not report leechers`
                            : `${row.swarm.seeders} seeding, ${row.swarm.leechers} leeching`
                          : undefined
                      }
                    >
                      {row.swarm ? (
                        <>
                          <span
                            className={
                              row.swarm.seeders < THIN_SWARM ? "seeders-thin" : "seeders"
                            }
                          >
                            {row.swarm.seeders}
                          </span>
                          {row.swarm.leechers === null ? null : ` / ${row.swarm.leechers}`}
                        </>
                      ) : null}
                    </span>
                    <span className="result-size">{bytes(row.sizeBytes)}</span>
                    <span className="row-actions">
                      {/* A named target so the first Watch opens a Balerion tab
                          and every one after it reuses that tab. Deliberately no
                          `rel`: per the HTML spec, `noopener` (which `noreferrer`
                          implies) makes the browser ignore the target name and
                          open a fresh context every time, which defeats the whole
                          point. The target is our own page on loopback, and the
                          magnet rides in the fragment, which is never sent as a
                          Referer, so there is nothing here for `rel` to protect. */}
                      <a
                        className="button-small"
                        href={watchHere(local, row.copy ?? "")}
                        target="balerion"
                        title={`Opens ${watchHere(local, row.copy ?? "")}`}
                      >
                        Watch
                      </a>
                      {/* Same tab as Watch, so queueing several downloads does
                          not leave a trail of Balerion windows behind. */}
                      <a
                        className="button-small quiet"
                        href={watchHere(local, row.copy ?? "", [], { keep: true })}
                        target="balerion"
                        title="Fetch the whole thing and keep it until you delete it"
                      >
                        Download
                      </a>
                      <button
                        type="button"
                        className={
                          copied === row.key ? "button-small quiet done" : "button-small quiet"
                        }
                        onClick={() => copy(row)}
                      >
                        {copied === row.key ? "Copied" : copyLabel(row)}
                      </button>
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </section>
        ) : null}

        <section className="handoff">
          <h2>Where the watching happens</h2>
          <p>
            This page only searches. Balerion&apos;s engine holds long-lived connections to
            dozens of peers, listens on a UDP port for the DHT and writes gigabytes to a real
            disk, none of which a serverless function can do. Paste what you copy into the
            Balerion running on your own machine: <code>balerion serve</code>.
          </p>
          <p style={{ marginTop: "0.6rem" }}>
            <strong>Watch</strong> opens Balerion at <code>{local}</code> with the magnet already
            handed over. That default is loopback, which means <em>this device</em>, so it works
            on the machine running Balerion and fails on a phone with &ldquo;could not connect to
            the server&rdquo;: your phone is not running one.
          </p>
          <p style={{ marginTop: "0.6rem" }}>
            That address comes from this deployment, so it is the same on every device you open
            this on. It is published to your tailnet by{" "}
            <code>tailscale serve --bg 8080</code> and to nothing else, which is the one way to
            reach it from a phone without also handing it to whoever else is on the wifi. Change
            it below only if this particular device needs a different one.
          </p>
          <p style={{ marginTop: "0.6rem" }}>
            The Archive is searched from here directly. apibay refuses datacentre addresses, so
            those searches are forwarded to the relay on your own machine instead.{" "}
            <button
              type="button"
              className="button-small quiet"
              onClick={() => setShowLocal((was) => !was)}
            >
              {showLocal ? "Done" : "Set the address for this device"}
            </button>
          </p>
          {showLocal ? (
            <div className="field" style={{ marginTop: "0.8rem", maxWidth: "22rem" }}>
              <label htmlFor="local">Local Balerion</label>
              <input
                id="local"
                type="url"
                spellCheck={false}
                value={local}
                onChange={(event) => rememberLocal(event.target.value)}
                placeholder={DEFAULT_LOCAL}
              />
              <p className="hint" style={{ marginTop: "0.5rem" }}>
                Remembered in this browser only, because it describes the device you are holding.
                On the machine running Balerion, leave it as {DEFAULT_LOCAL}.
              </p>
            </div>
          ) : null}
        </section>
      </main>
    </>
  );
}
