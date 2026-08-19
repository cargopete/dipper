import { NextResponse } from "next/server";

import { CATEGORIES, findCategory, search, thinCap } from "../../../lib/apibay";

/** The categories on offer, so the page does not duplicate the table. */
export async function GET(request: Request) {
  const params = new URL(request.url).searchParams;
  const terms = (params.get("q") ?? "").trim();

  if (params.get("catalogue") !== null) {
    return NextResponse.json({
      categories: CATEGORIES.map((category) => ({
        key: category.key,
        label: category.label,
        note: category.note,
        thinCap: thinCap(category),
      })),
    });
  }

  /* apibay has no browse: an empty query returns its no-results sentinel, which
   * would surface as "nothing matches that", which is not what happened. */
  if (!terms) {
    return NextResponse.json({ error: "type something to search for" }, { status: 400 });
  }

  const category = findCategory(params.get("category"));
  const thin = params.get("thin") === "true";
  const limit = Math.min(Math.max(Number(params.get("limit")) || 24, 1), 100);

  try {
    return NextResponse.json(await search(terms, category, thin, limit));
  } catch (err) {
    const because = err instanceof Error ? err.message : "unknown";
    return NextResponse.json({ error: `apibay search failed: ${because}` }, { status: 502 });
  }
}
