# Acceptance: Multikernel demo — userspace-driven core management

The one-kernel-per-core multikernel (`docs/MULTIKERNEL.md`), demonstrated end to end:
secondary cores boot into a minimal **parked** state and are **activated from
userspace** — by **herd**, not hardcoded in the kernel — and then host real work.

This is a **staged, expanding** playbook. Each milestone below adds a section; the
earlier ones keep passing as the later ones land.

- **Milestone 1 — core lifecycle (DONE):** secondaries boot, run their soundness
  self-tests, and **park** awaiting `MSG_CORE_INIT`; **herd** (the init system, not the
  kernel) activates the cores it intends to use via the `core_init` syscall.
- **Milestone 2 — pinned workload (DONE):** herd runs `/bin/hello` **pinned to secondary
  core 1** (its `core = 1` config). herd hands the program path to core 1's kernel in the
  activation message; core 1 fetches the ELF via forwarded `open`/`read` to core 0 and
  spawns it locally. That is `docs/MULTIKERNEL.md` §6.1 + §10 Part A and the core-aware
  scheduling in `userspace/herd/docs/CORE_AWARE_SCHEDULING.md`.
- **Milestone 3 — networking, the full demo (DONE, 2026-07-01):** `curl https://ifconfig.me`
  pinned to **core 1** prints the box's real public IP. core 1 has no net and no VFS, so DNS
  (UDP), the HTTPS TCP connection, the CA-bundle read, the curl ELF, entropy, and the wall
  clock are all forwarded to core 0 (`docs/MULTIKERNEL.md` §10 Part B / R4b.5). See the
  Milestone 3 section below.

## The model being verified

- **Kernel brings cores only to "parked."** At boot the BSP `CPU_ON`s each secondary,
  which runs its soundness self-tests (isolation, replicated `.data`/`.bss`, per-core
  PMM/heap) and then **parks** in a minimal WFI loop (`STATE_PARKED`), awaiting
  `MSG_CORE_INIT`. A core that is not initialized within a watchdog window (~120s, sized to
  comfortably exceed boot-to-herd time) logs an error and `CPU_OFF`s itself (idle cores don't
  spin); a later `core_init` re-`CPU_ON`s it. A parked core just `WFI`s, so the long window
  costs nothing and lets herd activate cleanly instead of forcing a re-bringup.
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

The service config is staged at `bootstrap/etc/herd/enabled/hello.conf`
(`command = /bin/hello`, `oneshot = true`, **`core = 1`**) → `/etc/herd/enabled/hello.conf`.
The `core = 1` key is what pins it to the secondary (Milestone 2).

### 3. Build + start the SMP VM

```bash
scripts/build_smp.sh
SMP=2 MEMORY=2048 scripts/run_smp.sh > 12_multikernel_demo_acceptance.log 2>&1
```

QEMU runs forever — do NOT block on it. Poll the log:

```bash
until grep -q "SSH Server\] Listening" 12_multikernel_demo_acceptance.log 2>/dev/null; do sleep 2; done
```

## Steps & expected result — Milestone 1 (core lifecycle: park → herd activates)

Secondaries boot, run their soundness self-tests, and **park**; herd then activates core 1
via the `core_init` syscall (carrying the program name — see Milestone 2). In the boot log
(secondary `[core N]` lines arrive via the console ring, so they interleave late):

```
[SMP] core 1 partition: base=0x… len=… MB
[SMP] core 1 PARKED (isolated-run confirmed) [replication: … PASS] [enforcement: … FAULTED PASS]
[SMP] core 1 R2: per-core pmm+heap PASS …
[core 1] parked: awaiting MSG_CORE_INIT (watchdog 120000 ms)
[herd] Userspace supervisor starting...
[SMP] core_init(1): activating (MSG_CORE_INIT sent), init program: /bin/hello
[core 1] init: scheduler + role up — ONLINE
```

`/proc/cores` reflects the lifecycle (`parked` → `online`):

```python
import subprocess
def ssh(cmd): return subprocess.run(["ssh","-o","StrictHostKeyChecking=no","-p","2222","root@localhost",cmd], capture_output=True, text=True)
print(ssh("cat /proc/cores").stdout)   # core 1 -> online after herd activates it
```

(With `MULTIKERNEL_INIT_HERD` off you'd instead see the kernel auto-init each core at boot
for its self-tests, with no herd/core_init line.) The workload that core 1 then runs is
Milestone 2.

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
crossed to core 0.

## Steps & expected result — Milestone 3 (networking: curl pinned to a secondary) — DONE

`curl https://ifconfig.me` runs pinned to **core 1** and prints the box's real public IP.
core 1 owns no network stack and no filesystem, so EVERY external dependency is forwarded to
core 0 (the Net + VFS owner) over the cross-core ring + bounce (`docs/MULTIKERNEL.md` §10
Part B / §15 R4b.5):

- **DNS** — a UDP socket + `sendto`/`recvmsg` to the nameserver in `/etc/resolv.conf` (8.8.8.8),
  serviced by core 0's smoltcp.
- **HTTPS** — `socket`/`connect`/`send`/`recv` on a TCP socket; the full TLS handshake crosses
  the bounce (4 KiB chunks).
- **Files** — the CA bundle (`/etc/ssl/certs/ca-certificates.crt`, ~189 KB) and the `curl` ELF,
  read via forwarded `openat`/`read`/`close`.
- **Entropy** — `/dev/urandom`/`getrandom` forwarded to core 0's virtio-rng (a BSP-only device).
- **Wall clock** — seeded once from core 0's RTC, so TLS cert date checks see real time (not 1970).

The whole command line rides the `core_init` activation, so `curl` receives its
`-sS https://ifconfig.me` arguments. Its stdout drains to core 0's UART via the §8.2 console ring.

### Preparation delta (host)

The service config is staged at `bootstrap/etc/herd/enabled/netcheck.conf` (`command = /bin/curl`,
`args = -sS https://ifconfig.me`, **`core = 1`**, `oneshot = true`). `bootstrap/etc/resolv.conf`
(nameserver 8.8.8.8) and `bootstrap/etc/ssl/certs/ca-certificates.crt` must be on the disk (use the
static mbedTLS `bootstrap/bin/curl`). Re-stage with `./scripts/populate_disk.sh` (or `--etc-only`
for a config-only change). Then build + boot as above (`scripts/build_smp.sh`;
`SMP=2 MEMORY=2048 scripts/run_smp.sh`).

In the boot log (core-1 lines arrive via the console ring):

```
[SMP] core_init(1): activating (MSG_CORE_INIT sent), init program: /bin/curl -sS https://ifconfig.me
[core 1] init: fetched /bin/curl (1511904 bytes) via forwarded openat/read/close; spawning EL0 process
[core 1] init: spawned /bin/curl (pid 1, tid 8) — running on this core
[syscall] connect(fd=4, ip=<ifconfig.me>:443)          # serviced on core 0
<your.public.ip.address>                                # curl stdout, drained from core 1's ring
```

**Pass criteria:** curl exits with NO error on stderr and prints a valid public IPv4 — proving
the socket + VFS forwarding path end to end, with core 1's `PMM`/`POOL`/`TALC` physically isolated
from core 0's throughout (criterion 3). Throughput is modest (4 KiB bounce chunks → many round-trips
for TLS); a shared `(offset,len)` bounce arena is the tracked §16 throughput follow-up.

> **Phase 2 (not yet done): interactive `sshd` on core 1.** `ssh -p 2323 root@localhost` into a
> sshd pinned to core 1, then run `curl` there. Beyond the socket forwarding above, this needs a PTY
> and a per-core procfs working *locally on the secondary* (sshd's shell bridge writes the login
> shell's stdin via `/proc/<pid>/fd/0`, and `<pid>` is a core-1-local process). `sshd.conf` is staged
> in `bootstrap/etc/herd/available/` (move to `enabled/` once Phase 2 lands).

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
