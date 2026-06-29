# Acceptance: Multikernel demo — userspace-driven core management

The one-kernel-per-core multikernel (`docs/MULTIKERNEL.md`), demonstrated end to end:
secondary cores boot into a minimal **parked** state and are **activated from
userspace** — by **herd**, not hardcoded in the kernel — and then host real work.

This is a **staged, expanding** playbook. Each milestone below adds a section; the
earlier ones keep passing as the later ones land.

- **Now (R4b.3b + core lifecycle):** secondaries park awaiting `MSG_CORE_INIT`; herd
  (the init system) activates them via a syscall; as the first workload herd runs
  `/bin/hello` once (oneshot) on the BSP.
- **Next (R4b.4/.5):** herd spawns `/bin/hello` — then `curl` — **pinned to a secondary
  core**, with the loader's `open`/`read` (and later sockets) forwarded to the owner
  core. That is `docs/MULTIKERNEL.md` §10 Part A/B and the core-aware scheduling
  in `userspace/herd/docs/CORE_AWARE_SCHEDULING.md`.

## The model being verified

- **Kernel brings cores only to "parked."** At boot the BSP `CPU_ON`s each secondary,
  which runs its soundness self-tests (isolation, replicated `.data`/`.bss`, per-core
  PMM/heap) and then **parks** in a minimal WFI loop (`STATE_PARKED`), awaiting
  `MSG_CORE_INIT`. A core that is not initialized within a ~5s watchdog window logs an
  error and `CPU_OFF`s itself (idle cores don't spin); a later `core_init` re-`CPU_ON`s it.
- **Userspace decides activation.** When `AUTO_START_HERD && MULTIKERNEL_INIT_HERD`
  (both default on), **herd** manages the cores: it calls the `core_init` syscall to
  activate the cores it intends to use. With `MULTIKERNEL_INIT_HERD` off, the kernel
  auto-initializes secondaries itself at boot (fallback for non-herd / bare SMP boots).
- **Activation is targeted + idempotent.** `MSG_CORE_INIT` is sent to a specific core;
  on receipt within the window the core stands up its scheduler/role and goes
  `STATE_ONLINE`. A late/duplicate one (already online) is logged and ignored.

## Preparation (host)

### 1. SSH authorized keys

```bash
mkdir -p bootstrap/etc/sshd/
cp ~/.ssh/id_ed25519.pub bootstrap/etc/sshd/authorized_keys
```

### 2. Build userspace (herd + hello) and stage the disk

```bash
cd userspace && ./build.sh && cd ..
./scripts/create_disk.sh
./scripts/populate_disk.sh
```

The oneshot service config is staged at `bootstrap/etc/herd/enabled/hello.conf`
(`command = /bin/hello`, `oneshot = true`) → `/etc/herd/enabled/hello.conf`.

### 3. Build + start the SMP VM

```bash
scripts/build_smp.sh
SMP=2 MEMORY=2048 scripts/run_smp.sh > 12_multikernel_demo_acceptance.log 2>&1
```

QEMU runs forever — do NOT block on it. Poll the log:

```bash
until grep -q "SSH Server\] Listening" 12_multikernel_demo_acceptance.log 2>/dev/null; do sleep 2; done
```

## Steps & expected result — Milestone 1 (core lifecycle + oneshot hello)

### A. Secondaries park, then herd activates them

In the boot log:

```
[SMP] core 1 partition: base=0x… len=… MB
[core 1] parked: awaiting MSG_CORE_INIT (watchdog … s)
[herd] Userspace supervisor starting...
[SMP] core_init(1): activating (MSG_CORE_INIT sent)
[core 1] init: scheduler + role up — ONLINE
```

(With `MULTIKERNEL_INIT_HERD` off you'd instead see the kernel auto-init the core at
boot, with no herd line.)

### B. herd runs /bin/hello once

```
[herd] Starting service: hello
[herd] Started hello (pid=…)
[herd] Service hello exited with code 0
[herd] Oneshot service hello completed
```

`herd log hello` prints hello's greeting; repeating it returns the **same** content
(run-once — not restarted). See `userspace/herd/README.md` for the oneshot semantics.

```python
import subprocess
def ssh(cmd): return subprocess.run(["ssh","-o","StrictHostKeyChecking=no","-p","2222","root@localhost",cmd], capture_output=True, text=True)
print(ssh("herd log hello").stdout)
```

## Steps & expected result — Milestone 2 (pinned to a secondary core) — PENDING

Once R4b.4 lands, `hello.conf` gains `core = 1` and the same run-once executes on the
secondary: the loader's `open`/`read` of `/bin/hello` forward to the BSP (the proven
exec-fetch path), the process runs on core 1's kernel, and its output is drained back to
core 0's console. `curl` then adds the socket-forward case. This section will be filled
in with the exact log lines when the path is wired.

## Notes

- The 5s self-`CPU_OFF` is not a failure: herd's `core_init` re-`CPU_ON`s a core that
  shut down before herd got to it, so there is no boot race — just no idle spinning.
- `MULTIKERNEL_INIT_HERD` (kernel) + `AUTO_START_HERD` gate whether herd or the kernel
  drives activation; the default (both on) is the userspace-driven path this demo shows.
