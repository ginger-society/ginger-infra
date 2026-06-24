#!/bin/sh
# entrypoint.sh — runner image entry point
#
# Reads REMOTE_SCRIPT (and optionally REMOTE_CLEANUP) from the environment
# (injected by the controller into the Tekton TaskRun step spec), writes them
# to /tmp, then delegates to `ginger-infra rpc`.
#
# All user-declared env vars are already present in the environment because
# Kubernetes injected them from the TaskRun step spec (literal values and
# secretKeyRef entries alike). ginger-infra rpc passes them through to the
# sidekick via the .envrc mechanism — here we write a minimal .envrc by
# dumping the current environment so nothing is lost.
#
# Note: we use --envrc /dev/null because the env is already inherited;
# the sidekick receives it via the RunJobRequest payload built in rpc.rs.

set -e

# ── write script ──────────────────────────────────────────────────────────────
if [ -z "$REMOTE_SCRIPT" ]; then
  echo "[runner] REMOTE_SCRIPT is not set — nothing to run" >&2
  exit 1
fi

printf '%s\n' "$REMOTE_SCRIPT" > /tmp/script.sh
chmod +x /tmp/script.sh

# ── write cleanup script (optional) ──────────────────────────────────────────
CLEANUP_ARG=""
if [ -n "$REMOTE_CLEANUP" ]; then
  printf '%s\n' "$REMOTE_CLEANUP" > /tmp/cleanup.sh
  chmod +x /tmp/cleanup.sh
  CLEANUP_ARG="--cleanup /tmp/cleanup.sh"
fi

# ── capability (default: unix) ────────────────────────────────────────────────
CAPABILITY="${REMOTE_CAPABILITY:-unix}"

# ── run ───────────────────────────────────────────────────────────────────────
echo "[runner] executing capability=${CAPABILITY} script=/tmp/script.sh"

exec ginger-infra rpc \
  --envrc /dev/null \
  --script /tmp/script.sh \
  --capability "$CAPABILITY" \
  $CLEANUP_ARG