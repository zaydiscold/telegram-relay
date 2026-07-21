#!/usr/bin/env bash
# telegram-relay status monitor — run on the deploy host (mothership WSL).
set -u
cd "$(dirname "$0")/.."
echo "══════════ telegram-relay status ══════════"
echo "── service ──"
systemctl is-enabled telegram-relay 2>/dev/null | sed 's/^/  enabled: /'
systemctl is-active  telegram-relay 2>/dev/null | sed 's/^/  active:  /'
systemctl show telegram-relay -p ActiveEnterTimestamp 2>/dev/null | sed 's/^/  /'
echo "── watched routes (config.yaml) ──"
awk '/^routes:/,/^webhooks:/' config.yaml | grep -E "name:|from:" | sed 's/^/  /'
echo "── settings ──"
awk '/^(media|refresh|store):/,0' config.yaml | grep -vE "^#" | sed 's/^/  /'
echo "── tracked posts (relay.db) ──"
[ -f relay.db ] && sqlite3 relay.db "SELECT COUNT(*) || ' tracked, ' || SUM(deleted) || ' deleted' FROM relayed" 2>/dev/null | sed 's/^/  /' || echo "  (no db yet)"
echo "── last log lines ──"
journalctl -u telegram-relay -n 6 --no-pager -o short-iso 2>/dev/null | sed 's/^/  /'
echo "════════════════════════════════════════════"
