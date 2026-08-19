import { NextResponse, type NextRequest } from "next/server";

import { COOKIE, configuredPassword, sameSecret, tokenFor } from "./lib/auth";

/* Everything except the login page, the login endpoint and Next's own assets.
 * Listed as an exclusion rather than an inclusion on purpose: a new route added
 * later is gated by default, which is the direction you want to fail in. */
export const config = {
  matcher: ["/((?!login|api/login|_next/static|_next/image|favicon.ico).*)"],
};

export async function middleware(request: NextRequest) {
  const password = configuredPassword();
  if (!password) {
    // No password configured means no way to let anyone in. Say so plainly
    // rather than falling open.
    return new NextResponse("BALERION_PASSWORD is not set on this deployment.", {
      status: 503,
      headers: { "content-type": "text/plain; charset=utf-8" },
    });
  }

  const offered = request.cookies.get(COOKIE)?.value ?? "";
  if (sameSecret(offered, await tokenFor(password))) {
    return NextResponse.next();
  }

  // An API call gets a status it can act on; a page gets the login form, with
  // where it was going remembered.
  if (request.nextUrl.pathname.startsWith("/api/")) {
    return NextResponse.json({ error: "locked" }, { status: 401 });
  }
  const login = new URL("/login", request.url);
  login.searchParams.set("next", request.nextUrl.pathname);
  return NextResponse.redirect(login);
}
