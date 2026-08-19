/* The category table, in one place.
 *
 * A deliberate port of `balerion-tpb`'s categories rather than a fresh take: the
 * local player and this site must not disagree about what a search means. If the
 * Rust changes these numbers, change them here in the same commit.
 *
 * This file used to search apibay too. It cannot any more: apibay serves a
 * Cloudflare bot challenge to datacentre addresses, so the search itself now
 * happens in the Rust relay on the user's own machine and this keeps only what
 * the page needs to draw its menus before the relay has been asked anything.
 * Keeping the table here rather than fetching it from the relay means the menus
 * still render when that machine is asleep. */

/** What a thin line sustains, in bits per second. */
export const THIN_LINE_BPS = 1_500_000;

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

export type RelayHit = {
  id: number;
  name: string;
  info_hash: string;
  seeders: number;
  leechers: number;
  num_files: number;
  size_bytes: number;
  username: string;
  status: string;
  added: number;
  category: number;
  category_label: string;
  magnet: string;
};

export type RelayResults = {
  hits: RelayHit[];
  total: number;
  unseeded: number;
  oversize: number;
  cap: number | null;
  category: string;
  note: string;
};

/** One hit in the shape the page reads, whichever index produced it. */
export type Hit = {
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

/**
 * Rename the relay's fields into the page's.
 *
 * Worth the ten lines: without it the page reads `hit.sizeBytes` off an object
 * that says `size_bytes`, renders `undefined` for every size and date, and looks
 * like a styling problem rather than a naming one.
 */
export function fromRelay(hit: RelayHit): Hit {
  return {
    id: hit.id,
    name: hit.name,
    seeders: hit.seeders,
    leechers: hit.leechers,
    numFiles: hit.num_files,
    sizeBytes: hit.size_bytes,
    username: hit.username,
    added: hit.added,
    categoryLabel: hit.category_label,
    magnet: hit.magnet,
  };
}
