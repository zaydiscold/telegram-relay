#!/usr/bin/env bash
# Post a Discord ops alert about a failed/degraded relay unit.
#
# Called by telegram-relay-alert@.service (systemd OnFailure=). Kept as a real
# script, not inline ExecStart shell: systemd's own quote parsing mangles nested
# JSON quoting, which silently produces an alert that never fires.
#
# Usage: relay-alert.sh <unit-name> [extra message]
set -uo pipefail

UNIT="${1:-telegram-relay.service}"
EXTRA="${2:-}"
ENV_FILE="${ENV_FILE:-/home/zaydk/telegram-relay/.env}"

# Pull DISCORD_WEBHOOK_OPS without echoing it or exporting the whole .env.
if [[ -r "$ENV_FILE" ]]; then
  WEBHOOK="$(grep -E '^DISCORD_WEBHOOK_OPS=' "$ENV_FILE" | head -1 | cut -d= -f2-)"
fi
[[ -n "${WEBHOOK:-}" ]] || { echo "no DISCORD_WEBHOOK_OPS; nothing to do" >&2; exit 0; }

STATE="$(systemctl is-active "$UNIT" 2>&1 || true)"
SINCE="$(systemctl show "$UNIT" -p ActiveEnterTimestamp --value 2>&1 || true)"
NRESTARTS="$(systemctl show "$UNIT" -p NRestarts --value 2>&1 || true)"
TAIL="$(journalctl -u "$UNIT" -n 5 --no-pager -o cat 2>/dev/null | tail -3 | tr -d '\r')"

read -r -d '' TEXT <<EOF || true
**relay alert** — \`${UNIT}\` is \`${STATE}\` (restarts: ${NRESTARTS}, since ${SINCE})
${EXTRA}
\`\`\`
${TAIL}
\`\`\`
EOF

# jq builds the JSON so message content can never break the payload.
if command -v jq >/dev/null 2>&1; then
  PAYLOAD="$(jq -n --arg c "$TEXT" '{content: $c, allowed_mentions: {parse: []}}')"
else
  # Minimal fallback: strip characters that would break naive JSON.
  SAFE="$(printf '%s' "$TEXT" | tr '\n' ' ' | sed 's/"/\x27/g; s/\\/ /g')"
  PAYLOAD="{\"content\":\"${SAFE}\",\"allowed_mentions\":{\"parse\":[]}}"
fi

curl -sS --max-time 15 -H 'Content-Type: application/json' \
  -d "$PAYLOAD" "$WEBHOOK" >/dev/null
