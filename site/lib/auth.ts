/* The gate.
 *
 * One shared password, no accounts, no session store. The cookie carries a
 * SHA-256 of the password rather than the password itself, so a stolen cookie is
 * still a stolen credential but reading it does not hand over the phrase people
 * type. Compared in constant time, because comparing secrets with `===` leaks
 * their length and prefix through timing.
 *
 * The password lives in an environment variable and never in this repository.
 * Set BALERION_PASSWORD in the Vercel project settings; there is a deliberate
 * absence of a default, so a deployment that forgets it refuses everyone rather
 * than admitting everyone. */

export const COOKIE = "balerion_gate";

/** Runs in middleware as well as in routes, so Web Crypto rather than node:crypto. */
export async function tokenFor(password: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(password));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Constant-time string compare. Length is compared first and non-secretly. */
export function sameSecret(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/** The configured password, or null when the deployment has not set one. */
export function configuredPassword(): string | null {
  const password = process.env.BALERION_PASSWORD;
  return password && password.length > 0 ? password : null;
}
