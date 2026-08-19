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
  tag: string;
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
  swarm: { seeders: number; leechers: number } | null;
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
function watchHere(base: string, open: string): string {
  /* Guarded, because an unusable base produces a link that looks fine and does
     something quite different. With an empty base this built "/#magnet=...",
     which is a fragment on *this* page: clicking it scrolls to the top and
     nothing else happens, and there is nothing on screen to say why. */
  const trimmed = base.trim().replace(/\/+$/, "");
  const usable = /^https?:\/\/[^/]+$/.test(trimmed) ? trimmed : DEFAULT_LOCAL;
  return `${usable}/#magnet=${encodeURIComponent(open)}`;
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

function toRows(index: "ia" | "tpb", data: Record<string, unknown>): Row[] {
  const hits = (data.hits ?? []) as Record<string, never>[];
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
  const [index, setIndex] = useState<"ia" | "tpb">("tpb");
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
  /* Remembered per browser rather than configured on the server: it describes
     the machine you are sitting at, and the server has no business knowing it. */
  const [local, setLocal] = useState(DEFAULT_LOCAL);
  const [showLocal, setShowLocal] = useState(false);

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
      .then((body: { indexes: IndexInfo[] }) => setIndexes(body.indexes))
      .catch(() => setError("Could not load the indexes. Nothing will search until that does."));
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
    async (searchFor: string, forIndex: "ia" | "tpb" = index) => {
      const entry = indexes.find((i) => i.key === forIndex);
      if (!entry) return;
      // The Archive browses on an empty query; apibay has no browse.
      if (!searchFor.trim() && forIndex === "tpb") return;
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
        const shown = produced.length
          ? `showing ${produced.length} of ${body.total}`
          : "nothing to show";
        const hidden = [
          body.oversize ? `${body.oversize} too large` : null,
          body.unseeded ? `${body.unseeded} unseeded` : null,
        ].filter(Boolean);
        setCount(hidden.length ? `${shown}, ${hidden.join(" and ")} hidden` : shown);
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
  function switchIndex(next: "ia" | "tpb") {
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

  const copyLabel = index === "tpb" ? "Copy magnet" : "Copy identifier";

  return (
    <>
      <header className="masthead">
        <p className="brand">BALERION</p>
        <p className="masthead-note">search only</p>
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
                    <p className="show-summary">{openShow.summary.slice(0, 320)}</p>
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

                      <ul className="episodes">
                        {episodeList
                          .filter((e) => e.season === season)
                          .map((e) => (
                            <li key={e.id}>
                              <span className="episode-tag">{e.tag}</span>
                              <span>{e.name}</span>
                              <span className="episode-date">{e.airdate ?? ""}</span>
                              <button
                                type="button"
                                className="button-small"
                                onClick={() => watchQuery(e.tag)}
                              >
                                Find
                              </button>
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
              <div className="field">
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
                          ? `${row.swarm.seeders} seeding, ${row.swarm.leechers} leeching`
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
                          {` / ${row.swarm.leechers}`}
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
                      <button
                        type="button"
                        className={
                          copied === row.key ? "button-small quiet done" : "button-small quiet"
                        }
                        onClick={() => copy(row)}
                      >
                        {copied === row.key ? "Copied" : copyLabel}
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
            <strong>Watch</strong> opens the Balerion running on the machine you are reading this
            on, at <code>{local}</code>, with the magnet already handed over. If nothing is
            running there the tab simply will not load, which is the case on a phone. Copy
            magnet always works.
          </p>
          <p style={{ marginTop: "0.6rem" }}>
            The Archive is searched from here directly. apibay refuses datacentre addresses, so
            those searches are forwarded to the relay on your own machine instead.{" "}
            <button
              type="button"
              className="button-small quiet"
              onClick={() => setShowLocal((was) => !was)}
            >
              {showLocal ? "Done" : "Change local address"}
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
            </div>
          ) : null}
        </section>
      </main>
    </>
  );
}
