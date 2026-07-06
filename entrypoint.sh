#!/bin/sh
set -e

# ── HOME ──────────────────────────────────────────────────────────────────────
export HOME=/tmp

# ── Install auth.json for ginger-infra rpc (local use in this pod) ───────────
#
# ginger-infra rpc reads $HOME/.ginger-society/auth.json to authenticate its
# POST to the external-executor API. This is separate from the credentials we
# forward to the device — this is just for the rpc binary itself running here
# in the runner pod.
GINGER_AUTH_PATH="${GINGER_AUTH_PATH:-/var/run/ginger-society/auth.json}"

if [ -f "$GINGER_AUTH_PATH" ]; then
  mkdir -p "$HOME/.ginger-society"
  cp "$GINGER_AUTH_PATH" "$HOME/.ginger-society/auth.json"
  echo "[runner] auth.json installed for ginger-infra rpc (from $GINGER_AUTH_PATH)"
else
  echo "[runner] WARNING: $GINGER_AUTH_PATH not found — ginger-infra rpc will fail to authenticate" >&2
  exit 1
fi

# ── Encode credentials for forwarding to the device ──────────────────────────
#
# ARCHITECTURE:
#   runner pod  →  ginger-infra rpc  →  executor  →  WAMP execute  →  device
#
# Credential files exist only in this pod. We base64-encode each one into a
# CRED_* env var so it flows through the envrc → RunJobRequest.env →
# ExecuteArgs.env → device bash subprocess, where the device script reconstructs
# them by sourcing ~/.ginger-society/hooks/rpc_creds.sh

CREDS_ROOT="$(dirname "$(dirname "$GINGER_AUTH_PATH")")"  # /workspace/creds

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

encode_cred CRED_AUTH_JSON     "$CREDS_ROOT/ginger-society/auth.json"
encode_cred CRED_SSH_KEY       "$CREDS_ROOT/ssh/id_ed25519"
encode_cred CRED_SSH_KEY_PUB   "$CREDS_ROOT/ssh/id_ed25519.pub"
encode_cred CRED_SSH_CERT      "$CREDS_ROOT/ssh/id_ed25519-cert.pub"
encode_cred CRED_DOCKER_CONFIG "$CREDS_ROOT/docker/config.json"
encode_cred CRED_NPMRC         "$CREDS_ROOT/.npmrc"
encode_cred CRED_PYPIRC        "$CREDS_ROOT/.pypirc"

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
SKIP_VARS="REMOTE_SCRIPT REMOTE_CAPABILITY REMOTE_CLEANUP EXTERNAL_EXECUTOR_URL GINGER_AUTH_PATH HOME PATH PWD HOSTNAME done"

ENVRC=/tmp/.envrc
: > "$ENVRC"

env | while IFS= read -r line; do
  key=${line%%=*}
  value=${line#*=}

  case "$key" in
    *[!A-Za-z0-9_]*|'') continue ;;
    KUBERNETES_*) continue ;;
    TEKTON_*) continue ;;
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