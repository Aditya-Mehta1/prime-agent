#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE="$HOME/.agent-projects"
COMMAND="$HOME/.local/bin/agent-projects"
launcher() { printf '#!/usr/bin/env bash\nexec %q "$@"\n' "$ROOT/agent-projects.sh"; }
if [[ -e "$COMMAND" || -L "$COMMAND" ]] && ! cmp -s "$COMMAND" <(launcher); then
  echo "$COMMAND already exists and points elsewhere; leaving it unchanged." >&2
  exit 1
fi
if [[ -z "${SAIL_API_KEY:-}" && ! -s "$STATE/sail-api-key" ]]; then
  echo "Set SAIL_API_KEY in your environment before the first install." >&2
  exit 1
fi

(cd "$ROOT" && npm ci)

umask 077
mkdir -p "$STATE"
if [[ -n "${SAIL_API_KEY:-}" ]]; then
  KEY_FILE="$(mktemp "$STATE/.sail-key.XXXXXX")"
  trap 'rm -f "$KEY_FILE"' EXIT
  printf '%s' "$SAIL_API_KEY" > "$KEY_FILE"
  mv -f "$KEY_FILE" "$STATE/sail-api-key"
  trap - EXIT
fi
if [[ ! -e "$STATE/settings.json" ]]; then
  printf '%s\n' '{"defaultProvider":"sail-asap","defaultModel":"zai-org/GLM-5.3-Flash","subagentDefaultModel":"sail/zai-org/GLM-5.3-Flash"}' > "$STATE/settings.json"
fi
mkdir -p "$HOME/.local/bin"
if [[ ! -e "$COMMAND" ]]; then
  (set -C; launcher > "$COMMAND")
fi
chmod 755 "$COMMAND"
echo "Ready. Run agent-projects from your project directory."
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo 'Add ~/.local/bin to your shell PATH: export PATH="$HOME/.local/bin:$PATH"' ;;
esac
