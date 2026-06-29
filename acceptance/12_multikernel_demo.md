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

## Steps & expected result — Milestone 2 (pinned to a secondary core) — DONE

`hello.conf` carries `core = 1`, so herd does **not** spawn `/bin/hello` on the BSP. It
hands the program path to core 1's kernel in the activation message — `core_init(1,
"/bin/hello")` — and core 1 spawns it **locally**: the loader fetches the whole ELF via
forwarded `openat`/`read`/`close` to the BSP (the VFS owner), maps it into core 1's
partition, runs it on core 1's scheduler, and its stdout drains back to core 0's console
through core 1's console ring (§8.2). There is **no cross-core spawn** — the process is
created by core 1's own kernel (docs/MULTIKERNEL.md §7/§10).

In the boot log (core-1 lines arrive via the console ring, so they interleave late):

```
[core 1] parked: awaiting MSG_CORE_INIT (watchdog 120000 ms)
[SMP] core_init(1): activating (MSG_CORE_INIT sent), init program: /bin/hello
[herd] Starting service: hello on core 1
[herd] core_init(1) requested: /bin/hello
[core 1] init: scheduler + role up — ONLINE
[core 1] init: fetched /bin/hello (72264 bytes) via forwarded openat/read/close; spawning EL0 process
[core 1] init: spawned /bin/hello (pid 1, tid 8) — running on this core
hello: started (PID 1, outputs=10, delay_ms=1000)
hello (1/10)
...
hello (10/10)
hello: done
```

`pid 1` is core 1's **own** process namespace (per-kernel-private) — not the BSP's pid
space. The process runs entirely on core 1's CPU/partition/scheduler; only its ELF reads
crossed to core 0. `curl` (socket-forward) is the next step — add socket arms to the
owner-side `service_forwarded_syscall` dispatcher (§10.3); no new machinery.

## Notes

- A pinned service has **no local pid** in herd: herd fires `core_init` and treats it as
  launched (oneshot → `Completed`). The process's lifecycle (output, exit/reap) is owned
  by its core's kernel, not herd. herd does not supervise/restart pinned services in this
  cut.
- A box and a non-BSP `core` are mutually exclusive (boxes are per-kernel-private state);
  herd rejects that combination and does not start the service.
- The self-`CPU_OFF` watchdog is now 120s (was 5s) so the common case is a clean herd
  activation while the core is still parked; the re-`CPU_ON`-on-`core_init` path still
  covers a core that shut down (no boot race).
- `MULTIKERNEL_INIT_HERD` (kernel) + `AUTO_START_HERD` gate whether herd or the kernel
  drives activation; the default (both on) is the userspace-driven path this demo shows.
  With them off, the BSP auto-activates each secondary at boot for its self-tests.
