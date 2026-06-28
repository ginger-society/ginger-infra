#!/bin/sh
set -e

# ── auth ──────────────────────────────────────────────────────────────────────
# GINGER_AUTH_PATH is set by the controller to point at the auth.json that
# init-credentials wrote to the shared creds workspace:
#   /workspace/creds/ginger-society/auth.json
#
# For backwards-compat / standalone RemoteTask (no pipeline workspace),
# fall back to the old Secret-mount location.
GINGER_AUTH_PATH="${GINGER_AUTH_PATH:-/var/run/ginger-society/auth.json}"
CREDS_ROOT="$(dirname "$(dirname "$GINGER_AUTH_PATH")")"   # /workspace/creds

AUTH_DST="$HOME/.ginger-society/auth.json"

if [ -f "$GINGER_AUTH_PATH" ]; then
  mkdir -p "$(dirname "$AUTH_DST")"
  cp "$GINGER_AUTH_PATH" "$AUTH_DST"
  echo "[runner] auth.json installed from $GINGER_AUTH_PATH"
else
  echo "[runner] WARNING: $GINGER_AUTH_PATH not found — ginger-infra may fail to authenticate" >&2
fi

# ── SSH keys ──────────────────────────────────────────────────────────────────
# init-credentials writes the SSH certificate + key pair to
# /workspace/creds/ssh/. Copy them into ~/.ssh/ so ginger-infra can use the
# signed certificate when connecting to remote executors.
SSH_SRC="$CREDS_ROOT/ssh"
if [ -d "$SSH_SRC" ]; then
  mkdir -p "$HOME/.ssh"
  chmod 700 "$HOME/.ssh"
  for f in id_ed25519 id_ed25519.pub id_ed25519-cert.pub; do
    if [ -f "$SSH_SRC/$f" ]; then
      cp "$SSH_SRC/$f" "$HOME/.ssh/$f"
      # Private key must be 600 or ssh-agent/ginger-infra will refuse it.
      case "$f" in
        id_ed25519) chmod 600 "$HOME/.ssh/$f" ;;
        *)          chmod 644 "$HOME/.ssh/$f" ;;
      esac
    fi
  done
  echo "[runner] SSH keys installed from $SSH_SRC"
else
  echo "[runner] WARNING: $SSH_SRC not found — remote SSH auth may fail" >&2
fi

# ── Docker config ─────────────────────────────────────────────────────────────
DOCKER_SRC="$CREDS_ROOT/docker/config.json"
if [ -f "$DOCKER_SRC" ]; then
  mkdir -p "$HOME/.docker"
  cp "$DOCKER_SRC" "$HOME/.docker/config.json"
  echo "[runner] docker config installed from $DOCKER_SRC"
fi

# ── npm / pypi ────────────────────────────────────────────────────────────────
if [ -f "$CREDS_ROOT/.npmrc" ]; then
  cp "$CREDS_ROOT/.npmrc" "$HOME/.npmrc"
  echo "[runner] .npmrc installed"
fi
if [ -f "$CREDS_ROOT/.pypirc" ]; then
  cp "$CREDS_ROOT/.pypirc" "$HOME/.pypirc"
  echo "[runner] .pypirc installed"
fi

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
SKIP_VARS="REMOTE_SCRIPT REMOTE_CAPABILITY REMOTE_CLEANUP EXTERNAL_EXECUTOR_URL GINGER_AUTH_PATH HOME PATH"

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