import { NextResponse } from "next/server";

import { COOKIE, configuredPassword, sameSecret, tokenFor } from "../../../lib/auth";

/* Deliberately unhelpful about which half was wrong, and deliberately slow to
 * answer: a gate that replies instantly is a gate you can try a few thousand
 * times a minute. Not a substitute for a real rate limit, and not pretending to
 * be one. */
const WRONG_ANSWER_DELAY_MS = 600;

export async function POST(request: Request) {
  const configured = configuredPassword();
  if (!configured) {
    return NextResponse.json({ error: "no password is configured" }, { status: 503 });
  }

  let offered = "";
  try {
    const body = (await request.json()) as { password?: unknown };
    offered = typeof body.password === "string" ? body.password : "";
  } catch {
    offered = "";
  }

  if (!sameSecret(await tokenFor(offered), await tokenFor(configured))) {
    await new Promise((resolve) => setTimeout(resolve, WRONG_ANSWER_DELAY_MS));
    return NextResponse.json({ error: "that is not the password" }, { status: 401 });
  }

  const response = NextResponse.json({ ok: true });
  response.cookies.set({
    name: COOKIE,
    value: await tokenFor(configured),
    httpOnly: true,
    sameSite: "lax",
    // Vercel serves https; a local `next dev` does not, and a Secure cookie
    // would silently never be stored there.
    secure: process.env.NODE_ENV === "production",
    path: "/",
    maxAge: 60 * 60 * 24 * 30,
  });
  return response;
}
