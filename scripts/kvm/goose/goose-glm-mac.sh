#!/bin/sh
# goose on the Mac against z.ai GLM: glm-5.3-flash, low thinking effort, no permission prompts.
# Sibling of goose-kimi-mac.sh (same shape, own config dir ~/.config-goose-glm so ~/.config/goose is never touched).
# The z.ai token is read from ~/.akuma/z.ai/token and written 0600 to that dir; it is never printed.
# Usage: goose-glm-mac.sh [goose args...]   (default: `goose session`; planning server: `serve --port 3286`)
set -eu
CFG=${GOOSE_GLM_HOME:-$HOME/.config-goose-glm}
mkdir -p "$CFG/goose"
umask 077
printf 'OPENAI_API_KEY: %s\n' "$(cat "$HOME/.akuma/z.ai/token")" > "$CFG/goose/secrets.yaml"
printf 'GOOSE_TELEMETRY_ENABLED: false\n' > "$CFG/goose/config.yaml"
XDG_CONFIG_HOME=$CFG; export XDG_CONFIG_HOME
GOOSE_DISABLE_KEYRING=1; export GOOSE_DISABLE_KEYRING
GOOSE_MODE=auto; export GOOSE_MODE
GOOSE_PROVIDER=openai; export GOOSE_PROVIDER
GOOSE_MODEL=${GOOSE_MODEL:-glm-5.3-flash}; export GOOSE_MODEL
GOOSE_THINKING_EFFORT=${GOOSE_THINKING_EFFORT:-low}; export GOOSE_THINKING_EFFORT
OPENAI_HOST=https://api.z.ai; export OPENAI_HOST
OPENAI_BASE_PATH=api/coding/paas/v4/chat/completions; export OPENAI_BASE_PATH
KEYFILE=${GOOSE_ACP_KEY_FILE:-$HOME/.akuma/goose/acp_secret_key}
if [ -r "$KEYFILE" ]; then GOOSE_SERVER__SECRET_KEY=$(cat "$KEYFILE"); export GOOSE_SERVER__SECRET_KEY; fi
[ $# -eq 0 ] && set -- session
exec goose "$@"
