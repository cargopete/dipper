"use client";

import { useCallback, useEffect, useState } from "react";

type Option = { key: string; label: string; note: string; thinCap: number | null };

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
  /** What gets copied, or a link to open when there is nothing to copy. */
  copy: string | null;
  href: string | null;
};

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
  const [index, setIndex] = useState<"ia" | "tpb">("ia");
  const [filter, setFilter] = useState("prelinger");
  const [terms, setTerms] = useState("");
  const [thin, setThin] = useState(false);
  const [rows, setRows] = useState<Row[] | null>(null);
  const [count, setCount] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<string | null>(null);

  const chosenIndex = indexes.find((entry) => entry.key === index);
  const chosenOption = chosenIndex?.options.find((option) => option.key === filter);

  useEffect(() => {
    fetch("/api/search?catalogue")
      .then((response) => (response.ok ? response.json() : Promise.reject(new Error())))
      .then((body: { indexes: IndexInfo[] }) => setIndexes(body.indexes))
      .catch(() => setError("Could not load the indexes. Nothing will search until that does."));
  }, []);

  const run = useCallback(
    async (event?: React.FormEvent) => {
      event?.preventDefault();
      const entry = indexes.find((i) => i.key === index);
      if (!entry) return;
      // The Archive browses on an empty query; apibay has no browse.
      if (!terms.trim() && index === "tpb") return;

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
    [filter, index, indexes, terms, thin],
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
        <section className="console">
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
                    <button
                      type="button"
                      className={copied === row.key ? "button-small done" : "button-small"}
                      onClick={() => copy(row)}
                    >
                      {copied === row.key ? "Copied" : copyLabel}
                    </button>
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
            The Archive is searched from here directly. apibay refuses datacentre addresses, so
            those searches are forwarded to the relay on your machine instead, and need it
            awake.
          </p>
        </section>
      </main>
    </>
  );
}
