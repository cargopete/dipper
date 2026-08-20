import { NextResponse } from "next/server";

import * as archive from "../../../lib/archive";
import { CATEGORIES, fromRelay, thinCap, type RelayResults } from "../../../lib/apibay";
import { find, reachableSources } from "../../../lib/find";
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
/* Where this deployment's Balerion lives, if it has been told.
 *
 * One address for every device rather than a setting per browser. A tailnet name
 * works from the machine itself as well as from a phone, so there is nothing to
 * configure twice and nothing that only works when you are sitting at the right
 * desk. Loopback is the fallback, which is right for someone running both halves
 * on one machine and wrong for everyone else, which is why this exists. */
function localBalerion(): string | null {
  const url = process.env.BALERION_LOCAL_URL?.trim();
  return url ? url.replace(/\/+$/, "") : null;
}

async function catalogue() {
  /* Whatever else this deployment's relay can reach. Torznab indexers live on
   * that machine and are named by whoever set them up, so they can only arrive
   * at runtime; asking for them is free and failing to reach them simply makes
   * the menu shorter. */
  const extra = await reachableSources();
  const indexers = extra.filter((source) => source.key.startsWith("torznab:"));

  return NextResponse.json({
    relayConfigured: relayConfig() !== null,
    local: localBalerion(),
    // Ordered as the menu shows them, apibay first: it is what this is used for.
    // The Archive is the safer index and the second choice, which is a different
    // claim from being the default one.
    indexes: [
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
      ...indexers.map((source) => ({
        key: source.key,
        label: source.label,
        reachable: true,
        // A Torznab indexer has categories, but which ones depends entirely on
        // what is behind it, and offering the wrong list is worse than none.
        filterLabel: null,
        options: [],
        note: source.note,
      })),
      /* Every index at once, which is the thing the seam exists for: the
       * answers are merged, results that are the same torrent are folded into
       * one row on their infohash, and the ordering is by what a release is
       * rather than by how popular it is. Only offered when there is more than
       * one thing to ask. */
      ...(extra.length > 1
        ? [
            {
              key: "all",
              label: "Every index",
              reachable: true,
              filterLabel: null,
              options: [],
              note:
                "Asks all of them at once and folds results that are the same torrent into " +
                "one row. The cautions above apply to whichever index a row came from.",
            },
          ]
        : []),
    ],
  });
}

export async function GET(request: Request) {
  const params = new URL(request.url).searchParams;
  if (params.get("catalogue") !== null) return catalogue();

  const terms = (params.get("q") ?? "").trim();
  const asked = params.get("index") ?? "ia";
  const limit = Math.min(Math.max(Number(params.get("limit")) || 24, 1), 100);

  /* Anything that is not one of the two written out below goes through the
   * relay's seam: a named Torznab indexer, or all of them at once. */
  if (asked.startsWith("torznab:") || asked === "all") {
    const relay = relayConfig();
    if (!relay) {
      return NextResponse.json(
        { error: "that index lives on your machine, and the relay is not configured." },
        { status: 503 },
      );
    }
    if (!terms) {
      return NextResponse.json({ error: "type something to search for" }, { status: 400 });
    }

    const keys =
      asked === "all"
        ? (await reachableSources()).map((source) => source.key)
        : [asked];
    try {
      const results = await find(relay, terms, keys, limit);
      return NextResponse.json({ index: asked, ...results });
    } catch (err) {
      if (err instanceof RelayError) {
        const status = err.kind === "unreachable" || err.kind === "unconfigured" ? 503 : 502;
        return NextResponse.json({ error: err.message, kind: err.kind }, { status });
      }
      const because = err instanceof Error ? err.message : "unknown";
      return NextResponse.json({ error: `search failed: ${because}` }, { status: 502 });
    }
  }

  const index = asked === "tpb" ? "tpb" : "ia";

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
          "apibay searches need the relay. Set BALERION_RELAY_URL on this deployment and " +
          "point it at a Balerion relay. No token is needed if that relay was started with " +
          "--vercel-project for this project.",
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
