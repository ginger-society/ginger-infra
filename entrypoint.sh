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

# ── envrc — forward user env vars to the remote executor ─────────────────────
# All env vars are present in the container environment. We write the ones
# that are NOT controller-managed into /tmp/.envrc so that ginger-infra rpc
# includes them in the RunJobRequest sent to the external executor.
# Controller-managed vars (consumed here in the runner, not on the remote):
SKIP_VARS="REMOTE_SCRIPT REMOTE_CAPABILITY REMOTE_CLEANUP EXTERNAL_EXECUTOR_URL HOME PATH"

ENVRC=/tmp/.envrc
: > "$ENVRC"  # truncate/create

# `env` prints NAME=VALUE lines; we export everything that isn't in SKIP_VARS.
env | while IFS='=' read -r key value; do
  # Skip vars with empty names or that contain special chars (e.g. bash funcs)
  case "$key" in
    *[!A-Za-z0-9_]*|'') continue ;;
  esac

  # Skip controller-managed vars
  skip=0
  for s in $SKIP_VARS; do
    if [ "$key" = "$s" ]; then
      skip=1
      break
    fi
  done
  [ "$skip" = "1" ] && continue

  # Write as `export NAME='VALUE'` — single-quote the value so special
  # characters (spaces, $, etc.) are preserved literally.
  # Escape any single quotes inside the value (replace ' with '\'' ).
  escaped=$(printf '%s' "$value" | sed "s/'/'\\\\''/g")
  printf "export %s='%s'\n" "$key" "$escaped" >> "$ENVRC"
done

echo "[runner] envrc written: $(wc -l < "$ENVRC") var(s)"

# ── run ───────────────────────────────────────────────────────────────────────
CAPABILITY="${REMOTE_CAPABILITY:-unix}"
echo "[runner] capability=${CAPABILITY}"

exec ginger-infra rpc \
  --envrc /tmp/.envrc \
  --script /tmp/script.sh \
  --capability "$CAPABILITY" \
  $CLEANUP_ARG