#!/bin/sh
# entrypoint.sh — runner image entry point
#
# 1. Copies auth.json from the Secret mounted at /var/run/ginger-society/
#    to ~/.ginger-society/auth.json (where ginger-infra expects it).
# 2. Writes REMOTE_SCRIPT / REMOTE_CLEANUP from env to /tmp.
# 3. Delegates to: ginger-infra rpc --envrc /dev/null --script /tmp/script.sh …
#
# The Secret is mounted by the TaskRun spec generated in taskrun.rs.
# Create it once in the cluster:
#
#   kubectl create secret generic ginger-society-auth \
#     --from-literal=auth.json='{"API_TOKEN":"<your-token>"}' \
#     -n <namespace>

set -e

# ── auth ──────────────────────────────────────────────────────────────────────
AUTH_SRC="/var/run/ginger-society/auth.json"
AUTH_DST="$HOME/.ginger-society/auth.json"

if [ -f "$AUTH_SRC" ]; then
  mkdir -p "$(dirname "$AUTH_DST")"
  cp "$AUTH_SRC" "$AUTH_DST"
  echo "[runner] auth.json installed from mounted secret"
else
  echo "[runner] WARNING: $AUTH_SRC not found — ginger-infra may fail to authenticate" >&2
fi

# ── script ────────────────────────────────────────────────────────────────────
if [ -z "$REMOTE_SCRIPT" ]; then
  echo "[runner] REMOTE_SCRIPT is not set — nothing to run" >&2
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