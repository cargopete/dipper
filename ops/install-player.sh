#!/usr/bin/env bash
# Keep the player running at 127.0.0.1:8080, so the search site's Watch button
# always has something to open.
#
#   ./ops/install-player.sh
#
# Undo with:
#
#   launchctl bootout "gui/$UID/com.balerion.serve"
#   rm ~/Library/LaunchAgents/com.balerion.serve.plist
set -euo pipefail

LABEL="com.balerion.serve"
PORT="${PORT:-8080}"
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
    -e "s|__LOGDIR__|$LOGDIR|" \
    "$(dirname "$0")/com.balerion.serve.plist" > "$PLIST"

# Anything already listening on the port has to go first, or the agent will
# respawn into a bind failure for ever.
pkill -f "balerion serve" 2>/dev/null || true
launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$UID" "$PLIST"
launchctl enable "gui/$UID/$LABEL"

for _ in $(seq 1 20); do
  if curl -sf --max-time 2 -o /dev/null "http://127.0.0.1:$PORT/api/shelves"; then
    echo "player is up on http://127.0.0.1:$PORT and will start at login"
    exit 0
  fi
  sleep 1
done

echo "player did not answer on 127.0.0.1:$PORT; see $LOGDIR/serve.err.log" >&2
exit 1
