import { NextResponse } from "next/server";

import { fromRelay, type RelayResults } from "../../../lib/apibay";
import { pick, rateLabel } from "../../../lib/pick";
import { RelayError, relayConfig, relayFetch } from "../../../lib/relay";

/* Pick and play: one request from "I want this episode" to "here is the magnet".
 *
 * The search, the filtering and the choosing all happen here so the page does
 * not have to show anyone a list of releases they did not ask to read. What it
 * returns still says exactly what was chosen and why, because a thing that picks
 * for you and will not say what it picked is worse than a list. */

export async function GET(request: Request) {
  const params = new URL(request.url).searchParams;
  const terms = (params.get("q") ?? "").trim();
  const runtime = Number(params.get("runtime")) || 45;

  if (!terms) {
    return NextResponse.json({ error: "nothing to search for" }, { status: 400 });
  }

  const relay = relayConfig();
  if (!relay) {
    return NextResponse.json(
      { error: "apibay searches need the relay, and this deployment has not been told where it is" },
      { status: 503 },
    );
  }

  const forwarded = new URLSearchParams({
    q: terms,
    category: "video",
    limit: "50",
    // Deliberately no size cap here: the picker judges by bitrate against the
    // real runtime, which is a better question than size against a guess.
    thin: "false",
  });

  try {
    const results = (await relayFetch("/search", forwarded, relay)) as RelayResults;
    const hits = results.hits.map(fromRelay);
    const choice = pick(hits, runtime);

    if (!choice) {
      return NextResponse.json(
        {
          error:
            hits.length > 0
              ? "nothing here is worth playing: what there is looks like a cam recording"
              : "nothing found for that episode",
        },
        { status: 404 },
      );
    }

    return NextResponse.json({
      magnet: choice.hit.magnet,
      name: choice.hit.name,
      sizeBytes: choice.hit.sizeBytes,
      seeders: choice.hit.seeders,
      bitrate: choice.bitrate,
      rate: rateLabel(choice.bitrate),
      overBudget: choice.overBudget,
      considered: choice.considered,
      why: choice.why,
      // Tried in order if the first will not give up a file list.
      alternatives: choice.alternatives.map((hit) => hit.magnet),
    });
  } catch (err) {
    if (err instanceof RelayError) {
      const status = err.kind === "unreachable" || err.kind === "unconfigured" ? 503 : 502;
      return NextResponse.json({ error: err.message, kind: err.kind }, { status });
    }
    const because = err instanceof Error ? err.message : "unknown";
    return NextResponse.json({ error: `could not pick a release: ${because}` }, { status: 502 });
  }
}
