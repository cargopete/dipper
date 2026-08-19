/* The search rules, in one place.
 *
 * A deliberate port of `balerion-tpb` rather than a fresh take: the local player
 * and this site must not disagree about what a search means. If the Rust changes
 * these numbers, change them here in the same commit.
 *
 * Everything in this file runs on the server. apibay sends no CORS headers, so a
 * browser cannot call it directly, and routing through a server keeps the
 * visitor's address off somebody else's log as a side effect. */

export const API_URL = "https://apibay.org/q.php";

/** The sentinel row the API returns instead of an empty array. */
const NO_RESULTS_ID = "0";

/** What a thin line sustains, in bits per second. */
export const THIN_LINE_BPS = 1_500_000;

/** Below this, do not offer it: a magnet with no seeders never starts. */
const MIN_SEEDERS = 1;

/** The trackers thepiratebay's own magnet links carry. */
const TRACKERS = [
  "udp://tracker.opentrackr.org:1337/announce",
  "udp://open.stealth.si:80/announce",
  "udp://tracker.torrent.eu.org:451/announce",
  "udp://tracker.bittor.pw:1337/announce",
  "udp://open.demonii.com:1337/announce",
  "udp://tracker.dler.org:6969/announce",
  "udp://exodus.desync.com:6969/announce",
  "udp://explodie.org:6969/announce",
];

export type Category = {
  key: string;
  label: string;
  note: string;
  code: number;
  /** Typical runtime in seconds. A stated guess: apibay reports no duration. */
  runtime: number;
};

/* Video only. The parent code broadens to its children, so `200` covers all of
 * 2xx in one request. `cat=0` searches everything including the adult
 * categories and will return them for an innocent query, so it is never sent. */
export const CATEGORIES: Category[] = [
  {
    key: "video",
    label: "All video",
    note: "Everything filed under video. One request covers the lot.",
    code: 200,
    runtime: 3600,
  },
  {
    key: "hd_movies",
    label: "HD movies",
    note: "1080p and thereabouts. The best trade between size and picture.",
    code: 207,
    runtime: 6600,
  },
  {
    key: "movies",
    label: "Movies",
    note: "Standard definition. Small, quick to start, and it shows.",
    code: 201,
    runtime: 6600,
  },
  {
    key: "hd_tv",
    label: "HD TV shows",
    note: "Episodes and season packs at 1080p. A pack is one torrent of many files.",
    code: 208,
    runtime: 2700,
  },
  {
    key: "tv",
    label: "TV shows",
    note: "Standard definition episodes and packs.",
    code: 205,
    runtime: 2700,
  },
  {
    key: "uhd_movies",
    label: "UHD / 4K movies",
    note: "Tens of gigabytes each. A download rather than a stream.",
    code: 211,
    runtime: 6600,
  },
];

/** The largest item in this category a thin line could stream. */
export function thinCap(category: Category): number {
  return Math.floor((THIN_LINE_BPS * category.runtime) / 8);
}

export function findCategory(key: string | null): Category {
  return CATEGORIES.find((c) => c.key === key) ?? CATEGORIES[0];
}

export function isVideo(code: number): boolean {
  return code >= 200 && code <= 299;
}

export function categoryLabel(code: number): string {
  const named: Record<number, string> = {
    201: "Movies",
    202: "Movies DVDR",
    203: "Music videos",
    204: "Movie clips",
    205: "TV shows",
    206: "Handheld",
    207: "HD movies",
    208: "HD TV shows",
    209: "3D",
    210: "Cam / telesync",
    211: "UHD / 4K movies",
    212: "UHD / 4K TV shows",
  };
  if (named[code]) return named[code];
  if (isVideo(code)) return "Video";
  return "Unknown";
}

/** Percent-encode a URI query value. */
function encode(value: string): string {
  return encodeURIComponent(value).replace(
    /[!'()*]/g,
    (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase(),
  );
}

/**
 * Assemble a magnet URI, or null if the hash is not one.
 *
 * The check is the point: a magnet built from a blank or truncated hash is
 * syntactically perfect and resolves to nothing, which is indistinguishable
 * from a swarm with no peers.
 */
export function magnetUri(infoHash: string, name: string): string | null {
  if (!/^[0-9a-fA-F]{40}$/.test(infoHash)) return null;
  const parts = [`magnet:?xt=urn:btih:${infoHash.toUpperCase()}`, `dn=${encode(name)}`];
  for (const tracker of TRACKERS) parts.push(`tr=${encode(tracker)}`);
  return parts.join("&");
}

export type Hit = {
  id: number;
  name: string;
  infoHash: string;
  seeders: number;
  leechers: number;
  numFiles: number;
  sizeBytes: number;
  username: string;
  added: number;
  category: number;
  categoryLabel: string;
  magnet: string;
};

export type Results = {
  hits: Hit[];
  total: number;
  unseeded: number;
  oversize: number;
  cap: number | null;
  category: string;
  note: string;
};

/** The wire format: every field is a string, the numbers included. */
type RawTorrent = Record<string, string>;

function toHit(raw: RawTorrent): Hit | null {
  const infoHash = (raw.info_hash ?? "").toUpperCase();
  /* apibay serves names HTML-escaped, so "español" arrives as "espa&ntilde;ol".
   * Decoded here, once, before it reaches either the page or the magnet's
   * display name. */
  const name = decodeEntities(raw.name ?? "");
  const magnet = magnetUri(infoHash, name);
  if (!magnet) return null;

  const size = Number(raw.size);
  const id = Number(raw.id);
  if (!Number.isFinite(size) || !Number.isFinite(id)) return null;

  const category = Number(raw.category) || 0;
  return {
    id,
    name,
    infoHash,
    seeders: Number(raw.seeders) || 0,
    leechers: Number(raw.leechers) || 0,
    numFiles: Number(raw.num_files) || 0,
    sizeBytes: size,
    username: raw.username ?? "",
    added: Number(raw.added) || 0,
    category,
    categoryLabel: categoryLabel(category),
    magnet,
  };
}

/* A small named-entity table plus numeric forms. Enough for torrent names,
 * which is what this handles: accented letters and the five XML specials. A
 * full HTML entity table would be a dependency for no gain here. */
const ENTITIES: Record<string, string> = {
  amp: "&",
  lt: "<",
  gt: ">",
  quot: '"',
  apos: "'",
  nbsp: " ",
  ntilde: "ñ",
  Ntilde: "Ñ",
  eacute: "é",
  Eacute: "É",
  aacute: "á",
  Aacute: "Á",
  iacute: "í",
  oacute: "ó",
  uacute: "ú",
  uuml: "ü",
  ouml: "ö",
  auml: "ä",
  ccedil: "ç",
  egrave: "è",
  agrave: "à",
  ldquo: "“",
  rdquo: "”",
  lsquo: "‘",
  rsquo: "’",
  hellip: "…",
};

export function decodeEntities(text: string): string {
  return text.replace(/&(#x?[0-9a-fA-F]+|[a-zA-Z]+);/g, (whole, body: string) => {
    if (body.startsWith("#x") || body.startsWith("#X")) {
      const code = parseInt(body.slice(2), 16);
      return Number.isFinite(code) ? String.fromCodePoint(code) : whole;
    }
    if (body.startsWith("#")) {
      const code = parseInt(body.slice(1), 10);
      return Number.isFinite(code) ? String.fromCodePoint(code) : whole;
    }
    return ENTITIES[body] ?? whole;
  });
}

/**
 * Search apibay and apply every filter the local player applies.
 *
 * Order matters: size before seeders, so the two tallies never overlap and each
 * reports only what it alone excluded.
 */
export async function search(
  terms: string,
  category: Category,
  thin: boolean,
  limit: number,
): Promise<Results> {
  const url = `${API_URL}?q=${encode(terms.trim())}&cat=${category.code}`;
  const response = await fetch(url, {
    headers: { "user-agent": "balerion-site/0.1 (+https://github.com/cargopete/balerion)" },
    // A search is only ever as good as it is fresh, and apibay is quick.
    cache: "no-store",
    signal: AbortSignal.timeout(20_000),
  });
  if (!response.ok) {
    throw new Error(`apibay returned ${response.status}`);
  }

  const raw = (await response.json()) as RawTorrent[];
  if (!Array.isArray(raw)) throw new Error("apibay did not return a list");

  /* The API never returns []. An empty search comes back as a single row with
   * id "0", which parses perfectly and renders as a convincing fake result. */
  if (raw.length === 1 && raw[0]?.id === NO_RESULTS_ID) {
    return {
      hits: [],
      total: 0,
      unseeded: 0,
      oversize: 0,
      cap: thin ? thinCap(category) : null,
      category: category.key,
      note: category.note,
    };
  }

  const usable = raw
    .map(toHit)
    .filter((hit): hit is Hit => hit !== null)
    .filter((hit) => isVideo(hit.category));

  const cap = thin ? thinCap(category) : null;
  const affordable = cap === null ? usable : usable.filter((hit) => hit.sizeBytes <= cap);
  const oversize = usable.length - affordable.length;

  const seeded = affordable.filter((hit) => hit.seeders >= MIN_SEEDERS);
  const unseeded = affordable.length - seeded.length;

  // The endpoint appears to sort by seeders, and appears is not a promise.
  seeded.sort((a, b) => b.seeders - a.seeders);

  return {
    hits: seeded.slice(0, limit),
    total: seeded.length,
    unseeded,
    oversize,
    cap,
    category: category.key,
    note: category.note,
  };
}
