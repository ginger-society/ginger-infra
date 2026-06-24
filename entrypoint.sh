#!/bin/sh
set -e

# ── auth ──────────────────────────────────────────────────────────────────────
AUTH_SRC="/var/run/ginger-society/auth.json"
AUTH_DST="/tmp/.ginger-society/auth.json"

if [ -f "$AUTH_SRC" ]; then
  mkdir -p "$(dirname "$AUTH_DST")"
  cp "$AUTH_SRC" "$AUTH_DST"
  echo "[runner] auth.json installed"
else
  echo "[runner] WARNING: $AUTH_SRC not found" >&2
fi

# Also set HOME to /tmp so ginger-infra finds it via the default path
export HOME=/tmp

# ── script ────────────────────────────────────────────────────────────────────
if [ -z "$REMOTE_SCRIPT" ]; then
  echo "[runner] REMOTE_SCRIPT is not set" >&2
  exit 1
fi

printf '%s\n' "$REMOTE_SCRIPT" > /tmp/script.sh
chmod +x /tmp/script.sh

# ── cleanup (optional) ────────────────────────────────────────────────────────
CLEANUP_ARG=""
if [ -n "$REMOTE_CLEANUP" ]; then
  printf '%s\n' "$REMOTE_CLEANUP" > /tmp/cleanup.sh
  chmod +x /tmp/cleanup.sh
  CLEANUP_ARG="--cleanup /tmp/cleanup.sh"
fi

# ── run ───────────────────────────────────────────────────────────────────────
CAPABILITY="${REMOTE_CAPABILITY:-unix}"
echo "[runner] capability=${CAPABILITY}"

exec ginger-infra rpc \
  --envrc /dev/null \
  --script /tmp/script.sh \
  --capability "$CAPABILITY" \
  $CLEANUP_ARG