#!/usr/bin/env bash
# Safe helper for the local Omarchy/Arch Docker deployment.
# It never edits data, never deletes containers, and never prints .env secrets.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

usage() {
  cat <<'USAGE'
Usage: scripts/omarchy-agent2api.sh <command>

Commands:
  up        Start the compose service (data preserved)
  down      Remove container/network (data preserved)
  restart   Restart the service
  status    Show container state, loopback bind and /health
  logs      Follow logs
  health    Call /health only
  backup    Stop service, tar ./data, start again; writes backups/*.tar.gz
USAGE
}

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing command: $1" >&2; exit 1; }
}

port() {
  # Read only the public port from compose; never source .env into the shell.
  docker compose config 2>/dev/null | awk '/published:/ {print $2; exit}' | tr -d '"'
}

cmd="${1:-}"
case "$cmd" in
  up)
    require docker
    docker compose up -d
    docker compose ps
    ;;
  down)
    require docker
    docker compose down
    ;;
  restart)
    require docker
    docker compose restart
    docker compose ps
    ;;
  status)
    require docker
    docker compose ps
    p="$(port)"
    echo "published_port=${p:-unknown}"
    ss -ltnp 2>/dev/null | grep -E ":${p:-__none__}\\b" || true
    if [ -n "${p:-}" ]; then
      curl -fsS --max-time 5 "http://127.0.0.1:${p}/health" && echo
    fi
    ;;
  logs)
    require docker
    docker compose logs -f --tail=200
    ;;
  health)
    p="$(port)"
    [ -n "$p" ] || { echo "could not read published port from compose" >&2; exit 1; }
    curl -fsS --max-time 5 "http://127.0.0.1:${p}/health" && echo
    ;;
  backup)
    require docker
    require tar
    mkdir -p backups
    chmod 700 backups
    stamp="$(date +%Y%m%d-%H%M%S)"
    out="backups/agent2api-data-${stamp}.tar.gz"
    started=0
    cleanup() {
      if [ "$started" -eq 0 ]; then
        docker compose start >/dev/null 2>&1 || true
      fi
    }
    trap cleanup EXIT INT TERM
    docker compose stop
    started=1
    tar -C . -czf "$out" data
    chmod 600 "$out"
    docker compose start
    started=0
    trap - EXIT INT TERM
    echo "backup written: $out"
    ;;
  ""|-h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
