/* The search seam, as this deployment sees it.
 *
 * There is deliberately no logic in here. The rules about what a search means
 * — which categories are video, what a seeder floor is for, how two indexes'
 * copies of the same release are folded into one row, which of two releases is
 * the better one — all live in Rust, in `balerion-web`'s `find` module, and are
 * tested there.
 *
 * That is the whole point of this file existing at all. This site used to carry
 * its own translation of the archive.org rules and the apibay ones, so there
 * were two implementations of "what a search means" and they agreed only for as
 * long as somebody remembered to edit both. The relay now serves the seam, and
 * this asks it.
 *
 * What is left here is the shape of the answer, so the page can be typed.
 */

import { RelayError, relayConfig, relayFetch, type RelayConfig } from "./relay";

/** One index the relay can reach. */
export type Source = {
  /** What to put in `sources=`. */
  key: string;
  label: string;
  /** What is on the other end, said plainly. A collection label is not a
   * rights clearance and neither is an indexer's category. */
  note: string;
  /** On unless a viewer says otherwise. */
  default: boolean;
};

/** One result, whichever index produced it. */
export type Found = {
  /** Which index, or indexes: the same release from two of them is one row. */
  sources: string[];
  title: string;
  /** What to hand the player: an archive.org identifier or a magnet. */
  open: string;
  info_hash?: string;
  size?: number;
  seeders?: number;
  detail?: string;
};

export type FindResults = {
  hits: Found[];
  /** Which indexes answered, so the page can say when one did not. A short
   * list is otherwise indistinguishable from a thorough search that found
   * little. */
  answered: string[];
  failed: string[];
  /** How many results were the same release arriving twice. */
  duplicates: number;
};

/** Which indexes this deployment's relay can reach. */
export async function sources(config: RelayConfig): Promise<Source[]> {
  const answer = await relayFetch("/sources", new URLSearchParams(), config);
  return answer as Source[];
}

/** Ask, through the relay. */
export async function find(
  config: RelayConfig,
  terms: string,
  keys: string[],
  limit: number,
): Promise<FindResults> {
  const params = new URLSearchParams({
    q: terms,
    sources: keys.join(","),
    limit: String(limit),
  });
  // Longer than the default: a fan-out is as slow as its slowest index, and
  // the relay is already bounding each of them.
  return (await relayFetch("/find", params, config, 30_000)) as FindResults;
}

/**
 * The indexes to offer, or none when there is no relay to ask.
 *
 * Failure is not an error here. A relay that is asleep means this deployment
 * can still search archive.org directly, which is the half that works whether
 * or not anybody's laptop is open, and the menu should simply be shorter rather
 * than the page being broken.
 */
export async function reachableSources(): Promise<Source[]> {
  const config = relayConfig();
  if (!config) return [];
  try {
    return await sources(config);
  } catch (err) {
    if (!(err instanceof RelayError)) throw err;
    return [];
  }
}
