# balerion-site

Balerion's search front end, for deploying to Vercel. It finds things and hands
you a magnet. It plays nothing, and says so on the page.

## Why it cannot play anything

Balerion's engine holds long-lived TCP connections to dozens of peers, binds a
UDP port for the DHT, shells out to ffmpeg and writes gigabytes to local disk.
A serverless function has none of those: no persistent process, no UDP
listener, an ephemeral `/tmp`, an execution limit measured in seconds, and no
ffmpeg. So the engine stays on your machine and this deploys the half that is
just HTTP.

Searching apibay from a server rather than the browser is not a workaround
either, it is required: apibay sends no CORS headers, so a page cannot call it
directly. Routing through `/api/search` keeps the visitor's address off
somebody else's log as a side effect.

## The gate

One shared password, no accounts. Set it in the Vercel project settings:

```
BALERION_PASSWORD=...
```

There is deliberately no default. A deployment that forgets the variable
answers `503` to everyone rather than letting everyone in.

The cookie holds a SHA-256 of the password rather than the password, compared in
constant time. That is a modest bar and worth being honest about: it stops the
phrase leaking through a cookie jar or a proxy log, and it is not authentication
in any serious sense. One password shared by everyone who has it never is.

## Two indexes, reached two different ways

archive.org answers this deployment directly and needs nothing else running.

apibay does not. Cloudflare serves a bot challenge to datacentre addresses:
measured from Vercel's `iad1`, both our own User-Agent and a full browser one
get a `403` whose body is `<title>Just a moment...</title>`, while the same
request from a domestic connection gets a clean `200`. No header shape fixes
that, and solving the challenge would mean driving a headless browser, which is
fragile, costs money per request and is precisely what the challenge exists to
stop.

So apibay queries are forwarded to a relay running on your own machine:

```sh
balerion relay --port 8090            # token from BALERION_RELAY_TOKEN
tailscale funnel --bg 8090            # expose it over HTTPS
```

`ops/install-relay.sh` in the repository root installs that relay as a launch
agent, so it starts at login and comes back if it dies.

The relay is deliberately not the whole server. `balerion serve` has
`/api/resolve`, which downloads whatever magnet it is handed, and exposing that
to the internet would let anyone who guesses the URL fill your disk. The relay
can search and nothing else: no resolve, no file reads, no sight of the torrent
session. Every route needs a bearer token and there is no default one, so a
relay started without a token refuses to start.

Two variables point this deployment at it:

```
BALERION_RELAY_URL=https://your-machine.your-tailnet.ts.net
BALERION_RELAY_TOKEN=...
```

Half a configuration counts as none. Without both, apibay searches answer `503`
with a line saying so, and the Archive carries on working.

## The category table lives in two places

`lib/apibay.ts` carries the category table and the thin-line cap formula, ported
from the `balerion-tpb` crate. The searching itself is no longer duplicated: it
happens in the relay, and this file was cut from 314 lines to 139 when that
became true, because a second implementation of the filtering that nothing calls
is worse than none.

What remains must still agree with the Rust. If the categories or the cap change
there, change them here in the same commit. As of writing both produce an
identical cap of 675,000,000 bytes for all-video and identical tallies for the
same query.

The table is kept here rather than fetched from the relay so the menus still
render when your machine is asleep.

## Running it locally

```sh
npm install
BALERION_PASSWORD=whatever npm run dev
```

## Deploying

The app lives in a subdirectory of the Balerion repository, so either set the
project's Root Directory to `site` in the Vercel dashboard, or deploy from here:

```sh
vercel --cwd site          # preview
vercel --cwd site --prod   # production
```

Both need you to be logged in (`vercel login`) and will prompt on first run to
link a project. Set `BALERION_PASSWORD` in the project's environment variables
before the first deploy, or the site will answer `503` until you do.

One thing to watch on the first real deploy: apibay may treat requests from a
datacentre address differently from requests from your flat. That cannot be
tested from here, and if `/api/search` starts returning `502` from Vercel while
working locally, that is the reason rather than a bug in the code.
