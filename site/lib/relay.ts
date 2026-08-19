/* Talking to the Balerion relay on the user's own machine.
 *
 * apibay refuses datacentre addresses: Cloudflare answers this deployment with
 * a bot challenge rather than JSON, and no header shape changes that (a full
 * browser User-Agent gets the identical 403). Measured from `iad1`, not guessed.
 * A domestic connection gets a clean 200, so the search runs there and this
 * forwards to it.
 *
 * The browser cannot call the relay directly for the same reason it cannot call
 * apibay: it would need CORS headers from a host we do not control, and it would
 * put the bearer token in front of every visitor. So it goes server to server,
 * and the token never leaves Vercel. */

export type RelayConfig = { url: string; token: string };

/** Configured only when both halves are present; half a configuration is none. */
export function relayConfig(): RelayConfig | null {
  const url = process.env.BALERION_RELAY_URL?.trim();
  const token = process.env.BALERION_RELAY_TOKEN?.trim();
  if (!url || !token) return null;
  return { url: url.replace(/\/+$/, ""), token };
}

/** Why a relay call failed, in terms a page can put in front of someone. */
export type RelayFailure = "unconfigured" | "unreachable" | "refused" | "upstream";

export class RelayError extends Error {
  kind: RelayFailure;

  constructor(message: string, kind: RelayFailure) {
    super(message);
    this.name = "RelayError";
    this.kind = kind;
  }
}

/**
 * Ask the relay, and translate its failures into ones that say something.
 *
 * The distinction that matters is "your machine is not answering" against "the
 * search itself failed". The first is a laptop that is shut, asleep or off the
 * tunnel, and the fix is to open it. Reporting both as "search failed" would
 * send someone hunting for a bug in the wrong place, which is precisely what
 * happened to us with the 403.
 */
export async function relayFetch(
  path: string,
  params: URLSearchParams,
  config: RelayConfig,
  timeoutMs = 20_000,
): Promise<unknown> {
  const url = `${config.url}${path}${params.size ? `?${params}` : ""}`;

  let response: Response;
  try {
    response = await fetch(url, {
      headers: { authorization: `Bearer ${config.token}` },
      cache: "no-store",
      signal: AbortSignal.timeout(timeoutMs),
    });
  } catch (err) {
    const because = err instanceof Error ? err.name : "unknown";
    throw new RelayError(
      `your machine is not answering (${because}). Balerion's relay has to be running ` +
        `on it and exposed through the tunnel for apibay searches to work.`,
      "unreachable",
    );
  }

  if (response.status === 401) {
    throw new RelayError(
      "the relay refused this deployment's token. The value in BALERION_RELAY_TOKEN " +
        "has to match the one the relay was started with.",
      "refused",
    );
  }
  if (!response.ok) {
    // The relay forwards apibay's own complaints, which are worth passing on
    // verbatim rather than flattening into "something went wrong".
    const body = (await response.json().catch(() => null)) as { error?: string } | null;
    throw new RelayError(body?.error ?? `the relay returned ${response.status}`, "upstream");
  }

  return response.json();
}
