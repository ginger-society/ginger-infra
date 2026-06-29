#!/bin/sh
set -e

# ── HOME ──────────────────────────────────────────────────────────────────────
export HOME=/tmp

# ── ARCHITECTURE NOTE ─────────────────────────────────────────────────────────
#
#   runner pod  →  ginger-infra rpc  →  sidekick  →  WAMP "execute"  →  device
#
# Credential files in /workspace/creds exist ONLY in the cluster pod.
# The device receives only env vars (via ExecuteArgs.env).
#
# Strategy: base64-encode every credential file and inject it as a CRED_*
# env var. Also inject RPC_JOB_ID (the WAMP job_id) so the device script
# can reconstruct the files at a unique, collision-safe path:
#   /tmp/rpc/<RPC_JOB_ID>/{ginger-society/auth.json, .ssh/, .docker/, ...}
#
# The device script calls the helper at the top:
#   eval "$(reconstruct_rpc_creds)"   # or source a helper written by this runner

GINGER_AUTH_PATH="${GINGER_AUTH_PATH:-/var/run/ginger-society/auth.json}"
CREDS_ROOT="$(dirname "$(dirname "$GINGER_AUTH_PATH")")"  # /workspace/creds

# ── encode one credential file into an env var ────────────────────────────────
# Usage: encode_cred VAR_NAME /path/to/file
encode_cred() {
  var_name="$1"
  file_path="$2"
  if [ -f "$file_path" ] && [ -r "$file_path" ]; then
    encoded=$(base64 < "$file_path" | tr -d '\n')
    eval "export ${var_name}='${encoded}'"
    echo "[runner] encoded $file_path → \$$var_name (${#encoded} chars b64)"
  else
    echo "[runner] WARNING: $file_path not found or unreadable — skipping $var_name" >&2
  fi
}

encode_cred CRED_AUTH_JSON       "$CREDS_ROOT/ginger-society/auth.json"
encode_cred CRED_SSH_KEY         "$CREDS_ROOT/ssh/id_ed25519"
encode_cred CRED_SSH_KEY_PUB     "$CREDS_ROOT/ssh/id_ed25519.pub"
encode_cred CRED_SSH_CERT        "$CREDS_ROOT/ssh/id_ed25519-cert.pub"
encode_cred CRED_DOCKER_CONFIG   "$CREDS_ROOT/docker/config.json"
encode_cred CRED_NPMRC           "$CREDS_ROOT/.npmrc"
encode_cred CRED_PYPIRC          "$CREDS_ROOT/.pypirc"

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

# ── envrc ─────────────────────────────────────────────────────────────────────
# CRED_* vars and RPC_JOB_ID will be picked up here automatically since they
# are now in the environment. Controller-only vars that must NOT reach the device:
SKIP_VARS="REMOTE_SCRIPT REMOTE_CAPABILITY REMOTE_CLEANUP EXTERNAL_EXECUTOR_URL GINGER_AUTH_PATH HOME PATH"

ENVRC=/tmp/.envrc
: > "$ENVRC"

env | while IFS='=' read -r key value; do
  case "$key" in
    *[!A-Za-z0-9_]*|'') continue ;;
  esac
  skip=0
  for s in $SKIP_VARS; do
    [ "$key" = "$s" ] && skip=1 && break
  done
  [ "$skip" = "1" ] && continue
  escaped=$(printf '%s' "$value" | sed "s/'/'\\\\''/g")
  printf "export %s='%s'\n" "$key" "$escaped" >> "$ENVRC"
done

echo "[runner] envrc written: $(wc -l < "$ENVRC") var(s)"

# ── run ───────────────────────────────────────────────────────────────────────
CAPABILITY="${REMOTE_CAPABILITY:-unix}"
echo "[runner] capability=${CAPABILITY} executor=${EXTERNAL_EXECUTOR_URL}"

exec ginger-infra rpc \
  --envrc /tmp/.envrc \
  --script /tmp/script.sh \
  --capability "$CAPABILITY" \
  $CLEANUP_ARG