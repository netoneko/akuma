#!/bin/sh
# goose on the Mac against z.ai GLM: glm-5.3-flash, low thinking effort, no permission prompts.
# Sibling of goose-kimi-mac.sh (same shape, own config dir ~/.config-goose-glm so ~/.config/goose is never touched).
# The z.ai token is read from ~/.akuma/z.ai/token and written 0600 to that dir; it is never printed.
# Usage: goose-glm-mac.sh [goose args...]   (default: `goose session`; planning server: `serve --port 3286`)
#
# Prefix-cache setup per docs/archive/GOOSE_GLM_LOCAL_OPTIMIZATIONS.md:
# - goose >= 1.59 keeps the <turn-context> block as its own frozen message, so every request's
#   prompt extends the previous one and z.ai's provider cache hits (billed as cache reads) instead
#   of re-billing the whole history; the script refuses to run older goose unless overridden.
# - config.yaml below trims the static prefix (extensions off, developer tools limited, no
#   session-naming / tool-call-summary side calls), which cuts the uncached tokens each turn.
set -eu
# Refuse goose < 1.59: it rewrote the first user message's <turn-context>
# (current-time, minute resolution) on every request, so the provider prefix cache
# missed each turn and the whole history re-billed. 1.59 freezes the block per turn.
gv=$(goose --version 2>/dev/null | head -n1 | tr -cd '0-9.')
gmaj=${gv%%.*}; gmin=${gv#*.}; gmin=${gmin%%.*}
if { [ -z "$gv" ] || [ "${gmaj:-0}" -lt 1 ] || { [ "${gmaj:-0}" -eq 1 ] && [ "${gmin:-0}" -lt 59 ]; }; } \
   && [ "${GOOSE_GLM_ALLOW_OLD:-0}" != 1 ]; then
  echo "goose-glm-mac: goose ${gv:-<none>} found; z.ai prefix caching needs goose >= 1.59." >&2
  echo "  fix: brew upgrade block-goose-cli" >&2
  echo "  old-goose fallbacks: GOOSE_CONTEXT_LIMIT=31999 goose-glm-mac ... (skips the <turn-context>" >&2
  echo "                       block entirely on < 1.59) or GOOSE_GLM_ALLOW_OLD=1 to run anyway" >&2
  exit 1
fi
CFG=${GOOSE_GLM_HOME:-$HOME/.config-goose-glm}
mkdir -p "$CFG/goose"
umask 077
printf 'OPENAI_API_KEY: %s\n' "$(cat "$HOME/.akuma/z.ai/token")" > "$CFG/goose/secrets.yaml"
# Trimmed prompt: fewer tool schemas and no side model calls means a smaller static
# prefix on every request (z.ai quota is token-capped, see scripts/kvm/goose/zai_usage.py).
# todo/tom stay on: with goose >= 1.59 they leave the cache chain intact.
cat > "$CFG/goose/config.yaml" <<'Y'
GOOSE_TELEMETRY_ENABLED: false
GOOSE_DISABLE_SESSION_NAMING: true
GOOSE_DISABLE_TOOL_CALL_SUMMARY: true
extensions:
  developer:
    available_tools: [shell, edit, write, tree]
  apps:
    enabled: false
  extensionmanager:
    enabled: false
  summon:
    enabled: false
  skills:
    enabled: false
  analyze:
    enabled: false
Y
XDG_CONFIG_HOME=$CFG; export XDG_CONFIG_HOME
GOOSE_DISABLE_KEYRING=1; export GOOSE_DISABLE_KEYRING
GOOSE_MODE=auto; export GOOSE_MODE
GOOSE_PROVIDER=openai; export GOOSE_PROVIDER
GOOSE_MODEL=${GOOSE_MODEL:-glm-5.3-flash}; export GOOSE_MODEL
GOOSE_THINKING_EFFORT=${GOOSE_THINKING_EFFORT:-low}; export GOOSE_THINKING_EFFORT
GOOSE_CLI_SHOW_THINKING=${GOOSE_CLI_SHOW_THINKING:-1}; export GOOSE_CLI_SHOW_THINKING
GOOSE_SHOW_FULL_OUTPUT=${GOOSE_SHOW_FULL_OUTPUT:-1}; export GOOSE_SHOW_FULL_OUTPUT
GOOSE_CLI_MIN_PRIORITY=${GOOSE_CLI_MIN_PRIORITY:-0}; export GOOSE_CLI_MIN_PRIORITY
OPENAI_HOST=https://api.z.ai; export OPENAI_HOST
OPENAI_BASE_PATH=api/coding/paas/v4/chat/completions; export OPENAI_BASE_PATH
KEYFILE=${GOOSE_ACP_KEY_FILE:-$HOME/.akuma/goose/acp_secret_key}
if [ -r "$KEYFILE" ]; then GOOSE_SERVER__SECRET_KEY=$(cat "$KEYFILE"); export GOOSE_SERVER__SECRET_KEY; fi
[ $# -eq 0 ] && set -- session
exec goose "$@"
