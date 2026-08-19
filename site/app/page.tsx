"use client";

import { useEffect, useState } from "react";

type CategoryInfo = { key: string; label: string; note: string; thinCap: number };

type Hit = {
  id: number;
  name: string;
  seeders: number;
  leechers: number;
  numFiles: number;
  sizeBytes: number;
  username: string;
  added: number;
  categoryLabel: string;
  magnet: string;
};

type Results = {
  hits: Hit[];
  total: number;
  unseeded: number;
  oversize: number;
  cap: number | null;
  note: string;
};

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

function bytes(n: number): string {
  if (!n) return "0 B";
  let size = n;
  let unit = 0;
  while (size >= 1024 && unit < UNITS.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size < 10 && unit > 0 ? size.toFixed(1) : Math.round(size)} ${UNITS[unit]}`;
}

/** Below about ten seeders a stream will not keep ahead of the playhead. */
const THIN_SWARM = 10;

export default function Page() {
  const [categories, setCategories] = useState<CategoryInfo[]>([]);
  const [category, setCategory] = useState("video");
  const [terms, setTerms] = useState("");
  const [thin, setThin] = useState(false);
  const [results, setResults] = useState<Results | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<number | null>(null);

  useEffect(() => {
    fetch("/api/search?catalogue")
      .then((response) => (response.ok ? response.json() : Promise.reject(new Error())))
      .then((body: { categories: CategoryInfo[] }) => setCategories(body.categories))
      .catch(() =>
        setError("Could not load the categories. The search will not work until that does."),
      );
  }, []);

  const chosen = categories.find((entry) => entry.key === category);

  async function run(event?: React.FormEvent) {
    event?.preventDefault();
    if (!terms.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const query = new URLSearchParams({
        q: terms.trim(),
        category,
        limit: "24",
        thin: thin ? "true" : "false",
      });
      const response = await fetch(`/api/search?${query}`);
      const body = await response.json();
      if (!response.ok) throw new Error(body?.error ?? `${response.status}`);
      setResults(body as Results);
    } catch (err) {
      setResults(null);
      setError(err instanceof Error ? err.message : "the search failed");
    } finally {
      setBusy(false);
    }
  }

  /* Re-run when the cap is toggled, but only with results already on screen:
   * otherwise ticking the box before searching fires a search nobody asked
   * for. */
  useEffect(() => {
    if (results) void run();
    // Only the toggle should retrigger this, not every keystroke in the box.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [thin]);

  async function copy(hit: Hit) {
    try {
      await navigator.clipboard.writeText(hit.magnet);
      setCopied(hit.id);
      window.setTimeout(() => setCopied((was) => (was === hit.id ? null : was)), 1600);
    } catch {
      setError("The browser would not let the page write to the clipboard.");
    }
  }

  const count = results
    ? (() => {
        const shown = results.hits.length
          ? `showing ${results.hits.length} of ${results.total}`
          : "nothing to show";
        const hidden = [
          results.oversize ? `${results.oversize} too large` : null,
          results.unseeded ? `${results.unseeded} unseeded` : null,
        ].filter(Boolean);
        return hidden.length ? `${shown}, ${hidden.join(" and ")} hidden` : shown;
      })()
    : "";

  return (
    <>
      <header className="masthead">
        <p className="brand">BALERION</p>
        <p className="masthead-note">search only</p>
      </header>

      <main>
        <section className="console" aria-labelledby="search-heading">
          <h1 id="search-heading" className="brand" style={{ display: "none" }}>
            Search
          </h1>
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
                <label htmlFor="category">Category</label>
                <select
                  id="category"
                  value={category}
                  onChange={(event) => setCategory(event.target.value)}
                >
                  {categories.map((entry) => (
                    <option key={entry.key} value={entry.key}>
                      {entry.label}
                    </option>
                  ))}
                </select>
              </div>
              <button type="submit" disabled={busy || terms.trim().length === 0}>
                {busy ? "Searching" : "Search"}
              </button>
            </div>

            {chosen ? (
              <label className="toggle" htmlFor="thin">
                <input
                  id="thin"
                  type="checkbox"
                  checked={thin}
                  onChange={(event) => setThin(event.target.checked)}
                />
                <span>Fits a thin line (under {bytes(chosen.thinCap)})</span>
              </label>
            ) : null}
          </form>

          {chosen ? <p className="hint">{chosen.note}</p> : null}
          <p className="hint hint-caution">
            A public index of whatever strangers uploaded. Most of it is copyrighted, none of
            it is cleared, and the category is not a licence. Your connection, your
            jurisdiction, your problem.
          </p>
        </section>

        {error ? (
          <section className="notice" role="alert">
            <p className="notice-title">That did not work</p>
            <p>{error}</p>
          </section>
        ) : null}

        {busy && !results ? (
          <section aria-busy="true" aria-label="Searching">
            {[0, 1, 2, 3, 4].map((row) => (
              <div className="skeleton-row" key={row} />
            ))}
          </section>
        ) : null}

        {results ? (
          <section aria-labelledby="results-heading">
            <div className="results-head">
              <h2 id="results-heading">Results</h2>
              <p className="results-count">{count}</p>
            </div>

            {results.hits.length === 0 ? (
              <p className="empty">
                {thin
                  ? "Nothing here small enough for a thin line. The standard definition categories are where the small releases live; failing that, untick the box and accept the stalling."
                  : "Nothing in this category with anyone seeding it. Try fewer words, or a broader category."}
              </p>
            ) : (
              <ul className="results">
                {results.hits.map((hit) => (
                  <li key={hit.id}>
                    <div className="result-title">
                      {hit.name}
                      <span className="result-meta">
                        {[
                          hit.categoryLabel,
                          hit.numFiles > 1 ? `${hit.numFiles} files` : null,
                          new Date(hit.added * 1000).toISOString().slice(0, 10),
                          hit.username,
                        ]
                          .filter(Boolean)
                          .join("  /  ")}
                      </span>
                    </div>
                    <span
                      className="result-swarm"
                      title={`${hit.seeders} seeding, ${hit.leechers} leeching`}
                    >
                      <span className={hit.seeders < THIN_SWARM ? "seeders-thin" : "seeders"}>
                        {hit.seeders}
                      </span>
                      {` / ${hit.leechers}`}
                    </span>
                    <span className="result-size">{bytes(hit.sizeBytes)}</span>
                    <button
                      type="button"
                      className={copied === hit.id ? "button-small done" : "button-small"}
                      onClick={() => copy(hit)}
                    >
                      {copied === hit.id ? "Copied" : "Copy magnet"}
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
            disk, none of which a serverless function can do, so nothing here plays anything.
            Copy a magnet and paste it into the Balerion running on your own machine, which is
            where the swarm and the video both live: <code>balerion serve</code>.
          </p>
        </section>
      </main>
    </>
  );
}
