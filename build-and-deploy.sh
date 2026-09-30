#!/usr/bin/env bash
#
# build-and-deploy.sh — build the llmproxy image locally, ship it to a host via
# `docker save | ssh docker load`, and run it there on the host network using
# that host's own ~/.llmproxy/config.toml (mounted read-only).
#
# Defaults: HOST=sesame, config=~/.llmproxy/config.toml (on the host), proxy
# binds 0.0.0.0 at the port named in the config (8080 unless overridden), the
# loopback admin (11180) stays loopback, the container restarts always, and the
# proxy port is opened on the host firewall.
#
# Examples:
#   ./build-and-deploy.sh                 # deploy to sesame
#   ./build-and-deploy.sh -h otherbox -p 9000
#   HOST=sesame IMAGE=llmproxy:edge ./build-and-deploy.sh --no-build
set -euo pipefail

HOST="${HOST:-sesame}"
IMAGE="${IMAGE:-llmproxy:latest}"
CONTAINER="${CONTAINER:-llmproxy}"
PROXY_PORT="${PROXY_PORT:-}"          # empty -> auto-detect from the host config
RESTART="${RESTART:-always}"          # survive reboots
FIREWALL="${FIREWALL:-1}"             # 1 opens the proxy port, keeps admin loopback
BUILD="${BUILD:-1}"                   # 1 builds locally before shipping
DOCKER="${DOCKER:-docker}"            # set to "sudo docker" if needed
CONFIG_REL=".llmproxy/config.toml"    # host-relative config, per product contract
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_CONTEXT="$REPO_DIR"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--host)       HOST="$2"; shift 2 ;;
    -i|--image)      IMAGE="$2"; shift 2 ;;
    -c|--container)  CONTAINER="$2"; shift 2 ;;
    -p|--proxy-port) PROXY_PORT="$2"; shift 2 ;;
    --no-build)      BUILD=0; shift ;;
    --no-firewall)   FIREWALL=0; shift ;;
    --restart)       RESTART="$2"; shift 2 ;;
    --help)          usage 0 ;;
    *)               die "unknown argument: $1 (try --help)" ;;
  esac
done

if [[ -n "$PROXY_PORT" ]]; then
  [[ "$PROXY_PORT" =~ ^[0-9]+$ ]] && (( PROXY_PORT >= 1 && PROXY_PORT <= 65535 )) \
    || die "invalid --proxy-port: $PROXY_PORT"
fi

# --- 1. build locally --------------------------------------------------------
if [[ "$BUILD" == "1" ]]; then
  log "building $IMAGE from $BUILD_CONTEXT"
  "$DOCKER" build -t "$IMAGE" "$BUILD_CONTEXT"
else
  log "skipping build (using existing $IMAGE)"
  "$DOCKER" image inspect "$IMAGE" >/dev/null 2>&1 || die "image $IMAGE not found locally"
fi

# --- 2. confirm the host config exists and learn the proxy port --------------
log "checking $HOST:~/$CONFIG_REL"
ssh "$HOST" 'test -f "$HOME/'"$CONFIG_REL"'"' \
  || die "missing \$HOME/$CONFIG_REL on $HOST"
PROXY_PORT_REMOTE="$(
  ssh "$HOST" \
    'f="$HOME/'"$CONFIG_REL"'"; sed -n "s/^[[:space:]]*listen_addr[[:space:]]*=[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$f" | head -n1' \
  || true
)"

if [[ -z "$PROXY_PORT" ]]; then
  CAND="${PROXY_PORT_REMOTE##*:}"
  [[ "$CAND" =~ ^[0-9]+$ ]] && [[ "$CAND" -ge 1 && "$CAND" -le 65535 ]] \
    && PROXY_PORT="$CAND" || PROXY_PORT=8080
  log "proxy port from host config: ${PROXY_PORT_REMOTE:-<none>} -> :$PROXY_PORT"
fi
ADMIN_PORT="${ADMIN_PORT:-11180}"

# --- 3. ship the image (save | load) -----------------------------------------
log "shipping $IMAGE to $HOST (docker save | ssh docker load)"
"$DOCKER" save "$IMAGE" | ssh "$HOST" "$DOCKER load"

# --- 4. (re)start the container on the host network --------------------------
log "starting container '$CONTAINER' on $HOST"
ssh "$HOST" bash -s <<REMOTE
set -euo pipefail
uid=\$(id -u); gid=\$(id -g)
mkdir -p "\$HOME/.llmproxy/logs"
$DOCKER rm -f "$CONTAINER" >/dev/null 2>&1 || true
$DOCKER run -d \\
  --name "$CONTAINER" \\
  --restart "$RESTART" \\
  --network host \\
  --user "\$uid:\$gid" \\
  -e HOME=/home/llmproxy \\
  -e PROXY_PORT="$PROXY_PORT" \\
  -e RUST_LOG="\${RUST_LOG:-info}" \\
  -v "\$HOME/$CONFIG_REL:/config/config.toml:ro" \\
  -v "\$HOME/.llmproxy/logs:/home/llmproxy/.llmproxy" \\
  "$IMAGE" --config /config/config.toml --listen 0.0.0.0:$PROXY_PORT
echo "container: \$($DOCKER ps --filter name="$CONTAINER" --format '{{.Status}}')"
REMOTE

# --- 5. enable the proxy port on the host firewall (admin stays loopback) ----
if [[ "$FIREWALL" == "1" ]]; then
  log "opening proxy port $PROXY_PORT/tcp on $HOST (admin $ADMIN_PORT left loopback-only)"
  ssh "$HOST" PROXY_PORT="$PROXY_PORT" bash -s <<'REMOTE'
set -e
SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"
if command -v ufw >/dev/null 2>&1; then
  $SUDO ufw allow "${PROXY_PORT}/tcp" || true
elif command -v firewall-cmd >/dev/null 2>&1; then
  $SUDO firewall-cmd --permanent --add-port="${PROXY_PORT}/tcp" >/dev/null \
    && $SUDO firewall-cmd --reload >/dev/null || true
elif command -v iptables >/dev/null 2>&1; then
  $SUDO iptables -I INPUT -p tcp --dport "${PROXY_PORT}" -j ACCEPT || true
else
  echo "no ufw/firewalld/iptables found; leaving firewall untouched" >&2
fi
REMOTE
else
  log "firewall step skipped (--no-firewall)"
fi

log "done. proxy http://$HOST:$PROXY_PORT  admin (SSH tunnel) http://localhost:$ADMIN_PORT"
echo "  logs: $HOST:~/.llmproxy/logs   config: $HOST:~/$CONFIG_REL"
