# Multi-VM / hang hunting scripts

Grade: — (index)

| Script | What it does |
|---|---|
| [`run_multiple.sh`](../../../scripts/run_multiple.sh) | Launches N parallel Akuma boots (own disk, own port band, own log) with a log-stall watchdog, for hunting hangs that don't reproduce every boot. `scripts/run_multiple.sh 8`. Background: [`../../archive/STABILITY_URGENT_ISSUES.md`](../../archive/STABILITY_URGENT_ISSUES.md). |
| [`run_two_vms.sh`](../../../scripts/run_two_vms.sh) | Boots the two-VM agent demo (a `meow` VM + a `llama.cpp` server VM wired together over SLIRP). Used by [`../../../acceptance/archive/03_two_vms_agent_workflow.md`](../../../acceptance/archive/03_two_vms_agent_workflow.md) (archived — superseded by `acceptance/08_meow_clone_compile_run.md`'s Ollama-backed pipeline); background in [`../../archive/TWO_VMS_AGENT_DEMO.md`](../../archive/TWO_VMS_AGENT_DEMO.md). |
| [`lockprobe.py`](../../../scripts/lockprobe.py) | Names the lock (and the fault) a **wedged SMP VM** is stuck on, via QEMU's gdbstub. Boot with `GDB=1`, then `scripts/lockprobe.py <gdb-port> -n 3`. Per core: PC/LR/SP symbolised, `ESR_EL1`/`ELR_EL1`/`FAR_EL1` with the exception class decoded, and every register that resolves to a named static (a kernel lock *is* a static, so this names the lock). Decodes `KERNEL_LOCK` into HELD-by-core-N / LOST-TICKET / idle. See the notes below. |

## `lockprobe.py` — reading it correctly

Three traps, each of which produced a confidently wrong answer first:

- **`KernelLock` is not `#[repr(C)]`.** Its field order in memory is chosen by
  the compiler and is *not* declaration order (measured: `barged[8]` at `+0x00`,
  `owner` at `+0x08`, `next_ticket` at `+0x0c`, `now_serving` at `+0x10`).
  Assuming declaration order reported a confident "LOST TICKET" for a lock that
  was plainly HELD. The script therefore recovers the offsets from the binary by
  disassembling `KernelLock::release`; if that ever fails, do not fall back to
  guessing.
- **No debug info is required, and you should not add any.** Symbolisation comes
  from `.symtab`. Building with `debug = true` changes the loaded image by
  ~100 KB (1.51 MB vs 1.41 MB), which can move a timing-sensitive race — the
  thing you are trying to observe.
- **Frozen registers under `-accel hvf` are not proof of a halted core.** HVF
  syncs vCPU state only on exit to the hypervisor, so a core spinning in guest
  mode can report identical registers forever. Corroborate with host CPU% (`ps`)
  before concluding. Note also that a `fault -> handler -> eret -> refault` loop
  *legitimately* reproduces byte-identical state every pass, because the handler
  epilogue restores exactly what its prologue pushed — 100% CPU with frozen
  registers is that shape, not a parked core.

Two wedge signatures captured with it on 2026-08-08, for comparison:

| | BKL storm | silent wedge |
|---|---|---|
| `KERNEL_LOCK` | HELD, `owner=4` (core 3) | free, `owner=0` |
| `[BKL] stuck` lines | 3153 | 1 |
| host CPU | ~398% | ~399% |
| cores | 3 × `KernelLock::acquire` spin loop, holder in the EL1 sync-exception path | `pmm::alloc_pages_contiguous_zeroed`, `talc_alloc`, `__rust_dealloc`, `sys_futex` |
| locks spun on | `KERNEL_LOCK` | `allocator::TALC` (×2), `pmm::PMM` (×1, IRQs disabled), `syscall::sync::FUTEX_WAITERS` (×1) |

The silent-wedge lock names came from the spin instruction's own encoding — each of
those PCs is a test-and-test-and-set inner loop (`isb; ldrb wN,[xB,#off]; cbnz`),
so `adrp`-base + displacement *is* the lock address, and `info symbol` names it.
That works even when the waiter holds no register pointing at the lock.

## Want source lines instead of addresses?

Once you already have a reliable repro (not while still hunting — see below),
[`release-smp-shared-debug`](../build-profiles.md#debug-info-variant-opt-in-off-by-default)
is a DWARF-enabled build for source-level `lldb` debugging against the same
gdbstub. It's off by default and separate from the plain `release-smp-shared`
profile precisely because adding debug info changes the loaded image size
(+102,720 bytes measured, `text`-only) enough to plausibly move a
timing-sensitive race — exactly the failure mode `lockprobe.py` avoids by
symbolicating off `.symtab` instead. Reach for the DWARF profile only after
you can reproduce the bug on the plain profile; use it to read source instead
of hunting with it.

Back to [`README.md`](README.md).
