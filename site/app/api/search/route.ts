import { NextResponse } from "next/server";

import * as archive from "../../../lib/archive";
import { CATEGORIES, fromRelay, thinCap, type RelayResults } from "../../../lib/apibay";
import { RelayError, relayConfig, relayFetch } from "../../../lib/relay";

/* Two indexes, reached two different ways, and the difference is not a design
 * choice.
 *
 * archive.org answers this deployment directly. apibay does not: Cloudflare
 * serves a bot challenge to datacentre addresses, measured from `iad1` with both
 * our own User-Agent and a full browser one. So apibay queries are forwarded to
 * a relay on the user's own machine, where the request comes from a domestic
 * connection and gets a clean 200. */

/** What the page needs to build its menus, plus what it can actually reach. */
function catalogue() {
  return NextResponse.json({
    relayConfigured: relayConfig() !== null,
    indexes: [
      {
        key: "ia",
        label: "Internet Archive",
        reachable: true,
        filterLabel: "Collection",
        options: archive.SHELVES.map((shelf) => ({
          key: shelf.key,
          label: shelf.label,
          note: shelf.note,
          thinCap: null,
        })),
        note: null,
      },
      {
        key: "tpb",
        label: "apibay",
        reachable: relayConfig() !== null,
        filterLabel: "Category",
        options: CATEGORIES.map((category) => ({
          key: category.key,
          label: category.label,
          note: category.note,
          thinCap: thinCap(category),
        })),
        note:
          "A public index of whatever strangers uploaded. Most of it is copyrighted, none of " +
          "it is cleared, and the category is not a licence. Your connection, your " +
          "jurisdiction, your problem.",
      },
    ],
  });
}

export async function GET(request: Request) {
  const params = new URL(request.url).searchParams;
  if (params.get("catalogue") !== null) return catalogue();

  const terms = (params.get("q") ?? "").trim();
  const index = params.get("index") === "tpb" ? "tpb" : "ia";
  const limit = Math.min(Math.max(Number(params.get("limit")) || 24, 1), 100);

  if (index === "ia") {
    /* An empty query is a browse of the collection here, which is genuinely
     * useful and quite unlike apibay. */
    try {
      const shelf = archive.findShelf(params.get("filter"));
      const results = await archive.search(terms, shelf, limit);
      return NextResponse.json({ index, ...results });
    } catch (err) {
      const because = err instanceof Error ? err.message : "unknown";
      return NextResponse.json(
        { error: `archive.org search failed: ${because}` },
        { status: 502 },
      );
    }
  }

  /* apibay has no browse: an empty query returns its no-results sentinel, which
   * would surface as "nothing matches that", which is not what happened. */
  if (!terms) {
    return NextResponse.json({ error: "type something to search for" }, { status: 400 });
  }

  const relay = relayConfig();
  if (!relay) {
    return NextResponse.json(
      {
        error:
          "apibay searches need the relay. Set BALERION_RELAY_URL and BALERION_RELAY_TOKEN " +
          "on this deployment and point them at the Balerion relay on your own machine.",
      },
      { status: 503 },
    );
  }

  const forwarded = new URLSearchParams({
    q: terms,
    category: params.get("filter") ?? "video",
    limit: String(limit),
    thin: params.get("thin") === "true" ? "true" : "false",
  });

  try {
    const results = (await relayFetch("/search", forwarded, relay)) as RelayResults;
    // The relay speaks Rust field names; the page reads ours.
    return NextResponse.json({ ...results, index, hits: results.hits.map(fromRelay) });
  } catch (err) {
    if (err instanceof RelayError) {
      // 503 for "your machine is asleep", 502 for anything it actually said.
      const status = err.kind === "unreachable" || err.kind === "unconfigured" ? 503 : 502;
      return NextResponse.json({ error: err.message, kind: err.kind }, { status });
    }
    const because = err instanceof Error ? err.message : "unknown";
    return NextResponse.json({ error: `apibay search failed: ${because}` }, { status: 502 });
  }
}
