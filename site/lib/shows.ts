/* The catalogue.
 *
 * apibay has no browse: an empty query returns its no-results sentinel, and
 * there is no listing endpoint. So browsing needs a source of titles, and this
 * is it. TVmaze because it needs no API key, which matters more than it sounds:
 * every key is a secret somebody has to carry between two dashboards, and there
 * are enough of those already.
 *
 * Checked before being relied on: TVmaze answers a datacentre address with a
 * 200, from both Hetzner regions. apibay does not, which is the whole reason the
 * relay exists, so it was worth confirming rather than assuming.
 *
 * This only ever produces *titles*. Turning a title into something watchable is
 * still an apibay search through the relay. */

const API = "https://api.tvmaze.com";

export type Show = {
  id: number;
  name: string;
  premiered: string | null;
  ended: string | null;
  status: string | null;
  genres: string[];
  /** TVmaze's own popularity, 0 to 100. */
  weight: number;
  summary: string | null;
  poster: string | null;
};

export type Episode = {
  id: number;
  season: number;
  number: number;
  name: string;
  airdate: string | null;
  /** Minutes. What makes a release's bitrate computable rather than guessed. */
  runtime: number | null;
  /** `S01E03`, the form a release name uses. */
  tag: string;
};

/** Strip the HTML TVmaze puts in summaries. */
function plain(html: string | null | undefined): string | null {
  if (!html) return null;
  return html
    .replace(/<[^>]*>/g, "")
    .replace(/&amp;/g, "&")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&nbsp;/g, " ")
    .trim();
}

type RawShow = Record<string, unknown>;

function toShow(raw: RawShow): Show {
  const image = (raw.image ?? {}) as Record<string, string>;
  return {
    id: Number(raw.id),
    name: String(raw.name ?? ""),
    premiered: (raw.premiered as string) ?? null,
    ended: (raw.ended as string) ?? null,
    status: (raw.status as string) ?? null,
    genres: Array.isArray(raw.genres) ? (raw.genres as string[]) : [],
    weight: Number(raw.weight ?? 0),
    summary: plain(raw.summary as string),
    // The medium is a portrait thumbnail, which is what a grid wants; the
    // original is a full-size poster and far too much for a row of tiles.
    poster: image.medium ?? image.original ?? null,
  };
}

async function get(path: string): Promise<unknown> {
  const response = await fetch(`${API}${path}`, {
    headers: { accept: "application/json" },
    // Titles change slowly. An hour keeps the grid quick without going stale in
    // any way anyone would notice.
    next: { revalidate: 3600 },
    signal: AbortSignal.timeout(15_000),
  });
  if (!response.ok) throw new Error(`tvmaze returned ${response.status}`);
  return response.json();
}

/**
 * A hand-picked opening shelf.
 *
 * TVmaze has no "popular" endpoint, and its `weight` only ranks within whatever
 * you already fetched, so there is nothing honest to sort. Rather than page
 * through thousands of shows and pretend the result is a chart, this is a shelf
 * somebody chose, in the same spirit as the Archive collections: a stated
 * selection rather than an implied ranking.
 */
export const SHELF = [
  "Game of Thrones",
  "Better Call Saul",
  "Breaking Bad",
  "Dune: Prophecy",
  "House of the Dragon",
  "The Sopranos",
  "The Wire",
  "Chernobyl",
  "True Detective",
  "Succession",
  "Severance",
  "The Last of Us",
  "Fargo",
  "Mr. Robot",
  "Peaky Blinders",
  "Andor",
];

/** Look one title up. Null when TVmaze has never heard of it. */
export async function lookup(name: string): Promise<Show | null> {
  try {
    const raw = await get(`/singlesearch/shows?q=${encodeURIComponent(name)}`);
    return raw ? toShow(raw as RawShow) : null;
  } catch {
    // One title missing should not empty the shelf.
    return null;
  }
}

/** The opening shelf, resolved. Order is the shelf's, not a ranking. */
export async function shelf(): Promise<Show[]> {
  const found = await Promise.all(SHELF.map(lookup));
  return found.filter((show): show is Show => show !== null);
}

/** Search by title. */
export async function search(terms: string): Promise<Show[]> {
  const raw = (await get(`/search/shows?q=${encodeURIComponent(terms)}`)) as {
    show: RawShow;
  }[];
  return raw.map((entry) => toShow(entry.show));
}

export async function episodes(showId: number): Promise<Episode[]> {
  const raw = (await get(`/shows/${showId}/episodes`)) as RawShow[];
  const pad = (n: number) => String(n).padStart(2, "0");
  return raw
    .filter((entry) => Number(entry.season) > 0)
    .map((entry) => ({
      id: Number(entry.id),
      season: Number(entry.season),
      number: Number(entry.number),
      name: String(entry.name ?? ""),
      airdate: (entry.airdate as string) || null,
      runtime: Number(entry.runtime) || null,
      tag: `S${pad(Number(entry.season))}E${pad(Number(entry.number))}`,
    }));
}

/**
 * What to type into apibay for one episode.
 *
 * Punctuation is dropped rather than escaped: release names are full of dots
 * and underscores where the title had spaces, and apibay matches on words. A
 * colon in "Dune: Prophecy" matches nothing at all.
 */
export function queryFor(show: Show, episode: Episode): string {
  const title = show.name.replace(/[^\p{L}\p{N}]+/gu, " ").trim();
  return `${title} ${episode.tag}`;
}

/** And for a whole season, which is how packs are usually named. */
export function seasonQueryFor(show: Show, season: number): string {
  const title = show.name.replace(/[^\p{L}\p{N}]+/gu, " ").trim();
  return `${title} season ${season}`;
}
