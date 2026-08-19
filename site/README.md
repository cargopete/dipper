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

## The search rules live in two places

`lib/apibay.ts` is a deliberate port of the `balerion-tpb` crate: video
categories only, never `cat=0` (which reaches the adult categories whatever you
searched for), the no-results sentinel handled, zero-seeder results dropped and
counted, and the thin-line size cap derived from 1.5 Mbit/s times the category's
typical runtime.

The two implementations must agree. If the Rust changes those numbers, change
them here in the same commit. As of writing, both return an identical cap of
675,000,000 bytes for all-video and identical tallies for the same query.

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
