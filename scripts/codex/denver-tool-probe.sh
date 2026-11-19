#!/bin/sh
# End-to-end Codex exec_command in a disposable Denver QEMU guest.
# Requires a prebuilt Denver kernel with --features codex, a static musl
# BusyBox, the matching Codex binary/host, and a Codex model cache. No auth.
set -eu
KERNEL=${DENVER_KERNEL:?Set DENVER_KERNEL to a Denver kernel built with --features codex}
CODEX=${CODEX_BINARY:-$(command -v codex)}
CODEX_HOST=${CODEX_HOST_BINARY:?Set CODEX_HOST_BINARY to codex-code-mode-host}
BUSYBOX=${DENVER_BUSYBOX:?Set DENVER_BUSYBOX to a static musl BusyBox}
MODELS=${CODEX_MODELS_CACHE:?Set CODEX_MODELS_CACHE to models_cache.json}
PORT=${DENVER_FAKE_PORT:-47325}
D=$(mktemp -d "${TMPDIR:-/tmp}/denver-codex.XXXXXX")
vm= server=
cleanup() {
    [ -z "$vm" ] || kill "$vm" 2>/dev/null || true
    [ -z "$server" ] || kill "$server" 2>/dev/null || true
    [ -z "$vm" ] || wait "$vm" 2>/dev/null || true
    [ -z "$server" ] || wait "$server" 2>/dev/null || true
    rm -r "$D"
}
trap cleanup EXIT HUP INT TERM
cat > "$D/config.toml" <<CONFIG
model = "gpt-6.1-terra"
model_provider = "fake"
sandbox_mode = "danger-full-access"
approval_policy = "never"
[features]
code_mode_host = true
[model_providers.fake]
name = "fake"
base_url = "http://10.0.2.2:$PORT"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
CONFIG
truncate -s 768M "$D/root.img"
mke2fs -q -t ext2 -F "$D/root.img"
for path in /bin /tmp /proc /dev /root /root/.codex /work; do debugfs -w -R "mkdir $path" "$D/root.img" >/dev/null 2>&1; done
for pair in "$CODEX:/bin/codex" "$CODEX_HOST:/bin/codex-code-mode-host" "$BUSYBOX:/bin/busybox" "$MODELS:/root/.codex/models_cache.json" "$D/config.toml:/root/.codex/config.toml"; do
    src=${pair%%:*}; dst=${pair#*:}
    debugfs -w -R "write $src $dst" "$D/root.img" >/dev/null 2>&1
done
for path in /bin/codex /bin/codex-code-mode-host /bin/busybox; do debugfs -w -R "set_inode_field $path mode 0100755" "$D/root.img" >/dev/null 2>&1; done
debugfs -w -R 'ln /bin/busybox /bin/sh' "$D/root.img" >/dev/null 2>&1
python3 scripts/codex/fakemodel.py "$PORT" "$D/model.log" 'printf DENVER_TOOL_PROBE > /work/proof; read v < /work/proof; printf "%s" "$v"' & server=$!
qemu-system-x86_64 -M microvm -cpu max -global virtio-mmio.force-legacy=false -m 3072 -smp 1 \
    -kernel "$KERNEL" -drive "id=d0,file=$D/root.img,format=raw,if=none,snapshot=on" \
    -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    -netdev user,id=n0 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1 \
    -append 'virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 smp=1 init=/bin/codex initargs=exec,--skip-git-repo-check,-C,/work,go initenv=HOME=/root,PATH=/bin' \
    -serial mon:stdio -display none -no-reboot > "$D/guest.log" 2>&1 & vm=$!
i=0
while [ "$i" -lt 75 ]; do
    if [ -f "$D/model.log" ] && grep -q '^TOOL_OUTPUT ' "$D/model.log"; then break; fi
    if ! kill -0 "$vm" 2>/dev/null || ! kill -0 "$server" 2>/dev/null; then break; fi
    sleep 1
    i=$((i+1))
done
if ! python3 - "$D/model.log" <<'PY'
import json, sys
from pathlib import Path
path = Path(sys.argv[1])
lines = path.read_text().splitlines() if path.exists() else []
for line in lines:
    if line.startswith('TOOL_OUTPUT '):
        data = json.loads(line.removeprefix('TOOL_OUTPUT '))
        if isinstance(data, list):
            for block in data:
                try:
                    result = json.loads(block.get('text', ''))
                    if result.get('exit_code') == 0 and result.get('output') == 'DENVER_TOOL_PROBE':
                        print('RESULT PASS: Codex exec_command wrote and read a Denver guest file')
                        sys.exit(0)
                except (ValueError, AttributeError):
                    pass
print('RESULT FAIL: no successful guest tool result', file=sys.stderr)
sys.exit(1)
PY
then
    [ ! -f "$D/model.log" ] || cat "$D/model.log"
    tail -60 "$D/guest.log"
    exit 1
fi
