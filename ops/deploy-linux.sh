#!/usr/bin/env bash
# Build and restart everything balerion runs on this machine.
#
#   ./ops/deploy-linux.sh
#
# There are two services and they share one binary. Rebuilding and restarting
# only the one you were thinking about leaves the other running whatever it
# started with, and a long-lived process keeps its own copy of the executable:
# the file on disk changing does nothing to it.
#
# That is not hypothetical. The relay ran for two days against routes that had
# been added the day before, answering 404 to the search site while the player
# beside it was up to date, and the symptom was "the machine is not answering",
# which sent two separate investigations after the network instead of the
# process table.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

cargo install --path crates/balerion-cli --force

for unit in balerion-serve balerion-relay; do
  # Absent is fine: not every machine runs both.
  if systemctl --user list-unit-files "$unit.service" >/dev/null 2>&1 &&
     systemctl --user cat "$unit.service" >/dev/null 2>&1; then
    systemctl --user restart "$unit"
    printf '  %-16s %s\n' "$unit" "$(systemctl --user is-active "$unit")"
  fi
done

echo
echo "Both are now running the binary that was just built. Check with:"
echo "  systemctl --user show balerion-relay -p ActiveEnterTimestamp --value"
