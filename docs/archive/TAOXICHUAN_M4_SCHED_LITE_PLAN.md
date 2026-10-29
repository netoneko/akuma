# Taoxichuan M4: sched-lite on a tick — the plan (session pb, 2026-10-27)

Twin of the `exec_runtime.rs` / `sched.rs` registrations (amd64 side), written after
session pb linked `akuma-exec` + `akuma-threading` (PB1) and gave taoxichuan a tick
(`akuma-lapic-amd64`, PB5). Owner's decisions: **preemption, not cooperative-only**
(nothing in the git/cargo ladder works without it), and the design is **SMP-ready**
with SMP itself a follow-on slice. Every claim not read in source this session is
marked **UNCHECKED**.

## 0. What session pb changed (the foundation this plan stands on)

- `crates/akuma-lapic-amd64` (new, extracted from `amd64/src/lapic.rs` 1121→~719 lines):
  xAPIC window (`read_reg`/`write_reg`), `eoi`, `TICKS`/`ticks`/`advance_tick`/`on_tick`,
  `start_timer`/`stop_timer`/`timer_running`, `delay_counts`, `timer_current_count`,
  `arm_oneshot`, `credit_ticks`, `calibrate` (8254 channel-0, verbatim) + `pit_wait_us`,
  `tsc_hz`/`tsc_uptime_us`, `software_enable`, `init(base_msr_enabled, MmioWindow)`.
  `#![forbid(unsafe_code)]`, unsafe budget 0. amd64 re-exports it under the old names;
  taoxichuan consumes it directly. One implementation, two consumers.
- taoxichuan: `lapic_init()` after the PMM/heap bring-up (boot-1 lesson: its
  `map_device` allocates page-table frames; before `PmmConfig` registration it panics),
  `trap::register(Kind::Timer, timer_trap)` + `IDT.set(32, trap::entry(Kind::Timer))`
  — `Kind::Timer` already existed in `akuma-trap-amd64` (lib.rs:174, `vector()==None`,
  stub `timer_entry` at :469) — and `timer_trap` = the crate's `on_tick` (count + EOI,
  **no task switch**). Ring 3 runs with IF=1 (`EntryFrame.rflags = 0x202`, child.rs:339/:487);
  every kernel path runs IF=0 (the `syscall` instruction masks; the M2 scaffold's
  discipline), so a tick can only land in ring 3 and returns to the same instruction.
  TSS.RSP0 is already pointed at `TRAP_STACK` (main.rs:186), which is what makes the
  ring-3→ring-0 push safe. `[TICK pre]`/`[TICK post]` console lines bracket each exec.

## 1. Hook table: what amd64 registers, what the taoxichuan twin must register

### `akuma_threading::ThreadRuntime` (crates/akuma-threading/src/lib.rs:153-160; 6 fn fields)
| field | amd64 registers (sched.rs `register_hooks`, :834-857+) | taoxichuan twin | needs a tick/LAPIC? |
|---|---|---|---|
| `uptime_us: fn() -> u64` | `crate::net::uptime_us` (ticks × `US_PER_TICK_TARGET`) | same shape over `akuma_lapic_amd64::ticks()` | **yes — the tick IS the clock** |
| `trigger_sgi: fn(u32)` | no-op with a reason (x86 scheduler is a call, not an ISR) | no-op (one core) | no |
| `wake_core: fn(u8)` | no-op | no-op | no |
| `wake_remote_idle: fn() -> bool` | no-op → `false` | no-op → `false` | no |
| `end_of_interrupt: fn(u32)` | no-op (ThreadRuntime's own EOI is dead on x86; ExecRuntime's is real) | no-op (or `lapic::eoi` discard-arg) | no |
| `print_str: fn(&str)` | `crate::serial::puts` | `akuma_primitives::safe_print!` wrapper | no |

### `akuma_threading::ThreadConfig` (lib.rs:163-179; 15 fields)
Pure numbers/flags (stack sizes, canaries, boot stack, cleanup cooldown…). amd64 fills
them from its config; the twin fills **UNCHECKED** values copied from amd64's
`config()` (amd64/src/exec_runtime.rs `fn config()` — not read line-by-line this
session). None need a timer.

### `akuma_threading::ProcessHooks` (lib.rs:184-198; 7 fn fields)
| field | what it asks | twin |
|---|---|---|
| `clear_draining` / `drain_in_flight` | retired-process drain state | no-op / `false` until the slot swap (UNCHECKED) |
| `lifecycle_trace_on` | trace flag | `false` |
| `pid_for_thread` / `find_pid_by_thread` | thread↔pid map | over `SpawnSlot` ring today (child.rs:40-70); over the real pid table after the swap |
| `is_current_interrupted` | pending-signal check | `false` (no signals — out of scope) |
| `proc_dump_info` / `dump_orphan_processes` | `[THR-DUMP]` correlation | no-ops |

### `akuma_threading::X86ArchHooks` (lib.rs:2942-3040; 10 fn fields, amd64/sched.rs:834-846)
| field | amd64 | twin (per-core state, NO `static` current-slot) | tick? |
|---|---|---|---|
| `switch_to(from,to)` | `hook_switch_to` (CR3 before switch, fxsave/fxrstor order :46-49) | **the switch itself** — lands with `fork_process`, not before | needs the tick to *call* it (preemption), not to run |
| `current_slot()` | `smp::current_task` (per-CPU `%gs` block) | one-core stub returning a slot static **shaped as `fn()`** so the SMP slice replaces the body, not the callers | no |
| `set_current_slot(usize)` | `smp::set_current_task` | same one-core stub | no |
| `idle_slot()` | `smp::idle_task` | slot 0's idle | no |
| `can_run(slot)` | `hook_can_run` (pinning) | `true` for the boot thread's slot | no |
| `transfer_lock_depth(from,to)` | `hook_transfer_lock_depth` (BKL depth is per-thread, lock per-core) | depth moves with the one switch; via `akuma-bkl` | no |
| `allow_tick()` | open one instruction window, re-mask (frozen-clock fix, measured 2026-09-06) | **yes — meaningless without a tick**; taoxichuan has one now | **yes** |
| `write_user_context(slot,&UserCtx)` | writes the per-slot `UserCtx` side table | writes the twin's capture (today: the `SAVE_*`/`RET_RSP` crossing statics' replacement) | no |
| `read_user_context(slot)` | mirror of the above, `None` if never trapped | same refusal | no |
| `prepare_task_slot(slot)` | stacks are the target's on x86; reset defaults | allocates/reuses the twin's kernel+trap stack pair | no |

### `akuma_exec::ExecRuntime` (crates/akuma-exec/src/runtime.rs:63+; amd64 registers in exec_runtime.rs:189 `runtime()`)
| field | amd64 | twin |
|---|---|---|
| `enter_user` | `usermode::enter_ring3` — owns the execve loop + teardown | today's `child_enter` crossing (child.rs:555), absorbed later |
| `fork_share_memory` | `usermode::fork_share_memory` (real page-table walk) | taoxichuan's fork copy (`PARENT_ROOT` clone path) until cow |
| `alloc_process_info` | `\|_space\| Ok(0)` ("no ProcessInfo page") | same `Ok(0)` |
| `bind_child_task` | child CR3 + SPAWN row + `UserCtx::proc_slot` | the twin's slot binding |
| `write_user_tid` | `uaccess::write_val` (stac/clac, SMAP) | plain write (no SMAP handling UNCHECKED) |
| `uptime_us` | same clock as ThreadRuntime | `ticks() × 10_000` — **the tick** |
| `disable_irqs`/`enable_irqs` | cli/sti pairs | cli/sti (IF discipline already: kernel always masked) |
| `end_of_interrupt(u32)` | `lapic::eoi()` | `akuma_lapic_amd64::eoi()` |
| `heap_stats`/`is_memory_low` | `akuma_alloc` | same over taoxichuan's allocator |
| `print_str` | serial | `safe_print!` |
| `exec_bkl_drop_enabled` | smp-shared flag, `false` off SMP | `false` |
| VFS `read_file`/`read_at`/`resolve_symlinks`/`resolve_file_id` | fs/`read_at` real, `resolve_file_id` stub (no mount table) | taoxichuan's `root_fs()` paths; `resolve_file_id` stub |
| `read_at_by_inode`, `on_process_exit`, `remove_socket`, `socket_clone_ref`, `rump_socket_clone_ref`, `futex_wake`, `check_itimers`, `pipe_close_write`/`read`/`clone_ref`/`write` | per-feature hooks (sockets/itimers/pipes) | pipes: real (M3's table already exists); the rest no-ops until their slices (**UNCHECKED** which of them `fork_process`'s first customer needs — read at that slice) |

## 2. Syscalls that would block, and where the switch happens

`fork_process` (plan §4-6) makes these blockable; each blocks = park the slot, switch:
- `read`/`write` on a pipe whose buffer is full/empty (M3's table already wakes
  registered waiters on events — the AArch64 pipe pattern amd64 reuses);
- `wait4` (the parent waiting out a child that now really runs);
- `spawn`'s stdio pump (the reason M2's blocking fork is throwaway-shaped: a writer
  outliving one read, `git clone`'s helper pump).

The switch happens in **`timer_trap`** (taoxichuan/src/main.rs) — the same place amd64
puts it (`timer_trap` at amd64/src/idt.rs:1300-1339: count, acknowledge, then
`preempt_if_needed(from_user)`): on the interrupted task's own trap stack, with IF
clear (interrupt gate), allocation-free. `from_user = t.from_ring3()` decides:
ring 3 and the idle loop may be switched away from; **ring 0 is only asked**
(`set_need_resched`) and switches at its next yield — a kernel thread preempted
inside the allocator leaves the heap's spinlock held forever (amd64/sched.rs module
header; do not preempt ring 0). Taoxichuan's IF discipline makes `from_user` almost
always true: kernel code never runs with IF set today, so every tick that fires at
all fired in ring 3. When `fork_process` lands, the syscall path gets its first
`sti` windows (the `allow_tick` shape), and the rule stays: switch on a tick only
if `from_user`, else mark need-resched.

## 3. A blocked task's kernel stack vs the `SYSCALL_STACK` / crossing statics

The T-Y bug-3 lesson (stack-band arithmetic): today the syscall path runs on ONE
`SYSCALL_STACK` band per crossing depth, and the fork/exec machinery hands state
through `static mut` crossing statics (`RET_RSP`, `SAVE_RBX/RBP/R12-R15`,
`SPAWN_OUT_SLOT`, `PARENT_*` — child.rs:72-94) because only one task is ever "in the
kernel". The moment two tasks can block, that identity breaks:

- A task that parks inside a syscall must keep its own kernel stack until wake:
  the `SYSCALL_STACK` becomes **per-slot** (allocated by the twin's
  `X86ArchHooks::prepare_task_slot`, exactly what amd64 leaks per slot and reuses on
  recycle), not one shared band. The TSS.RSP0 trap stack can stay shared (interrupt
  frames are transient, and the tick handler is bounded + allocation-free).
- The crossing statics (`RET_RSP`/`SAVE_*`) are the parent's resume context while the
  child runs inside `fork`. With a real scheduler they move **into the per-slot
  machine state** the hooks describe (amd64: the per-slot side table; taoxichuan twin:
  same shape) — a second task entering the kernel would otherwise overwrite the
  first's return band. Nothing else in the current scaffold may hold "the current
  task" in a `static`: the hooks (`current_slot`/`set_current_slot`) exist precisely
  so the SMP slice can put `%gs` behind them without touching callers.
- `NESTED_SLOT`/`NESTED_DEPTH` (child.rs:74-75) encode "who am I" by stack depth;
  they become lookups on `current_slot()` at the swap.

## 4. SMP slice: what booting the other cores needs, and which crate has it

| need | provider | state |
|---|---|---|
| AP startup path (INIT-SIPI, `send_init`/`send_startup`, `init_ap`) | `amd64/src/lapic.rs` (IPI face kept there on purpose: `send_fixed`/`send_init`/`send_startup` over `akuma_lapic_amd64::write_reg(REG_ICR_*)`) | exists in amd64; NOT a crate — extract per rule 3 when the slice comes |
| per-cpu `%gs` block, `NO_CPU`, `MAX_CPUS`, wake vector | `amd64/src/smp.rs` | exists; not a crate |
| per-core tick bookkeeping (per-core TICKS, oneshot arrays, TSC offsets) | `amd64/src/lapic.rs` oneshot/TSC-sync parts + `crate::smp::cpu_index` | exists in amd64; the crate's `on_tick` is the single-core core of it |
| BKL | `crates/akuma-bkl` (`ProcessBkl`, `bkl_free_io`, sync) | exists; amd64 already uses it |
| slot table | `crates/akuma-slot-table` | exists (**UNCHECKED** — not read this session) |
| scheduler core (states, wake generations, park discipline) | `crates/akuma-threading` | exists; that is the whole point of the seam |
| the KVM guest's LAPIC model (xAPIC page + timer) and 8254 `pit` | `akuma-svm-amd64`'s `lapic`, `akuma-port`'s `PIT` | exists (guest fact) |
| TLB shootdown transport | `amd64/src/shootdown.rs` + `lapic::send_fixed` | exists in amd64 |

The plan doc must say: SMP boot needs `smp.rs`'s AP path + per-cpu `%gs` + IPIs,
all three already in `amd64/src/` — the slice is an extraction + wiring, not a
rewrite. Booting extra cores is NOT this slice.

## 5. Proposed M4 slice list (each = one boot unless said)

1. **twin runtime** (done shape, no switch): register `ThreadRuntime` + `ExecRuntime`
   with the table above; `uptime_us` = `ticks()×10_000`; pipes hooks real. 1 boot
   (gates re-run).
2. **slot machine-state**: per-slot kernel+trap stacks, `prepare_task_slot`,
   `write/read_user_context`; crossing statics move into per-slot state. 1 boot.
3. **`fork_process` + `wait4`** over child channels; `spawn` pump exits the blocking
   shape. 1 boot + probe gate (`userspace/forkprobe` — libakuma fork/cow/sockfd/many;
   builds x86_64 static like the rest of the guest userspace; c_stress `grandfork.c`/
   `killtree.c` are musl-static by their README). Run P1-P5 + F1 + forkprobe.
4. **preemption**: `timer_trap` gains `preempt_if_needed(from_user)` semantics —
   ring 3/idle switch, ring 0 need-resched. 1 boot; `dd`-shaped burn loop shows
   context switches; P1-P5 + F1 must stay green.
5. **SMP slice** (separate): extract smp.rs/lapic IPI per rule 3, boot the other
   cores, BKL on. 2 boots (first-boot smoke + gates).
