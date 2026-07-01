#!/bin/bash
# QEMU runner for `cargo run`. Accepts the kernel ELF path as $1.
#
# Env vars:
#   MEMORY    - RAM size, default 256M (e.g. MEMORY=512M cargo run --release)
#   GDB       - if set, exposes a gdbstub on :$GDB_PORT (-s)
#   GDB_WAIT  - if set, also halts the CPU at reset until gdb attaches (-S).
#               Implies GDB.
#   GDB_PORT  - gdbstub TCP port. Default: 1234 + INSTANCE.
#   INSTANCE  - integer 0..98. Shifts every host-side port so multiple
#               QEMUs can run in parallel for hang-hunting:
#                 ssh   2222 + 100*INSTANCE
#                 http  8080 + 100*INSTANCE
#                 tel   2323 + 100*INSTANCE
#                 :44   44   + 100*INSTANCE   (skipped if would collide w/ low port)
#                 :4444 4444 + 100*INSTANCE
#                 gdb   1234 + INSTANCE
#               Defaults to 0 (legacy ports unchanged). With INSTANCE>0 the
#               disk is auto-snapshotted (writes discarded) to avoid the
#               concurrent-mount corruption that two raw mounts would cause.
#   SNAPSHOT  - "1" forces snapshot=on regardless of INSTANCE.
#               "0" forces it off (DANGEROUS with parallel runs).
#   DISK      - path to disk image. Default: ./disk.img.
#   SOUND     - virtio-sound output backend on virtio-mmio-bus.3:
#                 none      (default) no audio device
#                 coreaudio audible playback on the host (macOS)
#                 wav       dump guest audio to a WAV file (CI/headless-gradeable)
#               SOUND_WAV  - output path for SOUND=wav. Default: ./qemu_audio.wav.
#
# Parallel hunt example (build once, then launch N boots in parallel —
# don't call `cargo run` N times, it would serialize on the build lock):
#   cargo build --release
#   ELF=target/aarch64-unknown-none/release/akuma
#   for i in 0 1 2 3; do
#     INSTANCE=$i GDB=1 scripts/cargo_runner.sh "$ELF" \
#       2>&1 | tee logs/daif/hunt-$i.log &
#   done; wait
#
# Attach with:
#   aarch64-elf-gdb target/aarch64-unknown-none/release/akuma \
#     -ex "target remote :$((1234 + INSTANCE))"
set -e

MEMORY="${MEMORY:-256M}"
INSTANCE="${INSTANCE:-0}"
# Number of CPUs QEMU exposes (-smp N). Default 1 so ordinary `cargo run`/`run
# --release` stays single-CPU; the multikernel build sets SMP=2+ (scripts/run_smp.sh)
# to wake secondaries via PSCI CPU_ON. With a single-core kernel, extra CPUs simply
# stay PSCI-powered-off and idle — harmless — so the gate is convention, not safety.
SMP="${SMP:-1}"
ELF="$1"
BIN="${ELF}.bin"

if ! [[ "$SMP" =~ ^[0-9]+$ ]] || [ "$SMP" -lt 1 ]; then
  echo "[cargo_runner] SMP must be a positive integer (got '$SMP')" >&2
  exit 1
fi

if ! [[ "$INSTANCE" =~ ^[0-9]+$ ]] || [ "$INSTANCE" -gt 98 ]; then
  echo "[cargo_runner] INSTANCE must be an integer 0..98 (got '$INSTANCE')" >&2
  exit 1
fi

PORT_SHIFT=$((100 * INSTANCE))
SSH_PORT=$((2222 + PORT_SHIFT))
HTTP_PORT=$((8080 + PORT_SHIFT))
MODEL_PORT=$((21434 + PORT_SHIFT))
TEL_PORT=$((2323 + PORT_SHIFT))
P44_PORT=$((44 + PORT_SHIFT))
P4444_PORT=$((4444 + PORT_SHIFT))
GDB_PORT="${GDB_PORT:-$((1234 + INSTANCE))}"

# Convert ELF to flat binary.
# The binary starts with a branch instruction (not ARM64 Image magic),
# so QEMU loads it at RAM_BASE (0x40000000) without any offset.
# Skip if $BIN is already up-to-date — keeps parallel runs from racing on
# the same output file.
if [ ! -f "$BIN" ] || [ "$ELF" -nt "$BIN" ]; then
  rust-objcopy -O binary "$ELF" "$BIN"
fi

# Size guard: catch binary bloat before it silently breaks boot.
BIN_BYTES=$(wc -c < "$BIN")
if echo "$ELF" | grep -q "/size/"; then
  SIZE_LIMIT=$((1 * 1024 * 1024))   # 1 MB for size profile
  SIZE_LABEL="1 MB"
elif echo "$ELF" | grep -q "/release-smp/"; then
  # The multikernel (cfg(kernel_smp)) compiles in the per-core bringup/scheduler code on
  # top of release; allow a little more headroom than plain release.
  SIZE_LIMIT=$((4 * 1024 * 1024))   # 4 MB for release-smp profile
  SIZE_LABEL="4 MB"
else
  SIZE_LIMIT=$((3 * 1024 * 1024))   # 3 MB for release profile
  SIZE_LABEL="3 MB"
fi
if [ "$BIN_BYTES" -gt "$SIZE_LIMIT" ]; then
  echo "[cargo_runner] ERROR: kernel binary is $(( BIN_BYTES / 1024 )) KB, exceeds ${SIZE_LABEL} limit" >&2
  exit 1
fi
echo "[cargo_runner] kernel size: $(( BIN_BYTES / 1024 )) KB (limit ${SIZE_LABEL})" >&2

GDB_ARGS=()
if [ -n "$GDB_WAIT" ]; then
  GDB_ARGS+=(-gdb "tcp::$GDB_PORT" -S)
  echo "[cargo_runner] instance=$INSTANCE gdbstub on :$GDB_PORT, CPU halted — attach gdb to start" >&2
elif [ -n "$GDB" ]; then
  GDB_ARGS+=(-gdb "tcp::$GDB_PORT")
  echo "[cargo_runner] instance=$INSTANCE gdbstub on :$GDB_PORT (kernel runs; attach any time)" >&2
fi

# Default: snapshot the disk if INSTANCE>0 so parallel boots don't corrupt disk.img.
if [ -z "$SNAPSHOT" ]; then
  if [ "$INSTANCE" -gt 0 ]; then SNAPSHOT=1; else SNAPSHOT=0; fi
fi
DISK_PATH="${DISK:-disk.img}"
DRIVE_OPTS="file=${DISK_PATH},if=none,format=raw,id=hd0"
if [ "$SNAPSHOT" = "1" ]; then
  DRIVE_OPTS="$DRIVE_OPTS,snapshot=on"
  echo "[cargo_runner] $DISK_PATH mounted snapshot=on (writes discarded)" >&2
fi

echo "[cargo_runner] instance=$INSTANCE ssh=$SSH_PORT http=$HTTP_PORT model=$MODEL_PORT tel=$TEL_PORT disk=$DISK_PATH" >&2

# Optional virtio-sound device on bus 3 (slot free; net/blk/rng take 0/1/2).
SOUND="${SOUND:-none}"
SOUND_ARGS=()
case "$SOUND" in
  none)
    ;;
  coreaudio|wav)
    if [ "$SOUND" = "wav" ]; then
      SOUND_WAV="${SOUND_WAV:-qemu_audio.wav}"
      SOUND_ARGS+=(-audiodev "wav,id=snd0,path=${SOUND_WAV}")
      echo "[cargo_runner] virtio-sound: dumping guest audio to ${SOUND_WAV}" >&2
    else
      SOUND_ARGS+=(-audiodev "coreaudio,id=snd0")
      echo "[cargo_runner] virtio-sound: audible via coreaudio" >&2
    fi
    SOUND_ARGS+=(-device "virtio-sound-device,audiodev=snd0,bus=virtio-mmio-bus.3")
    ;;
  *)
    echo "[cargo_runner] ERROR: SOUND must be none|coreaudio|wav (got '$SOUND')" >&2
    exit 1
    ;;
esac

# RUMP_NIC - second virtio-net device (NIC1) on virtio-mmio-bus.4, for the kernel
#            `rump` feature's raw L2 tap path (/dev/net/tap0). Its own isolated
#            -netdev user SLIRP gives the rump stack DHCP + a 10.0.2.2 gateway,
#            independent of NIC0 (which stays on the native smoltcp stack).
#              0 (default) no second NIC; /dev/net/tap0 stays ENODEV
#              1           add NIC1 → rump tap usable
RUMP_NIC="${RUMP_NIC:-0}"
RUMP_NIC_ARGS=()
case "$RUMP_NIC" in
  0|off|no|false|FALSE)
    ;;
  1|on|yes|true|TRUE)
    # RUMP_SSH_PORT - host port forwarded to :22 on the RUMP stack's SLIRP (net1),
    #                 so you can `ssh -p <port> root@localhost` straight into a
    #                 box whose sshd listens on the NetBSD stack (acceptance/11).
    #                 Distinct from NIC0/smoltcp's 2222 (Akuma's own sshd). Default
    #                 2223; set empty to disable the forward.
    RUMP_SSH_PORT="${RUMP_SSH_PORT:-2223}"
    if [ -n "$RUMP_SSH_PORT" ]; then
      RUMP_NIC_ARGS+=(-netdev "user,id=net1,hostfwd=tcp::${RUMP_SSH_PORT}-:22")
      echo "[cargo_runner] rump: NIC1 (net1) → /dev/net/tap0; ssh box via host :${RUMP_SSH_PORT} → rump:22" >&2
    else
      RUMP_NIC_ARGS+=(-netdev "user,id=net1")
      echo "[cargo_runner] rump: NIC1 (net1) on virtio-mmio-bus.4 → /dev/net/tap0" >&2
    fi
    RUMP_NIC_ARGS+=(-device "virtio-net-device,netdev=net1,bus=virtio-mmio-bus.4")
    ;;
  *)
    echo "[cargo_runner] ERROR: RUMP_NIC must be 0|1 (got '$RUMP_NIC')" >&2
    exit 1
    ;;
esac

# CORE2_NIC - a THIRD virtio-net (NIC2) on virtio-mmio-bus.5, dedicated to a SECONDARY core
#             so it can run a LOCAL network stack (rump) instead of forwarding sockets to
#             core 0 (docs/MULTIKERNEL_NETWORKING_EXPERIMENT.md §7, Stage 0/1). Its own
#             isolated -netdev user SLIRP gives that core's stack DHCP + a 10.0.2.2 gateway,
#             independent of NIC0 (smoltcp on core 0) and NIC1 (core 0's rump tap).
#               0 (default) no third NIC
#               1           add NIC2 on bus.5 -> the secondary's /dev/net/tap0
CORE2_NIC="${CORE2_NIC:-0}"
CORE2_NIC_ARGS=()
case "$CORE2_NIC" in
  0|off|no|false|FALSE)
    ;;
  1|on|yes|true|TRUE)
    # CORE2_HTTP_PORT - host port forwarded to :80 on NIC2's SLIRP, so a plain-HTTP GET from
    #                   the secondary's rump stack (rumphttp) can reach a server you run on the
    #                   host. Default 8081; set empty to disable the forward.
    CORE2_HTTP_PORT="${CORE2_HTTP_PORT:-8081}"
    if [ -n "$CORE2_HTTP_PORT" ]; then
      CORE2_NIC_ARGS+=(-netdev "user,id=net2,hostfwd=tcp::${CORE2_HTTP_PORT}-:80")
      echo "[cargo_runner] core2 NIC (net2) on virtio-mmio-bus.5; host :${CORE2_HTTP_PORT} -> secondary rump :80" >&2
    else
      CORE2_NIC_ARGS+=(-netdev "user,id=net2")
      echo "[cargo_runner] core2 NIC (net2) on virtio-mmio-bus.5 -> secondary rump stack" >&2
    fi
    CORE2_NIC_ARGS+=(-device "virtio-net-device,netdev=net2,bus=virtio-mmio-bus.5")
    ;;
  *)
    echo "[cargo_runner] ERROR: CORE2_NIC must be 0|1 (got '$CORE2_NIC')" >&2
    exit 1
    ;;
esac

# Accelerator. Defaults to HVF (Apple Hypervisor.framework, near-native AArch64
# execution) on Apple Silicon where it is available, falling back to TCG (portable
# software emulation, ~3000x slower for NEON) elsewhere. Override with HVF=1 to
# force it on, or HVF=0 to force TCG (e.g. for deterministic gdb crash repro — HVF
# runs on real hardware timing and is non-deterministic).
#
# HVF notes (see docs/QEMU_HVF_ISV_BUG.md):
#   - Requires -cpu host (HVF rejects -cpu max).
#   - The default kernel build uses the GICv3 driver, which is why -machine virt
#     carries gic-version=3 below; HVF only supports GICv3 anyway. A kernel built
#     with --features gic-v2 needs gic-version=2 and will NOT run under HVF.
#   - ramfb is dropped under HVF: its dirty-page tracking can trip QEMU's HVF
#     data-abort path. -display none is already set, so it is unnecessary.
HVF="${HVF:-auto}"
use_hvf=0
case "$HVF" in
  1|on|yes|true|TRUE) use_hvf=1 ;;
  0|off|no|false|FALSE) use_hvf=0 ;;
  auto|*)
    if [ "$(uname -s)" = "Darwin" ] && [ "$(uname -m)" = "arm64" ] \
       && qemu-system-aarch64 -accel help 2>/dev/null | grep -qw hvf; then
      use_hvf=1
    fi
    ;;
esac

if [ "$use_hvf" = "1" ]; then
  ACCEL_ARGS=(-accel hvf -cpu host)
  FB_ARGS=()
  echo "[cargo_runner] accelerator: HVF (-accel hvf -cpu host; ramfb disabled). HVF=0 to force TCG." >&2
else
  ACCEL_ARGS=(-accel tcg -cpu max)
  FB_ARGS=(-device ramfb)
  echo "[cargo_runner] accelerator: TCG (software emulation)." >&2
fi

echo "[cargo_runner] -smp $SMP" >&2

exec qemu-system-aarch64 \
  -semihosting \
  -machine virt,gic-version=3 \
  "${ACCEL_ARGS[@]}" \
  -smp "$SMP" \
  -m "$MEMORY" \
  -serial mon:stdio \
  -display none \
  -netdev "user,id=net0,hostfwd=tcp::${TEL_PORT}-:23,hostfwd=tcp::${SSH_PORT}-:22,hostfwd=tcp::${HTTP_PORT}-:8080,hostfwd=tcp::${P44_PORT}-:44,hostfwd=tcp::${P4444_PORT}-:4444,hostfwd=tcp::${MODEL_PORT}-:11434" \
  -global virtio-mmio.force-legacy=false \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  -drive "$DRIVE_OPTS" \
  -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2 \
  "${RUMP_NIC_ARGS[@]}" \
  "${CORE2_NIC_ARGS[@]}" \
  "${FB_ARGS[@]}" \
  "${SOUND_ARGS[@]}" \
  -kernel "$BIN" \
  "${GDB_ARGS[@]}"
