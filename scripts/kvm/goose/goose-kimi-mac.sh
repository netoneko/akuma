#!/bin/sh
# goose on the Mac against Kimi Code: k3-256k, low thinking effort, no permission prompts
# (GOOSE_MODE=auto approves every tool call).
#
# Same wiring as /usr/local/bin/goose-kimi on the ryzen (OpenAI-compatible endpoint), but with its
# own config dir so ~/.config/goose is never touched. The key is read from crush's config at launch
# and written 0600 to that dir; it is never printed. Usage: goose-kimi-mac.sh [goose args...]
# (default: `goose session`). Bump effort for one session: GOOSE_THINKING_EFFORT=high goose-kimi-mac.sh
set -eu
CFG=${GOOSE_KIMI_HOME:-$HOME/.config-goose-kimi}
mkdir -p "$CFG/goose"
umask 077
python3 -I - "$CFG/goose/secrets.yaml" <<'PY'
import json, os, sys
c = json.load(open(os.path.expanduser("~/.local/share/crush/crush.json")))
k = c["providers"]["kimi-coding"]["api_key"]
open(sys.argv[1], "w").write("OPENAI_API_KEY: %s\n" % k)
PY
cat > "$CFG/goose/config.yaml" <<'Y'
GOOSE_TELEMETRY_ENABLED: false
Y
XDG_CONFIG_HOME=$CFG; export XDG_CONFIG_HOME
GOOSE_DISABLE_KEYRING=1; export GOOSE_DISABLE_KEYRING
GOOSE_MODE=auto; export GOOSE_MODE
GOOSE_PROVIDER=openai; export GOOSE_PROVIDER
GOOSE_MODEL=${GOOSE_MODEL:-k3-256k}; export GOOSE_MODEL
GOOSE_THINKING_EFFORT=${GOOSE_THINKING_EFFORT:-low}; export GOOSE_THINKING_EFFORT
OPENAI_HOST=https://api.kimi.com; export OPENAI_HOST
OPENAI_BASE_PATH=coding/v1/chat/completions; export OPENAI_BASE_PATH
# `serve` is secured with the owner's key (clients send it as X-Secret-Key; the driver scripts take
# GOOSE_ACP_KEY_FILE=~/.akuma/goose/acp_secret_key).
KEYFILE=${GOOSE_ACP_KEY_FILE:-$HOME/.akuma/goose/acp_secret_key}
if [ -r "$KEYFILE" ]; then GOOSE_SERVER__SECRET_KEY=$(cat "$KEYFILE"); export GOOSE_SERVER__SECRET_KEY; fi
[ $# -eq 0 ] && set -- session
exec goose "$@"
