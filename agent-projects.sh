#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE="$HOME/.agent-projects"
if [[ ! -x "$ROOT/node_modules/.bin/tsx" ]]; then
  echo "Run $ROOT/install-local.sh first." >&2
  exit 1
fi
if [[ -z "${SAIL_API_KEY:-}" ]]; then
  if [[ ! -s "$STATE/sail-api-key" ]]; then
    echo "Run $ROOT/install-local.sh with SAIL_API_KEY set first." >&2
    exit 1
  fi
  export SAIL_API_KEY="$(cat "$STATE/sail-api-key")"
fi

export PRIME_AGENT_APP_NAME="agent-projects"
export PRIME_AGENT_CONFIG_DIR=".agent-projects"
export AGENT_PROJECTS_CODING_AGENT_DIR="$STATE"
export AGENT_PROJECTS_SESSION_DIR="$STATE/sessions"
export PRIME_AGENT_CODING_AGENT_DIR="$STATE"
export PRIME_AGENT_KERNEL_VENV="$STATE/kernel-venv"
unset PRIME_AGENT_KERNEL_PYTHON
export PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_REGISTRY_DIR="$STATE/supervisor-owners"
# A separate TMPDIR also scopes daemon discovery and shutdown to this demo.
export TMPDIR="$STATE/tmp"
export PI_PACKAGE_DIR="$ROOT/packages/coding-agent"
export PRIME_AGENT_LAUNCHER_PATH="$ROOT/agent-projects.sh"
export TSX_TSCONFIG_PATH="$ROOT/tsconfig.json"

umask 077
mkdir -p "$TMPDIR"
exec "$ROOT/node_modules/.bin/tsx" "$ROOT/packages/coding-agent/src/cli.ts" "$@"
