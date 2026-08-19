#!/usr/bin/env bash
# Install the apibay relay as a launch agent, so it starts at login and comes
# back if it dies.
#
# The relay trusts OIDC tokens from one Vercel project, so there is no secret to
# generate, copy or paste. Pass the project as owner/project:
#
#   ./ops/install-relay.sh nbgn/balerion
#
# Run it again any time to reinstall.
set -euo pipefail

VERCEL_PROJECT="${1:-${VERCEL_PROJECT:-}}"
if [ -z "$VERCEL_PROJECT" ]; then
  echo "usage: $0 owner/project    (for example: nbgn/balerion)" >&2
  exit 1
fi

LABEL="com.balerion.relay"
PORT="${PORT:-8090}"
BINARY="${BINARY:-$HOME/.cargo/bin/balerion}"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOGDIR="$HOME/Library/Logs/balerion"
if [ ! -x "$BINARY" ]; then
  echo "no balerion binary at $BINARY" >&2
  echo "either 'cargo install --path crates/balerion-cli' or set BINARY=..." >&2
  exit 1
fi

mkdir -p "$LOGDIR" "$(dirname "$PLIST")"

sed -e "s|__BINARY__|$BINARY|" \
    -e "s|__PORT__|$PORT|" \
    -e "s|__VERCEL_PROJECT__|$VERCEL_PROJECT|" \
    -e "s|__LOGDIR__|$LOGDIR|" \
    "$(dirname "$0")/com.balerion.relay.plist" > "$PLIST"

# bootout first so a rerun replaces rather than duplicates. It fails when
# nothing is loaded, which is fine and not worth stopping for.
launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$UID" "$PLIST"
launchctl enable "gui/$UID/$LABEL"

sleep 1
# 401 is the right answer to an unauthenticated probe, and proves it is serving.
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$PORT/health" || true)"
if [ "$CODE" = "401" ]; then
  echo "relay is up on 127.0.0.1:$PORT and refusing unauthenticated callers, as it should"
else
  echo "relay answered $CODE on 127.0.0.1:$PORT, expected 401; see $LOGDIR/relay.err.log" >&2
  exit 1
fi

cat <<NOTE

Now expose it, once:

  tailscale funnel --bg $PORT

And tell the site where it is. There is no token to set:

  printf 'https://YOUR-MACHINE.YOUR-TAILNET.ts.net' | vercel env add BALERION_RELAY_URL production --cwd site
  vercel --cwd site --prod

Only $VERCEL_PROJECT's production deployments get in. A token from any other
project in the account is refused, and every token expires on its own.
NOTE
