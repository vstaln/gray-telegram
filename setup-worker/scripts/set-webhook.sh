#!/usr/bin/env bash
set -euo pipefail

url="${1:-${WEBHOOK_URL:-}}"
if [ -z "$url" ]; then
  echo "usage: WEBHOOK_SECRET=... $0 <https-webhook-url>" >&2
  exit 2
fi
case "$url" in
  https://*) ;;
  *) echo "webhook url must start with https://" >&2; exit 2 ;;
esac

secret="${WEBHOOK_SECRET:-}"
if [[ ! "$secret" =~ ^[A-Za-z0-9_-]{1,256}$ ]]; then
  echo "WEBHOOK_SECRET must be 1-256 characters from A-Z a-z 0-9 _ -" >&2
  exit 2
fi

token_file="${MANAGER_TOKEN_FILE:-$HOME/.config/gray-telegram/manager.token}"
if [ ! -r "$token_file" ]; then
  echo "cannot read bot token file: $token_file" >&2
  exit 2
fi
token="$(tr -d '[:space:]' < "$token_file")"
if [[ ! "$token" =~ ^[0-9]+:[A-Za-z0-9_-]{30,}$ ]]; then
  echo "malformed bot token in $token_file" >&2
  exit 2
fi

response="$(curl -sS -K - <<EOF
url = "https://api.telegram.org/bot${token}/setWebhook"
data-urlencode = "url=${url}"
data-urlencode = "secret_token=${secret}"
data-urlencode = "allowed_updates=[\"message\",\"managed_bot\"]"
EOF
)"

if printf '%s' "$response" | grep -q '"ok"[[:space:]]*:[[:space:]]*true'; then
  echo "webhook set"
  exit 0
fi

echo "setWebhook failed: $response" >&2
exit 1
