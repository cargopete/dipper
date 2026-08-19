/* archive.org search, direct from the server.
 *
 * Unlike apibay, the Archive answers a datacentre address perfectly happily, so
 * this needs no relay and works whether or not anyone's laptop is awake. A port
 * of the rules in the `balerion-web` search handler.
 *
 * Uses the page-based advanced endpoint rather than the scrape API, whose cursor
 * pagination does not work anonymously. See docs/archive-org-notes.md. */

const ENDPOINT = "https://archive.org/advancedsearch.php";

/** archive.org marks derived torrents with this format string. */
const TORRENT_FILTER = 'format:"Archive BitTorrent"';

export type Shelf = {
  key: string;
  label: string;
  note: string;
  /** The collection identifier, or null for the whole moving image library. */
  collection: string | null;
};

/* Curated shelves, most reliably free first. A collection label is not a rights
 * clearance, which is why the last one says so out loud. */
export const SHELVES: Shelf[] = [
  {
    key: "prelinger",
    label: "Prelinger Archives",
    note: "Ephemeral, industrial and educational film. Deliberately public domain.",
    collection: "prelinger",
  },
  {
    key: "classic_cartoons",
    label: "Classic cartoons",
    note: "Animation old enough that its copyright has lapsed.",
    collection: "classic_cartoons",
  },
  {
    key: "film_noir",
    label: "Film noir",
    note: "Noir features whose copyright was not renewed.",
    collection: "film_noir",
  },
  {
    key: "sci-fi_horror",
    label: "Science fiction and horror",
    note: "B-movies, mostly out of copyright.",
    collection: "sci-fi_horror",
  },
  {
    key: "computerchronicles",
    label: "The Computer Chronicles",
    note: "The television series, released under a Creative Commons licence.",
    collection: "computerchronicles",
  },
  {
    key: "all",
    label: "Everything",
    note:
      "The whole moving image library. Uploaded by the public, so the rights status of any " +
      "given item is whatever its uploader claimed.",
    collection: null,
  },
];

export function findShelf(key: string | null): Shelf {
  return SHELVES.find((shelf) => shelf.key === key) ?? SHELVES[0];
}

/**
 * Build the Lucene query archive.org wants.
 *
 * Both filters are load-bearing: without `mediatype` the results include things
 * Balerion cannot play, and without the torrent filter they include things it
 * cannot fetch at all.
 */
export function buildQuery(terms: string, collection: string | null): string {
  const parts = ["mediatype:(movies)", TORRENT_FILTER];
  if (collection) parts.push(`collection:(${collection})`);
  const trimmed = terms.trim();
  if (trimmed) {
    // Parenthesised so `a OR b` cannot break out of the conjunction and quietly
    // widen the search to the whole archive.
    parts.push(`(${trimmed})`);
  }
  return parts.join(" AND ");
}

export type ArchiveHit = {
  identifier: string;
  title: string;
  creator: string | null;
  year: string | null;
  sizeBytes: number | null;
  downloads: number | null;
  detailsUrl: string;
};

export type ArchiveResults = {
  hits: ArchiveHit[];
  total: number;
  shelf: string;
  note: string;
};

type RawDoc = Record<string, unknown>;

function asString(value: unknown): string | null {
  if (typeof value === "string") return value;
  if (Array.isArray(value) && typeof value[0] === "string") return value[0];
  return null;
}

function asNumber(value: unknown): number | null {
  const n = Number(asString(value) ?? value);
  return Number.isFinite(n) ? n : null;
}

/** An item with no derived torrent is one Balerion cannot open. */
function hasTorrent(doc: RawDoc): boolean {
  const format = doc.format;
  if (Array.isArray(format)) return format.some((f) => f === "Archive BitTorrent");
  return asString(format) === "Archive BitTorrent";
}

export async function search(
  terms: string,
  shelf: Shelf,
  limit: number,
): Promise<ArchiveResults> {
  const params = new URLSearchParams({
    q: buildQuery(terms, shelf.collection),
    rows: String(limit),
    page: "1",
    output: "json",
  });
  for (const field of [
    "identifier",
    "title",
    "creator",
    "year",
    "publicdate",
    "downloads",
    "item_size",
    "format",
  ]) {
    params.append("fl[]", field);
  }
  // Popularity is a decent stand-in for relevance, and far better than
  // identifier order, which sorts by whatever the uploader typed.
  params.append("sort[]", "downloads desc");

  const response = await fetch(`${ENDPOINT}?${params}`, {
    headers: {
      "user-agent": "balerion-site/0.1 (+https://github.com/cargopete/balerion)",
      accept: "application/json",
    },
    cache: "no-store",
    signal: AbortSignal.timeout(20_000),
  });
  if (!response.ok) throw new Error(`archive.org returned ${response.status}`);

  const body = (await response.json()) as {
    response?: { numFound?: number; docs?: RawDoc[] };
  };
  const docs = body.response?.docs ?? [];

  const hits: ArchiveHit[] = docs.filter(hasTorrent).map((doc) => {
    const identifier = asString(doc.identifier) ?? "";
    const publicdate = asString(doc.publicdate);
    return {
      identifier,
      title: (asString(doc.title) ?? identifier).trim(),
      creator: asString(doc.creator),
      year: asString(doc.year) ?? (publicdate ? publicdate.slice(0, 4) : null),
      sizeBytes: asNumber(doc.item_size),
      downloads: asNumber(doc.downloads),
      detailsUrl: `https://archive.org/details/${encodeURIComponent(identifier)}`,
    };
  });

  return {
    hits,
    total: body.response?.numFound ?? hits.length,
    shelf: shelf.key,
    note: shelf.note,
  };
}
