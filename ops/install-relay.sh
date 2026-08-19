#!/usr/bin/env bash
# Install the apibay relay as a launch agent, so it starts at login and comes
# back if it dies.
#
# Generates a token if there is not one already, and prints it once at the end:
# the site needs the same value in BALERION_RELAY_TOKEN. Run it again any time to
# reinstall; it reuses the existing token unless you pass --new-token.
set -euo pipefail

LABEL="com.balerion.relay"
PORT="${PORT:-8090}"
BINARY="${BINARY:-$HOME/.cargo/bin/balerion}"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOGDIR="$HOME/Library/Logs/balerion"
TOKEN_FILE="$HOME/.config/balerion/relay-token"

if [ ! -x "$BINARY" ]; then
  echo "no balerion binary at $BINARY" >&2
  echo "either 'cargo install --path crates/balerion-cli' or set BINARY=..." >&2
  exit 1
fi

mkdir -p "$LOGDIR" "$(dirname "$PLIST")" "$(dirname "$TOKEN_FILE")"

if [ "${1:-}" = "--new-token" ] || [ ! -s "$TOKEN_FILE" ]; then
  # 32 bytes of urandom, base64url. Long enough that guessing is not a strategy.
  python3 -c 'import secrets; print(secrets.token_urlsafe(32))' > "$TOKEN_FILE"
  chmod 600 "$TOKEN_FILE"
  echo "wrote a new token to $TOKEN_FILE"
fi
TOKEN="$(cat "$TOKEN_FILE")"

# The plist carries the token, so it is readable only by you.
sed -e "s|__BINARY__|$BINARY|" \
    -e "s|__PORT__|$PORT|" \
    -e "s|__TOKEN__|$TOKEN|" \
    -e "s|__LOGDIR__|$LOGDIR|" \
    "$(dirname "$0")/com.balerion.relay.plist" > "$PLIST"
chmod 600 "$PLIST"

# bootout first so a rerun replaces rather than duplicates. It fails when
# nothing is loaded, which is fine and not worth stopping for.
launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$UID" "$PLIST"
launchctl enable "gui/$UID/$LABEL"

sleep 1
if curl -sf --max-time 5 -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/health" >/dev/null; then
  echo "relay is up on 127.0.0.1:$PORT and answering"
else
  echo "relay did not answer on 127.0.0.1:$PORT; see $LOGDIR/relay.err.log" >&2
  exit 1
fi

cat <<NOTE

Now expose it, once:

  tailscale funnel --bg $PORT

And give the site the same token. From the repository root:

  printf '%s' "$TOKEN" | vercel env add BALERION_RELAY_TOKEN production --cwd site
  printf 'https://YOUR-MACHINE.YOUR-TAILNET.ts.net' | vercel env add BALERION_RELAY_URL production --cwd site
  vercel --cwd site --prod

The token is in $TOKEN_FILE if you need it again. Nothing prints it but this.
NOTE
