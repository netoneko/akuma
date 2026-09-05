# amd64 SMP: bringing the secondary cores up under one lock

**Done** 2026-09-05, in the `amd64-smp` worktree, merged to `main` the same
day. **Scope:** `amd64/`, plus two small additions in `akuma-multiboot2` and
`akuma-ryzen-amd64` so the GRUB path can find its MADT (§3.7). **Status:**
every self-test passes at `SMP=1` (198), `SMP=2` and `SMP=4` (207 — nine new
SMP checks) under QEMU `microvm` on an Apple Silicon host (TCG); **and on the
HP box's real Intel cores under KVM** — Firecracker with 4 vCPUs (195/195, the
PVH path) and the OVMF+GRUB rig with `-smp 4` (162/162, the multiboot2 path),
§4. A cold bare-metal boot of the same binary is staged and awaiting a reboot.

The aarch64 kernel went shared-kernel SMP with a Big Kernel Lock first and
fine-grained locks later (`docs/reference/subsystems/smp-shared.md`,
`BKL_FINE_GRAINED_LOCKING_PLAN.md`). This is the amd64 target taking the same
first step, and it is one module plus edits: `amd64/src/smp.rs` (per-CPU
state, the BKL, AP bring-up, the self-test), a trampoline in `boot.s`, and
changes in `sched.rs`, `usermode.rs`, `idt.rs`, `gdt.rs`, `lapic.rs`,
`serial.rs`. `amd64/README.md` § "SMP" is the current-state summary; this
document is what happened on the way.

## 1. What "SMP" means here

**One shared kernel, every core, one lock.** A core holds the Big Kernel Lock
(BKL) whenever it executes kernel code; it lets go on the way back to ring 3
and in the idle loop's `hlt`. User code runs in parallel on every core; kernel
code runs on one core at a time. Every `SAFETY: single core` comment in the
tree became true again under the reading "single *kernel* core", which is why
the `static mut` tables (`TASKS`, `PROCS`, the fd table, the pipe pool) needed
no locks of their own.

Three things had to exist for that to work:

**Per-CPU state through `%gs`.** `smp::PerCpu`, one block per core, holds the
running task, its `UserCtx` pointer, the BKL depth, the reschedule flag and a
scratch word. The kernel reaches its own through `IA32_GS_BASE`; the program's
GS base lives in `IA32_KERNEL_GS_BASE` while the kernel runs. `syscall_entry`
does `swapgs` first and `sysretq`'s neighbours do it last; the three
hand-assembled exception/interrupt stubs (`#PF`, `#GP`, the timer) `swapgs`
only when the saved `CS` says ring 3. The global `CURRENT_UCTX` and the
`SYSCALL_SCRATCH` word — both of which two cores in `syscall_entry` would
have clobbered — became `gs:[8]` and `gs:[16]`.

**The lock and its handoff on a switch.** A fair ticket lock (`smp::Bkl`) with
a per-core recursion depth. A context switch never releases it: the core keeps
the lock, saves the outgoing task's depth into its `Task`, installs the
incoming one's. The incoming task then releases wherever it was going to.
Every `yield_now` opens a drop window first — release, a few `pause`s,
reacquire — so a task waiting for another core's progress is not also
starving it (§3.1 is why "first").

**AP bring-up.** The MADT lists every enabled LAPIC; the BSP sends INIT then
STARTUP through the ICR (`lapic::send_init`/`send_startup`). The AP starts in
16-bit real mode at `0x8000`, where `start_secondaries` copied a trampoline
from `boot.s` (`ap_trampoline_start..end`, every address spelled as
`AP_BASE + (label - start)`). It enables paging on a root the BSP built — the
kernel PML4 plus an identity map of the first gigabyte in slot 0 — jumps to
`ap_entry64`, drops to the real kernel root, and finishes as Rust: per-core
GDT/TSS (`gdt::init_cpu`), the shared IDT (`idt::load`), `CR4.SMAP/SMEP`, the
syscall MSRs, its LAPIC (`lapic::init_ap`), then `online = true` **before**
`bkl_enter` (the BSP holds the lock while it waits for that flag), then
`sched::idle_loop`: `try_switch`, else release, `sti; hlt; cli`, reacquire.
Serial, one core at a time through one mailbox in the trampoline page.

## 2. Two things measured that were not in the plan

**QEMU puts the ACPI tables at the top of RAM.** With `MEMORY=2048` the XSDT
is at `0x7fff_ffaf`, above the 1 GiB `PHYSMAP_LIMIT` the SMP branch started
from, so `machine::Physmap::read` refused it and the MADT was invisible:
`acpi: tables:` printed empty and the kernel would have believed itself
single-core on any 4-vCPU guest with more than a gigabyte. The branch grew a
separate 4 GiB read bound for it; `main` had meanwhile raised `PHYSMAP_LIMIT`
itself to the 4 GiB `boot.s` maps (for the bare-metal framebuffer and LAPIC),
which fixes the same thing, so the cherry-pick kept `main`'s one constant and
dropped the second. Firecracker's tables are at `0xA00xx` and never saw this.

**`DISK=none` was already broken.** The pre-SMP kernel `#PF`s inside
`[SmolNet] Initializing network stack` when booted with no devices
(`cr2=0x8000020008`, a virtio slot read with no window set). Not touched here;
noted because it is the first thing a "quick" no-disk boot hits.

## 3. Bugs found, in the order they surfaced

Each was found by the existing self-tests running at `SMP=4`, which is the
argument for having run them under SMP rather than after the SMP checks.

### 3.1 Livelock: a lock released only "when idle" is never released

First version: `yield_now` dropped the BKL only when it found nothing to
switch to. `fork` test, `SMP=4 STRACE=1`: the shell (task 22) and the boot
task took turns on the BSP, each yield finding the other runnable, so the drop
window never opened — while the forked child on core 2 sat in
`syscall_handler`'s `bkl_enter` for the `execve` it never got to make. Trace:

```text
[sc>] cpu=0 task=22 nr=57 -> 6          fork returns pid 6
[sc>] cpu=2 task=23 nr=186 -> ENOSYS    child: gettid
[sc>] cpu=2 task=23 nr=14  -> 0         child: rt_sigprocmask
[sc>] cpu=0 task=22 nr=61 a1=-1         parent: wait4 — and nothing more, ever
```

Two tasks on one core is enough. Fix: every `try_switch` drops the lock
before it looks at the table, and the drop window shrank from 256 `pause`s to
4 — the ticket lock is FIFO, so a peer already spinning is served on the
release whatever the gap; the gap only matters for a peer that arrives during
it. (`sched::try_switch`'s doc carries the trace.)

### 3.2 Stale TLB: same `CR3` value, different address space

With the lock dropped at the top of `try_switch`, `finish()`'s yield releases
it while the finishing core still has the dead process's root in `CR3`. The
BSP reaps the process, frees its frames, and the very next `sys_spawn` gets
the freed PML4 frame back as the new process's root. The finishing core then
picks the new process up: `want == active_root()`, so the old code skipped
the `CR3` write — and with it the TLB flush. The new process ran on the dead
one's translations: `hello` read a garbage `argc` (exit status `0x47`, bits
3–5 clear — exactly the initial-stack checks) and busybox `#GP`'d at
`0x4bec45`, musl's `__init_libc` walking `argv` (`movq (%rdx,%rax,8), %rcx`
with a non-canonical `rdx`). Impossible on one core, where the reap always
follows the switch.

Two fixes, both kept: `finish()` moves the core to the kernel root *before*
its first yield, so a freed root is never live in any `CR3`; and
`try_switch` writes `CR3` unconditionally for a process root — only
kernel-root to kernel-root skips it. The doc on `finish` records the numbers.

### 3.3 The SSE registers belonged to the core, not the task

The kernel is soft-float and never touches xmm, so a preempted task's SSE
state survived by accident — on the same core. A task that resumes elsewhere
finds that core's registers. Pre-existing on one core for two SSE-using
processes interleaved by the tick (`AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md`
§8 called it out); SMP made it a certainty for any migrating musl binary.
`Task::fx` is a 512-byte `fxsave` area, saved and restored around
`switch_context`. Initialised to the reset state (`FCW 0x37F`, `MXCSR 0x1F80`),
not zero: an all-zero `MXCSR` unmasks every exception, and the first inexact
result in ring 3 would raise `#XM` into an "unhandled vector" halt.

### 3.4 A ring-3 fault halted a core, and the others carried on

`fatal` was right when every fault was the kernel's own. Under SMP a `#GP` in
busybox (the §3.2 symptom) parked one core with the machine otherwise fine,
which the test read as "busybox exited -1". Vectors 13 and 14 now check the
saved `CS`: a ring-3 fault prints one `[Fault]` line and calls
`usermode::kill_current_from_fault(128 + 11)`, which takes the BKL and returns
into `run_process` the way `.Lexit_to_kernel` does — onto the task's kernel
stack at the point `enter_user_mode` saved its registers — so the process
exits with the status a Linux parent would see for a signal death.

### 3.5 Two raw user writes the SMAP sweep missed

Not an SMP bug, found by the first SMP run with a real client: `sshd` on
`SMP=4`, first `ssh` connection, core 3 took `#PF err=3` at `cr2=0x7fff_ffff_dba8`
— a ring-0 *write* to a *present* user page, which with `CR4.SMAP` on is the
fault for touching user memory without `stac`. `sys_accept` stored `*addrlen`
with a bare `write_volatile`, and `sys_recvmsg` did the same for
`msg_namelen`; the 2026-09-05 sweep that moved everything else onto `uaccess`
missed both, and no self-test calls `accept` with a non-null `addrlen`. Under
SMP the fault was worse than a halt: core 3 held the BKL when it died, and the
other three printed `[BKL] stuck … owner 3` and spun forever. Both writes go
through `uaccess::write_val` now.

### 3.6 The GRUB path had no MADT, and the BSP kept the lock when it stopped

`kmain_mb2` built its `MachineDescription` from the memory map alone —
`from_memory_map(regions, None, None)` — so on the one machine that is
actually a 4-core box it would have said "no MADT — single core" and meant it.
On UEFI there is no RSDP in the BIOS window to scan for; the loader's copy in
the multiboot2 ACPI tag is the only way in. `akuma_multiboot2::BootInfo::rsdp`
hands the tag body over (the 2.0 tag preferred), `akuma_ryzen_amd64::acpi::
rsdp_from_bytes` parses it with the same checksums `rsdp_at` applies, and the
XSDT pointer inside leads to the firmware's real tables — below 4 GiB, inside
the physmap. Both have host tests. `kmain_mb2` also gained the trampoline-page
check both paths now do (`smp::trampoline_page_available`): the page at
`0x8000` must be RAM in the loader's map and clear of the information block
and the ramdisk module, or the boot stays single-core and says so.

The first rig run then ended with three `[BKL] stuck … owner 0` lines after
the verdict: the BSP had parked (`halt`, `cycle_forever`) still holding the
lock at depth 1, and every AP's next tick handler spun on it forever. Correct
diagnostic, wrong terminal state — `smp::bkl_abandon` now releases the lock
in both, gated on the per-CPU block being installed since `halt` can run
before it is.

### 3.7 What was changed on purpose, not to fix a bug

- **No kernel-mode preemption.** The tick preempts ring 3 and the idle loop.
  A kernel task (boot task, netpoll daemon) consumes `need_resched` at its next
  yield. The old behaviour was a latent deadlock: a kernel task preempted
  inside the heap allocator leaves `TALC`'s spinlock held, and the next task to
  allocate — a syscall, interrupts off — spins on it forever. Every kernel task
  yields on every lap, so nothing was lost; `sched::smoke_test`'s assertion
  moved from "preempted at least once" to "a yield found the tick's request".
- **`rflags` travels with the task.** `switch_context` pushes/pops it, so a
  syscall (interrupts off) that yields to a kernel task (on) comes back off.
- **`State::Reserved`.** `spawn_in_space_unpublished` published anyway (the
  name was aspirational); with a second core the gap between it and
  `seed_forked_task` is zero instructions wide, not a tick away.
- **`ARCH_SET_GS` writes `IA32_KERNEL_GS_BASE`**, recorded per task and
  restored on every switch. Writing `IA32_GS_BASE` from a syscall would have
  pointed every `gs:[..]` in the kernel at user memory.
- **`fd.rs`'s console read** yields instead of `spin_loop`ing: a shell waiting
  for a keypress held the BKL against every other core's syscalls.
- **`serial` has a best-effort lock** per call, with a budget so a crashed
  core's report is never silenced by a lock a dead core held.

## 4. What is verified

`SMP=4 amd64/run.sh` (QEMU `microvm`, `-cpu max`, TCG), 207 checks:

```text
  smp:  cpu 1 online (lapic id 1)
  smp:  cpu 2 online (lapic id 2)
  smp:  cpu 3 online (lapic id 3)
  smp:  4 cpus online
  smp: every secondary takes timer interrupts   [OK]
  smp: cpu mask the workers ran on 15
  smp: most workers in flight at once 4
  smp: two workers executed simultaneously   [OK]      <- kernel tasks, BKL dropped
  smp ring3: cpu mask the writes came from 11
  smp ring3: two processes ran on two cores   [OK]      <- user code
```

followed by the pre-existing ELF/fdprobe/spawn/busybox/execve/fork tests, all
now running with every core picking processes up. `SMP=1` is byte-for-byte the
old single-core behaviour plus the new bookkeeping (198 checks, all pass);
`SMP=2` passes the same 207. Each STARTUP took on the first try; bring-up is
~10 000 spins per core. Three `SMP=4` boots in a row were clean.

**Real hardware, under KVM** (`root@192.168.1.123`, the HP 500-502nj —
`AKUMA_AMD64_ON_HP_500_502NJ.md`; Intel, 4 cores):

| path | how | result |
|---|---|---|
| PVH | `FC_HOST=… VCPUS=4 amd64/run-firecracker.sh` (Firecracker v1.16.1) | 4 CPUs from the MADT, all online first try; workers in flight 4; ring-3 on two cores; **195 passed, 0 failed** (no NIC attached, so the net checks skip) |
| multiboot2 | the box's OVMF+GRUB rig (`grub-mkrescue`, `qemu -enable-kvm -smp 4`), serial to a file | `uart: present`; 4 CPUs from the loader's RSDP copy; same witnesses; **162 passed, 0 failed**; `/bin/sh` starts |

Ticks per core over the SMP phase are ~20–40 there against ~600 under TCG: the
real APIC clock is what `AP_INIT_DELAY` was sized against, and bring-up took
~200 spins per core instead of ~10 000. The first rig run failed one check,
`elf: on-disk and embedded images are identical`, because the ISO's `root.img`
predated the kernel copied in — a stale rig, not the kernel; rebuilt from the
same tree it passes.

Live: `SMP=4 INIT=/bin/sshd amd64/run.sh`, then six `ssh` sessions in a row
(`uname -a`, `uname; echo DONE`, `cat /etc/resolv.conf`, three `echo`s) all
returned their output with exit 0 and no `[Fault]`, `[EXCEPTION]` or
`[BKL] stuck` on the console — after §3.5's fix, which the first such session
found.

## 5. What is deliberately missing

- **No TLB shootdown.** `invlpg` is core-local. Complete today because a space
  is only active on the core running its one task and every switch to a
  process root writes `CR3`; kernel-half mappings change only at boot. The
  first `clone(CLONE_VM)` needs an IPI.
- **No wake IPI.** A core in `hlt` learns of new work at its next tick
  (~1.6 ms on QEMU's APIC clock). Latency, not correctness.
- **No IST, still.** A kernel stack overflow on any core is a triple fault.
- **The BKL is held across every kernel path**, including a whole-file ext2
  read through polled virtio. A peer's syscall waits for it. Fine-grained
  locking is the aarch64 tree's plan, not this stage's.
- **A cold bare-metal boot.** Everything above ran on the box's real cores
  but under KVM, which virtualises the INIT/STARTUP sequence. The binary that
  passed the rig is staged at `/boot/akuma/akuma-amd64` (with the 128 MiB
  `root.img` beside it, old files kept as `*.bak-<date>`) and
  `grub-reboot` is armed for the Akuma entry; the reboot itself is the user's.
- **`MAX_CPUS = 16`.** The MADT parser reports up to 32; the per-CPU table
  stops at 16 and starts the first 15 APs with a warning.

## 6. Files

| Path | |
|---|---|
| `amd64/src/smp.rs` | per-CPU block, BKL, AP bring-up, `ap_entry64`, the SMP self-test |
| `amd64/src/boot.s` | `ap_trampoline_start..end`: 16→32→64-bit, its own GDT, the mailbox |
| `amd64/src/sched.rs` | `on_cpu`/`pinned`/`idle`/`bkl_depth`/`fx` per task, `Reserved`, `idle_loop`, `register_idle_task`, `try_switch` |
| `amd64/src/usermode.rs` | `gs:`-relative `syscall_entry`, `swapgs`, `bkl_enter`/`leave`, `enter_user`, `kill_current_from_fault`, `WRITE_CPU`, `smp_parallel_test` |
| `amd64/src/idt.rs` | `timer_entry`/`timer_dispatch`, conditional `swapgs` in the stubs, `user_fault`, `load` |
| `amd64/src/gdt.rs` | GDT/TSS per core, `init_cpu` |
| `amd64/src/lapic.rs` | `init_ap`, `apic_id`, ICR IPIs, `delay_counts`, per-core ticks |
| `amd64/src/sock.rs` | `accept`'s `addrlen` and `recvmsg`'s `msg_namelen` through `uaccess` (§3.5) |
| `amd64/src/multiboot2.rs` | RSDP → MADT from the loader's tag, the SMP block after `preempt_test`, `initargs=`, `serial::init` (§3.6) |
| `crates/akuma-multiboot2` | `BootInfo::rsdp` |
| `crates/akuma-ryzen-amd64` | `acpi::rsdp_from_bytes` |
| `amd64/run.sh` | `SMP=N` |
