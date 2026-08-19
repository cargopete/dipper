import { NextResponse } from "next/server";

import { episodes, search, shelf } from "../../../lib/shows";

/* The catalogue: titles only.
 *
 * Served from here rather than from the browser because TVmaze is one more host
 * to keep out of the visitor's request log, and because it lets the shelf be
 * cached once for everyone rather than fetched sixteen times per visitor. */

export async function GET(request: Request) {
  const params = new URL(request.url).searchParams;

  try {
    const showId = params.get("show");
    if (showId) {
      const id = Number(showId);
      if (!Number.isInteger(id) || id <= 0) {
        return NextResponse.json({ error: "that is not a show id" }, { status: 400 });
      }
      return NextResponse.json({ episodes: await episodes(id) });
    }

    const terms = (params.get("q") ?? "").trim();
    // An empty query is a browse here, unlike apibay: the shelf is the point.
    const shows = terms ? await search(terms) : await shelf();
    return NextResponse.json({ shows, shelf: !terms });
  } catch (err) {
    const because = err instanceof Error ? err.message : "unknown";
    return NextResponse.json({ error: `the catalogue is not answering: ${because}` }, {
      status: 502,
    });
  }
}
