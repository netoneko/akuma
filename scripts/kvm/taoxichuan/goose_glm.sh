#!/bin/sh
# goose_glm.sh - talk to GLM through goose inside the Taoxichuan guest on the ryzen.
#
#   scripts/kvm/taoxichuan/goose_glm.sh                 a conversation: type a line, get a reply;
#                                                       /exit (or ^D) ends it
#   scripts/kvm/taoxichuan/goose_glm.sh 'a question'    one prompt, one answer, exit
#   TX_SESSION=name scripts/kvm/taoxichuan/goose_glm.sh resume that conversation
#
# The guest is a `herd vm-run` of a Taoxichuan kernel on the toolchain + goose image under
# Akuma's hypervisor on the ryzen, forwarded on 192.168.1.159:2224 (TX_HOST / TX_PORT
# override). The goose environment is set here rather than read from the image, so any guest
# booted from that image works. TX_KEY picks the ssh key (default: ssh's own choice).
#
# Why a conversation is a loop of one-shot `goose run`s on a named session, not one
# `goose session`: Taoxichuan's sshd gives a session a pipe, not a pty, and goose's
# interactive line editor waits for a terminal that never comes. And the hypervisor's port
# relay holds only two forwarded connections, so an interactive session left hanging locks
# everyone out of the guest; each turn here is one connection that closes when the reply ends.
set -eu
HOST=${TX_HOST:-192.168.1.159}
PORT=${TX_PORT:-2224}
ENV='export HOME=/root GOOSE_DISABLE_KEYRING=1 PATH=/usr/local/bin:/usr/local/rust/bin:/bin:/usr/bin TERM=dumb
export GOOSE_PROVIDER=openai GOOSE_MODEL=glm-5.3-flash GOOSE_THINKING_EFFORT=low GOOSE_MODE=auto GOOSE_TELEMETRY_ENABLED=false
export OPENAI_HOST=https://api.z.ai OPENAI_BASE_PATH=api/coding/paas/v4/chat/completions
cd /root'
KEY=""
[ -n "${TX_KEY:-}" ] && KEY="-i $TX_KEY"
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=20 $KEY -p $PORT root@$HOST"

# One goose turn: $1 = extra `goose run` flags, $2 = the prompt (single quotes escaped for
# the guest's shell). stdin is /dev/null so ssh never eats the next line the user types.
turn() {
    q=$(printf '%s' "$2" | sed "s/'/'\\\\''/g")
    $SSH "$ENV
goose run $1 --with-builtin developer -t '$q'" </dev/null
}

if [ $# -gt 0 ]; then
    turn "--no-session" "$*"
    exit
fi

NAME=${TX_SESSION:-mac-$(date +%Y%m%d-%H%M%S)}
RESUME=""
[ -n "${TX_SESSION:-}" ] && RESUME="--resume"
echo "goose on Taoxichuan ($HOST:$PORT), session '$NAME'. /exit or ^D ends it; TX_SESSION=$NAME resumes it later."
while printf '\nyou> ' && IFS= read -r line; do
    case "$line" in
        "") continue ;;
        /exit | /quit) break ;;
    esac
    turn "-n $NAME $RESUME" "$line" || echo "(that turn failed: rc=$?)"
    RESUME="--resume"
done
echo
