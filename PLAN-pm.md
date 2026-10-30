# PLAN-pm — Taoxichuan stage 8, session pm: the switch to the standard process slots and scheduler

Plan for the supervisor to execute session by session. Taoxichuan stops hand-rolling
process slots, the spawn crossing and its scheduler scaffolding, and becomes a
composition of `akuma-exec` / `akuma-threading` / `akuma-slot-table` / `akuma-fdtable`
/ `akuma-pipes` / `akuma-syscalls-*` in the way `amd64/src/` is. Every claim names
file:line; `UNCHECKED` marks what this session did not read. Planning only — no
kernel code changed here. Branch `rust-org-il-experiment`, push `litter` only.

**Executive answer.** The smallest sequence is five code slices + one sweep:
S0 baseline battery → S1 init becomes the first registered `Process` with a real
threading slot → S2 wait4 becomes the crate-owned park/wake (blocking wait over
`akuma-exec`'s process table, `PARENT_OF` deleted) → S3 sshd moves to `libakuma`'s
`linux-abi` (fork + pipe2 + dup3 + execve) and the entire M2 SPAWN crossing dies
(≈700 L deleted) → S4 the boot program list becomes fork+exec children of init and
the boot-task special arms die → S5 the hand-rolled ELF loader moves onto
`akuma-elf-exec`'s decisions → S6 the dispatcher gains amd64's two-table shape
(legacy x86_64 numbers translate through `akuma-syscalls-abi` into
`akuma-syscalls-glue`) → S7 sweep (docs, diagrams, leftover ring types). Total
≈ 1,600–1,900 L deleted, ≈ 300–350 L of glue added/changed, ~300 tool calls ≈ 3
executor sessions. If only half fits: S0–S3 (+ the S7 sweep folded in) is the
thesis; S5/S6 are stage-8/9 enablers, not process-model items.

## 0. Premises (checked against the tree this session unless marked)

| # | premise | verdict | evidence |
|---|---|---|---|
| 1 | Taoxichuan already consumes the crates for fork/fd/pipes/net/fs: it calls `akuma_exec::process::fork_process`, registers the four hook tables, and its syscall arms route into `akuma-fdtable`/`akuma-pipes`/`akuma-net`/`akuma-ext2` | HOLDS | `taoxichuan/src/child.rs:908` (`fork_process(child_pid, 0)`); `exec_twin.rs:262+` (`init` registering ExecRuntime/ExecConfig/ThreadRuntime/ThreadConfig/ProcessHooks/X86ArchHooks); `taoxichuan/docs/reference/syscalls.md` table (`decides` column names the crates) |
| 2 | The kernel scheduling policy is `akuma-exec`/`akuma-threading` (fixed pool, `THREAD_STATES`, round-robin); **`crates/akuma-scheduler` is a discrete-event simulator, not kernel policy** — the brief's premise that it owns "the scheduling policy" is WRONG per the code; the code wins | WRONG (crate role) | `crates/akuma-scheduler/src/lib.rs:1-10` ("Discrete-event simulator for Akuma's scheduler placement policy… a model, not the kernel"); policy doc `docs/reference/subsystems/scheduler.md:13-32` (`THREAD_STATES` in `crates/akuma-exec/src/threading/mod.rs:292`; `akuma-threading` is where all scheduler `unsafe` lives, its `src/lib.rs:1-24`) |
| 3 | amd64's init: `boot::late_init` → `exec_runtime::init()` (registers runtime, `reserve_pid_floor(2)`), then `usermode::run_init` loads the image, spawns the task slot, `register_exec_process(1, …)`, publishes | HOLDS | `amd64/src/boot.rs:128`; `amd64/src/exec_runtime.rs:614-641`; `amd64/src/usermode.rs:8845-8905` |
| 4 | amd64 `sys_fork` is a thin fold over `akuma_exec::process::fork_process`; the three things shared code cannot name (CR3 root, SPAWN row, proc_slot) bind through `ExecRuntime::bind_child_task`; fork and clone share the crate call (clone passes `child_stack`) | HOLDS | `amd64/src/usermode.rs:5520-5575` (fork), `:5585` (clone w/ stack), the five-slice comment at `:5525-5540` |
| 5 | Today taoxichuan fork/execve/wait4 only work inside the spawned shell: `fork_arm_real` answers `EPERM` for the boot task (`depth 0`), and "execve outside any task context" is a listed gap; that is why sshd needs private `SPAWN` | HOLDS | `taoxichuan/src/child.rs:868-886` (depth gate); `taoxichuan/docs/reference/syscalls.md` § "Known deliberate gaps" (execve outside task context) and rows 56/57/61 |
| 6 | `libakuma`'s `linux-abi` feature exists and is exactly fork+pipe2+dup3+execve (`spawn_full`) and `wait4(WNOHANG)` (`waitpid_status`); **but no build in the tree enables it today** (grep over `*.sh`/`*.toml`: only libakuma's own definition) — S3 makes taoxichuan its first consumer | HOLDS (feature exists; adoption UNCHECKED for runtime behaviour) | `userspace/libakuma/src/lib.rs:2195-2260` (`#[cfg(feature = "linux-abi")] fn spawn_full` = pipe, fork, dup3, close×2, execve), `:2493-2513` (`waitpid_status` = `wait4(WNOHANG)`), `Cargo.toml:19` (`linux-abi = []`); repo-wide grep for `linux-abi` outside `userspace/libakuma/`: no hits |
| 7 | amd64's dispatcher is two disjoint tables: a legacy `match nr` for x86-only numbers (its `X86_ONLY` list includes 82,83,84,87,88,89,90,92,94), each translating to a `Syscall::*` and routing into `akuma-syscalls-glue` (e.g. `83 => to_glue(Syscall::Mkdirat, [AT_FDCWD, …])`), plus the `Syscall` table; a boot smoke test asserts no number is in both. This is the shape to adopt — the numbers come **through** `akuma-syscalls-abi`, not from new taoxichuan arms | HOLDS | `amd64/src/usermode.rs:1511`, `:1658-1693`, the `X86_ONLY` table + `dispatch_smoke_test` at `:7506-7530` |
| 8 | The lost-wakeup class disappears with the M2 crossing: a design where the parent's wait is a registered thread park and the child is an ordinary published slot has no suspended parent frame (`CrossSave`/`EXEC_SAVE`) to lose; the pool cannot pick a slot left `INITIALIZING`, which is exactly the state an M2 SPAWN child's slot stays in | HOLDS (by design; verified only by gate at S3) | pl stall facts (`/root/dev-cycle/taoxichuan-s7/RESULT-pl.md`: last rung `EXE loaded`, nothing parked, child runs, crossing never resumes); pool gate `docs/reference/subsystems/scheduler.md:28-32` ("INITIALIZING exists so the scheduler cannot pick a slot whose context is half-built"); `taoxichuan/docs/reference/processes.md` § SPAWN crossing |
| 9 | Current gates state on the merge tree: F1, PK1, PK2 (ALL PASS), P1–P5 green; stages 6/7 (apk, curl HTTPS) green; the apk-spawn stall is the only open defect and needs the box's KVM timing (10/10 clean under supervisor's QEMU TCG) | HOLDS | `RESULT-pj.md` premise rows 2–3; `taoxichuan/docs/README.md` § Status; brief 09m ("10 of 10 boots clean") |

## 1. Inventory (measured this session)

Method: `analyze` (tree-sitter) over `taoxichuan/src/` + targeted reads; line spans
are function-start line numbers from that pass. `wc -l`: child.rs 1,936; exec_twin.rs
599; main.rs 3,891 (dispatch_inner spans 1647→3044); pipes.rs 290; total tree 8,062.

### 1.1 `child.rs` — seven groups

| piece | lines (spans) | owner today | the crate/file that replaces it | gap |
|---|---|---|---|---|
| SpawnSlot ring + `CrossSave` + `SlotStacks` + `SlotCross` types and accessors (`cross_mut`/`save_ptr`/`slots`/`nested_active`) | ≈225 (40–265) | private | `akuma-slot-table` (the lock-free per-slot store under `akuma-exec`'s process table, `crates/akuma-slot-table/src/lib.rs:1-45`) + `akuma-exec` `Process` rows + `akuma-threading` pool states | the ring conflates three things the crates split: thread slot (pool), pid↔process (process table), crossing save area (dies with M2). `PARENT_OF`'s slot-reuse lesson (pj-4: slots reused across pids 16..19) is why the crate keys parent links by pid |
| spawn-args marshaling (`child_write`, `read_ptr_array`, `read_user_string`, `read_spawn_args`) | ≈155 (289–443) | private | keep, but shared: amd64 does argv/envp copy in the loader/`Image::from_elf_argv` path with `akuma_exec::process::user_access` helpers (`amd64/src/exec_runtime.rs` doc lines 12–16) | taoxichuan keeps its `copy_from_user`/`copy_to_user` physmap pair (`main.rs:843/868`); the SPAWN-only parts die with M2, the argv/envp readers are reused by exec |
| M2 SPAWN crossing (`spawn_arm`, `exec_replace`, `exec_return`, `child_enter`, `child_exit`, `exit_cross`) | ≈535 (919–1129, 1129–1254, 1254–1298, 1298–1351, 1351–1404, 1404–1454) | private | **deleted**: sshd → `libakuma::linux-abi` spawn (fork+execve, premise 6); the nested-child syscall band ("lower half of the parent's band", processes.md) and `CrossSave` naked-function contract die with it | none — this is the private machinery the thesis excludes. Rollback: keep arms behind a const until the sshd gate passes |
| real fork (`bind_child_task`, `capture_user_context`, `fork_arm_real`, `register_shell_process`) | ≈255 (443–525, 746–789, 868–919, 789–868) | private glue over the crate | amd64's equivalents: `bind_child_task` hook stays (binds CR3/slot/proc_slot — `usermode.rs:5541-5546`); `capture_user_context` stays (arch); `register_shell_process` is replaced by init registration (S1); `NEXT_PID` static → the crate's pid allocation (amd64 `alloc_pid`, `usermode.rs:5557`) | depth gate (1..3, `child.rs:880-886`) is a ring artifact — real slots have no depth; the gate dies when every task is a slot |
| sched glue (`sched_kstack_top`, `sched_task_slot_of`, `sched_root_of`, `is_real_task`, `task_exit_real`, `slot_fs_of`, `set_slot_fs`, `stamp_child_parent`, `parent_of`) | ≈120 (525–664) | private | `switch_to`'s five effects stay (`exec_twin.rs:113-166`); `task_exit_real` → the crate's exit path + child-channel wake (amd64 slice-3 shape, `usermode.rs:5531-5534`); `PARENT_OF` map → `Process::parent_pid` as the only link (amd64 `sys_waitpid` comment, `usermode.rs:6165-6172`) | fs shadow (per-slot `IA32_FS_BASE`) is a real one-core need — keep; amd64 carries fs in `UserCtx`/per-CPU instead |
| wait family (`wait4_arm`, `wait4_real`, `wait4_arm_blocking_scan`, `waitpid_arm`, `close_stdin_arm`, `read_child_out`) | ≈200 (1652–1855) | private scans over the ring | amd64's `sys_waitpid` shape: `akuma_exec::process::find_process` + `is_child_of` + `exited` + park on the child channel (`usermode.rs:6153-6195+`); `WAITPID`/`CLOSE_CHILD_STDIN` deleted with SPAWN | "wait4 is parent-owned" rules (ECHILD for strangers, never park on them) are already the crate's `is_child_of_group` decision (pj pulled it); what remains private is only the scan plumbing |
| arch hooks/helpers (`sleep_us`, `leak_stacks`, `prepare_task_slot`, `write_user_context`, `read_user_context`) | ≈80 (1855–1936) | private, but they ARE the `X86ArchHooks` arms | keep (amd64 analogs: `amd64/src/sched.rs:2143` `prepare_task_slot`, `:1694`/`:1738` write/read_user_context) | leaked 64 KiB kernel + 32 KiB trap stacks are the twin of amd64's `Stacks` (`sched.rs:327-355`); keep |

Sum of groups ≈ 1,570 L; the rest of the file is constructors/consts/comments.
**Deletion target after S3+S4: ≈ 1,050–1,200 L** (crossing 535 + ring types ~200 +
PARENT_OF/wait scans ~200 + depth/nested plumbing ~100), leaving child.rs ≈ 350 L
of hooks, capture, and fs shadow.

### 1.2 `exec_twin.rs` (599 L) — mostly survives

| piece | lines | owner today | replaces it | gap |
|---|---|---|---|---|
| `current_slot`/`set_current_slot`/`shell_tid`/`uptime_us`/`switch_to` | ≈110 (55–166) | twin (kernel glue) | none — this is the amd64 `hook_switch_to`+per-CPU block equivalent (`amd64/src/sched.rs:401-497`) | `switch_to` lacks amd64's FPU save (`FxArea`, `sched.rs:116-278`) — see R6 |
| `idle_main`/`seed_idle`/`tick_window`/`allow_tick`/`init`/`idle_slot`/`can_run`/`transfer_lock_depth` | ≈160 (185–318) | twin | amd64 `idle_loop` (`sched.rs:1387+`) + `register_hooks` (`sched.rs:790`) | after S1, `can_run`/tick gates must cover the now-slotted init; `idle_main` gains the retired-reclaim drain amd64 has (`sched.rs:1418-1421`) |
| the four config constructors (`thread_runtime`/`thread_config`/`process_hooks`/`exec_runtime`/`exec_config`) | ≈200 (319–486, 564–599) | twin | none — this IS the amd64 `exec_runtime.rs::runtime()/config()` analog | `ProcessHooks.pid_for_thread` reads the SpawnSlot ring today; re-point at the crate process table in S2 |
| `twin_fork_share_memory`/`twin_bind_child_task`/`twin_enter_user` | ≈77 (487–563) | twin hooks | same roles as amd64's `fork_share_memory`/`bind_child_task`/`enter_ring3` | `twin_bind_child_task` calls `child::bind_child_task` — the one place the ring leaks into the hooks; re-point in S2 |

### 1.3 `main.rs` — the process/slot/fd parts

| piece | lines | owner today | replaces it | gap |
|---|---|---|---|---|
| `Task` record + `task()`/`task_slots`/`set_task_slot` | ≈90 (721–810) | private (16 fixed tasks; fd table + root + brk each) | `akuma-exec` `Process` (brk, signal table, parent) + `akuma-fdtable` per process; amd64 has no Task record — fd effects live in `amd64/src/fd.rs` over `akuma-fdtable` | keep short-term: it is the fd-effects owner the arms need; fold into Process-fields slice-by-slice (not this switch's critical path) |
| `clone_user_half`/`clone_user_tree`/`fork_share_memory` | ≈115 (3251–3367) | private effect hook | stays as the `ExecRuntime::fork_share_memory` arm (amd64's is also target-local, `usermode.rs` fork hook); `akuma-cow` exists for a later CoW slice | eager copy is the honest answer today (gaps.md) |
| `load_task` (ELF load + stack build) | ≈240 (3367–3608) | private | `akuma-elf-exec` owns the pure exec decisions (PT_INTERP rule etc., `crates/akuma-elf-exec/src/lib.rs:1-12` — it names taoxichuan's loader as one of the three it exists to unify); amd64 wraps them in `amd64/src/loader.rs` (713 L) | S5. Note the crate doc says taoxichuan maps through `akuma-ring3-amd64` — verify which parts of `load_task` are already crate calls before rewriting (UNCHECKED line-by-line) |
| boot program list (`Prog`, `parse_programs`, `run_program`, `task_done`, `spawn`) | ≈280 (3044–3251, 3608–3668) | private init-by-list | S4: init registered as pid 1, each Prog a fork+exec child, waited with the crate wait | the boot task's NO_SLOT ring-3 residency is what the tick's slotted-current gate protects (`scheduling.md` § two gates) — S4 removes the resident |
| `dispatch_inner` | ≈1,400 (1647–3044), ~175 match arms | arms per `syscalls.md` table (~90 rows; ~45 rows already name a crate as decider) | S6: legacy numbers route through `akuma-syscalls-abi` → `akuma-syscalls-glue` (amd64 two-table shape, premise 7); private rows that die with M2: SPAWN 0x1301, WAITPID 0x1303, CLOSE_CHILD_STDIN 0x1326, the M2 clone arm | the arm count barely shrinks — the *deciders* move; count of arms that become pure calls into `akuma-syscalls-*` families: ≈30 of the fs/stat/dirent/time/uts/poll rows are glue-adoptable when a stage needs them; fd/net rows already are crate calls with local marshaling |

### 1.4 `pipes.rs` (290 L) — keep

`table`/`fire`/`may_park`/`park_once` (45–84) are the threading-park glue;
`pipe_arm`/`pipe2_arm`/`read_arm`/`write_arm*` (84–247) are arms over
`akuma-pipes` (semantics already crate-owned); `clone_ref`/`release*` (247–290)
are the fd refcount effects. No private scheduling lives here; only `park_once`
re-points when the M2 pipe-park (PIPEPARK site) moves onto the threading park.
amd64 analog: `amd64/src/pipe.rs` (369 L, same shape).

## 2. How amd64 does it, concretely

### 2.1 One `fork` + `execve` + `wait4`

- **fork** — `amd64/src/usermode.rs:5520` `sys_fork`: `alloc_pid()` →
  `akuma_exec::process::fork_process(child_pid, 0)` (`:5563`). The crate does the
  whole fold (its five slices are listed in the comment `:5525-5540`): child address
  space through the `ExecRuntime::fork_share_memory` hook, fd table through
  `akuma-fdtable`'s fork copy, `Process` registration with `parent_pid`, thread slot
  claimed in the pool and seeded from the parent's saved `UserContext`
  (`get_saved_user_context` / `spawn_user_closure_initializing`, slices 5), then
  published runnable. What only the kernel can name binds through
  `ExecRuntime::bind_child_task` (`:5541-5546`): the CR3 root, the SPAWN row, the
  proc_slot. Child exit path: the crate's `ProcessChannel`/`CHILD_CHANNELS` slice
  (slice 3) is where a waiter registers.
- **execve** — `usermode.rs:5112` `sys_execve`: resolve symlinks + read image head
  (`fs.rs`), `Image::from_elf_argv` (`amd64/src/loader.rs`; `akuma-elf` +
  `akuma-elf-exec` decisions), then the process/image swap inside `akuma-exec`, and
  the `enter_user` hook (`usermode::enter_ring3`) owns the return-to-ring-3 loop and
  teardown (`exec_runtime.rs` doc, `runtime()` field table). The new image starts
  with a fresh `UserContext` (fs base 0 until musl's `arch_prctl` — the same answer
  taoxichuan's exec crossings hand, `processes.md` § fs discipline).
- **wait4** — `usermode.rs:6153` `sys_waitpid`: "whose children" is
  `akuma_exec::process::find_process` + `is_child_of` over the process table
  (`:6176-6181`; the doc records the old global-spawn-table hang);
  no matching child → `ECHILD` (`:6193`); matching exited child → reap pid+code
  from `exited`/`exit_code` published by `spawn_record_exit` before the wake
  (`:6189-6194`); none exited → register on the child channel and
  **park** (`sched::block_current`, `sched.rs:1157`); wake via
  `sched::wake` (`:1200`); amd64 additionally arms `install_untimed_park_backstop`
  (`:1082`) — the lost-wakeup insurance whose absence pl found in taoxichuan's
  crossing.
- **preemption** — `amd64/src/lapic.rs:88` `on_tick` does two atomics +
  `sched::set_need_resched`; the timer return path calls
  `sched::preempt_if_needed(from_user)` (`amd64/src/idt.rs:1382` → `sched.rs:944`):
  BKL enter, `yield_now` (`sched.rs:2539`), BKL leave. The switch itself is
  `hook_switch_to` (`sched.rs:439`): CR3, fxsave/fxrstor order, syscall stack,
  trap stack.
- **idle** — `sched::idle_loop` (`sched.rs:1387`): BKL tag, retired-process drain
  (`akuma_exec::process::reclaim::drain_retired_if_requested`, `:1418`), the
  thread-slot reaper, `hlt`.

### 2.2 One SSH session start

1. Boot: `amd64/src/main.rs:209` `kmain` → `boot::early_init` (GDT, per-CPU/BKL,
   IDT, SMAP, `sched::init` — `boot.rs:94-100`) → `boot::late_init` calls
   `exec_runtime::init()` (`boot.rs:128`) which registers `ExecRuntime`+`ExecConfig`
   and `reserve_pid_floor(2)` so the first allocated pid is 2 — **pid 1 belongs to
   init** (`exec_runtime.rs:625-632`).
2. init: `usermode::run_init(path, args)` (`usermode.rs:8845`): read image,
   `Image::from_elf_argv`, `spawn_process_task(6, root)` (`:4109`),
   `register_exec_process(1, 0, task_slot, proc, …)` (`:4534`), `publish_task`
   (`sched.rs:1628`). Nothing ever unregisters init (`:8896-8899`).
3. sshd is a normal child of init/herd; each connection is a **fork**
   (`fork-sessions` feature ON by default, `userspace/sshd/Cargo.toml`) and the
   shell is spawned by `libakuma` `spawn_full` — on taoxichuan today that is the
   private SPAWN 301; under `linux-abi` it is `pipe2`+`fork`+`dup3`+`close`×2+`execve`
   (`libakuma/src/lib.rs:2196-2230`), polled with `wait4(WNOHANG)` (`:2493-2513`).
   amd64 also still serves SPAWN 301 (`usermode.rs:1240`) for `spawn_console`/TLS
   users — serving the private family and using the Linux one are not exclusive.

### 2.3 What is amd64-only and would need extraction first

**Nothing blocks the switch.** The pieces taoxichuan lacks are of two kinds:
(a) already crate-side (`fork_process`, the process table over `akuma-slot-table`,
`is_child_of_group`, exit channels, `akuma-elf-exec` decisions) — pull, don't
extract; (b) target glue that amd64 also keeps in `amd64/src/` (`run_init`
registration ≈80 L, `spawn_process_task`, `publish_task`, `hook_switch_to`,
`FxArea`) — write taoxichuan's own ≈150 L in `exec_twin.rs`/`child.rs`/`main.rs`,
mirroring aarch64's `akuma_kernel_glue::build_exec_runtime` precedent
(`exec_runtime.rs` doc: "The AArch64 twin is `akuma_kernel_glue::build_exec_runtime`").
A new `forbid(unsafe_code)` crate is warranted only if slice work discovers a
*shared* pure decision written twice; the one candidate seen is the init
registration sequence (S1) — keep it as taoxichuan-local code first.

### 2.4 Hooks: twin vs amd64, registered differently

| hook table | amd64 | taoxichuan twin today | change needed |
|---|---|---|---|
| `ExecRuntime.enter_user` | `usermode::enter_ring3` (owns execve loop + teardown) | the M2 `child_enter` crossing / `twin_enter_user` (installs fs shadow) | keep `twin_enter_user`; it must become the only entry once the crossing dies |
| `ExecRuntime.fork_share_memory` | real page-table walk | `main.rs::fork_share_memory` (clone_user_tree) | keep |
| `ExecRuntime.bind_child_task` | CR3 + SPAWN row + `UserCtx::proc_slot` | `child::bind_child_task` stamps SpawnSlot + depth + out_slot | re-point at Process+pool; drop depth/out_slot |
| `ExecRuntime.uptime_us` | one clock | `akuma_lapic_amd64::ticks() × 10 ms` | keep (one-clock rule) |
| `ProcessHooks.pid_for_thread`/`find_pid_by_thread` | process table + `THREAD_PID_MAP` | SpawnSlot ring scan | re-point at the crate table (S2) |
| `ProcessHooks.clear_draining`/`drain_in_flight` | retired drain state | no-ops | stay no-ops **with the stated reason** until the reclaim slice (`AKUMA_AMD64_STALE_FALSE_HOOKS.md` lesson); if S2 parks waiters on channels, drain must become real then |
| `X86ArchHooks.switch_to` | CR3 + FPU + stacks | five effects, **no FPU save** | keep; FPU is R6, not this switch |
| `X86ArchHooks.can_run` | pinning-aware | "any ring slot, the boot task, or idle" | after S1 the boot task is a Process; keep answering true on one core |
| `X86ArchHooks.allow_tick`/`tick_window` | one-instruction sti window | same | keep |

## 3. Target architecture (one page)

Taoxichuan becomes what amd64 is: **crates own every process/scheduling decision;
`taoxichuan/src/` is only target glue** (trap entry, CR3/fs effects, device arms).

1. **init is the first `Process`, with a real slot.** At boot, the first ring-3
   program is registered like amd64's `run_init`: `register_process(pid=1)` with a
   claimed threading slot published `READY`, `reserve_pid_floor(2)` so children get
   pid >= 2, and the twin's `bind_child_task` reduced to binding CR3 + pool slot
   (`exec_twin.rs:487+`). From then on `fork`, `execve`, `wait4` work from init —
   today they answer `EPERM`/`ENOSYS` from the boot task (`child.rs:868-886`;
   syscalls.md "Known deliberate gaps"), which is the only reason sshd needs private
   `SPAWN`. The tick's `PREEMPT_ARMED` and slotted-current gates (scheduling.md)
   are re-derived: `PREEMPT_ARMED` dies when there is no NO_SLOT ring-3 resident
   left (S4); the slotted-current gate stays until it provably cannot fire.
2. **sshd is a Linux-spawning user program.** Built with `libakuma`'s `linux-abi`
   feature: session shell = `pipe2` + `fork` + `dup3` + `execve`
   (`libakuma/src/lib.rs:2196-2230`), collection = `wait4(WNOHANG)` (`:2493-2513`).
   `SPAWN` (0x1301), `WAITPID` (0x1303), `CLOSE_CHILD_STDIN` (0x1326) and the M2
   blocking-fork arm are **deleted** (`child.rs:919-1454` plus the dispatcher
   arms); `clone` keeps working for musl `posix_spawn` by routing
   `CLONE_VM|CLONE_VFORK|SIGCHLD` through `fork_process` with the child-stack
   argument — amd64's shape (`usermode.rs:5585`), not the M2 body. pty-less
   behaviour is unchanged (no pty in this kernel; linux-abi `spawn_full` ignores
   the flag bits, `libakuma/src/lib.rs:2196-2201`).
3. **Syscall numbers come from the ABI crate.** The legacy x86_64 numbers
   (mkdir 83, rename 82, unlink 87, rmdir 84, readlink 89, chmod 90, chown 92/94,
   symlink 88, ...) are served the amd64 way, not by new private arms: the legacy
   table lists x86-only numbers, translates each to its `Syscall::*`
   (`akuma-syscalls-abi`), and routes into the `akuma-syscalls-glue` family
   dispatch, which calls back through the twin's `ExecRuntime` hooks (ext2, fd
   table, console). amd64's smoke test — "no number is in both tables"
   (`usermode.rs:7506-7517`) — is adopted as a boot self-check. A number
   taoxichuan cannot serve yet still prints `unhandled <Name> (<nr>)` (the
   checklist rule, syscalls.md).
4. **Dispatcher shape.** Of `dispatch_inner`'s ~175 arms over ~90 table rows
   (syscalls.md): the fd/net/fs rows already call crates and stay (marshaling is
   target glue); ~30 fs/stat/dirent/time/uts/poll rows become `akuma-syscalls-*`
   family calls as stages need them; exactly four private rows die (SPAWN, WAITPID,
   CLOSE_CHILD_STDIN, M2 clone); `execve` stays as an `akuma-exec` call.
   `getpid`/`getppid`/`gettid` re-point at the process table (today: literals —
   the "1: one task" rows in syscalls.md — no longer true once children are real
   processes).
5. **Scheduling.** The pool (`akuma-threading`) owns runnability: every task —
   init, children, sshd, idle — is a pool slot moved `INITIALIZING -> READY ->
   RUNNING -> (WAITING)`; the tick preempts ring 3 via `x86_yield` (already live);
   parked waits are `WAITING` slots woken by the crate's waker; the idle thread
   halts and gains the retired-reclaim drain. Nothing hand-rolled remains except
   `switch_to`'s five machine effects (CR3, syscall band, trap stack, TASK_SLOT,
   fs shadow) — the same set amd64's `hook_switch_to` has, minus FPU (R6).

## 4. Slices

Conventions for every slice: gates that must stay green — F1, PK1, PK2, P1-P5
(`run_fork.sh`/`run_pipes.sh`), `apk add curl` + curl HTTPS (stage 7), sshd +
`sh -c` over ssh (stage 4); a stall on an apk boot before S3 is retried (pl: it
needs the box's KVM timing; harness `scripts/kvm/goose/run_stallrate.sh`), never
scored a FAIL; every slice carries a `REAL_FORK`-style const so the old path stays
selectable until its gate passes, and the deleting commit comes only after the
green run; ops restore after any boot (the brief's list). Budget rule: ~100 tool
calls per slice, one executor session each.

### S0 — baseline battery on the merge tree (~15 calls, no code)
- Files: none. Run the full battery once on the `rust-org-il-experiment` tip;
  record per-gate console lines as the reference verdicts.
- Gate: battery green; note the apk-stall rate (expect pl's ~1-in-3 on the box).

### S1 — init is a Process (~40 calls)
- Touch: `exec_twin.rs` (`init()`: add `reserve_pid_floor(2)` + registration of
  the first program as pid 1 — new ~80 L mirroring `register_exec_process(1, ...)`
  fields, `usermode.rs:8890-8902`); `child.rs` (`register_shell_process` becomes
  the init path; `fork_arm_real`'s depth gate: depth 0 now legal for pid 1);
  `main.rs` (`spawn()`/`run_program`: register + publish before entering ring 3).
- Rollback: `const INIT_AS_PROCESS: bool = true` — false restores today's
  unregistered boot spawn.
- Gate: battery green; new console line `[init] registered pid=1 slot=<n>`; F1
  still green from the shell (same rung as today, now via the process table).
- Host test: none (kernel glue). One boot.

### S2 — wait4 becomes the crate-owned park/wake (~50 calls)
- Touch: `child.rs` — replace `wait4_real`/`wait4_arm_blocking_scan`/`waitpid_arm`
  bodies (~140 L) with the amd64 `sys_waitpid` shape over
  `akuma_exec::process::find_process`/`is_child_of`/`exited` (read
  `crates/akuma-exec/src/process/` for the exact channel/waiter API first — names
  UNCHECKED in this plan); `task_exit_real` slims to publish-exit + wake;
  `exec_twin.rs` `ProcessHooks.pid_for_thread` re-points at the crate table;
  **delete `PARENT_OF`** (`child.rs:643-664`, ~25 L) — the crate's `parent_pid` is
  the only link (amd64 `usermode.rs:6165-6172`).
- Keep: the boot-task scan arm until S4 (the M-gates/bridge reaper path).
- Rollback: `const WAIT_VIA_CRATE: bool = true`.
- Gate: PK1 (`20000\n`), P1-P5, F1 green; grandfork step 5 -> exit 8 (ECHILD);
  one `[tmp]` print at the first park/wake pair, removed the same session.
- Host test: none in-kernel; the crate's own wait tests (akuma-exec
  children.rs tests :1859-1941, per pj's RESULT) are the reference for the
  expected answers.

### S3 — sshd goes linux-abi; the M2 crossing dies (~60 calls; the stall slice)
- Touch (userspace): the taoxichuan image's sshd build enables
  `libakuma/linux-abi` (feature wiring point UNCHECKED — see Q1); rebuild the
  userspace image.
- Touch (kernel): behind `const SSHD_LINUX_ABI`, delete `spawn_arm`,
  `exec_replace`, `exec_return`, `child_enter`, `child_exit`, `exit_cross`
  (~535 L), the SPAWN/WAITPID/CLOSE_CHILD_STDIN arms, `read_child_out` +
  the `ChildOut` fd kind (~150 L with `pipes.rs`/`main.rs` side effects), and the
  M2 clone body (clone re-routed to `fork_process(child_pid, child_stack)`,
  `usermode.rs:5585` shape; keep the flags check). `nested_active`, depth stamps,
  syscall-band splitting all die with the crossing.
- Order inside the slice: (1) add the clone-through-fork_process arm and boot it
  (gate: forktest + apk green with sshd still on SPAWN); (2) switch sshd to
  linux-abi, boot (gate: ssh `sh -c`, apk add curl, curl HTTPS — **the first boot
  with no M2 crossing on the apk path**); (3) delete the dead arms + update
  `syscalls.md` rows in the same commit.
- Rollback: rebuild with the const off + the sshd feature off (both switches are
  build-time).
- Gate: the stage-4 ssh gate plus the stage-6/7 gates; a `[tmp]`-free console.
- Host test: `cargo test -p libakuma --target $HOST` still passes (its spawn
  cfg-gated code is guest-only, but the build must not regress).

### S4 — boot programs become fork/exec children; the boot-task arms die (~40 calls)
- Touch: `main.rs` — `run_program` forks+execs each `Prog` from the registered
  init and waits via the S2 wait; delete `task_done`'s chain-boot and
  `wait4_arm_blocking_scan` (~150 L); `child.rs` loses `is_real_task` and the
  boot-task special cases; re-derive the tick gates (PREEMPT_ARMED deleted if no
  NO_SLOT ring-3 resident remains — check the boot path still has one between
  kmain and init's registration; if it does, keep the gate with the reason).
- Rollback: `const INIT_FORKS_PROGS: bool = true`.
- Gate: stage 1-3 console lines (`stage 1 gate reached`, `hello exit code 0`,
  `stage 3 gate reached`) + the full battery.
- Host test: none. One boot.

### S5 — ELF loading onto `akuma-elf-exec` (~50 calls; stage-8 enabler)
- Touch: `main.rs::load_task` (~240 L) re-based on `akuma-elf-exec`'s decisions +
  `akuma-elf` parsing, keeping taoxichuan's mapping (physmap writes, stack build)
  — amd64's `loader.rs` (713 L) is the reference shape; gains the PT_INTERP rule
  the crate exists for (stage-8 `git` is likely dynamic — UNCHECKED).
- Rollback: `const LOAD_VIA_CRATE: bool = true`.
- Gate: `hello exit code 0`, F1, apk add curl, curl HTTPS.
- Host test: none (taoxichuan is a no_std bin, per pj's RESULT); the crate's own
  tests cover the decisions.

### S6 — dispatcher two-table shape (~40 calls; stage-9 enabler)
- Touch: `main.rs::dispatch_inner` — add the `Syscall::from_x86_64` fallback arm +
  the x86-only legacy table with `to_glue`-style translation (start with the
  mkdir/rename/unlink/rmdir/readlink/symlink/chmod/chown families where the twin's
  hooks already answer); adopt amd64's disjointness self-check as a boot line;
  delete any arm it provably duplicates.
- Rollback: `const GLUE_DISPATCH: bool = true`.
- Gate: battery + the new self-check line + one mkdir-via-83 probe program
  (the `userspace/forkprobe` pattern).

### S7 — sweep (~25 calls)
- `grep -n 'SpawnSlot\|CrossSave\|EXEC_SAVE\|PARENT_OF\|SPAWN' taoxichuan/src/`
  -> expect only hooks; update `taoxichuan/docs/reference/{processes,scheduling,syscalls}.md`,
  the five diagrams, `gaps.md` (G1 dies with PK2's M2 scaffolding; G3 "no pump
  while a spawned child runs" is crossing-shaped and should die with S3), and the
  `switch-to-crates.md` doc. Delete any now-unused ring type.

**Cut order if only half fits:** drop S6, then S5 (both are stage-8/9 enablers);
fold S7's doc updates into the S3/S4 commits. The thesis set is S0-S3: after S3,
taoxichuan has no private process machinery left on any path userspace exercises
(init, fork, exec, wait, pipes, fds) — any remaining ring types are dead code that
S7 removes.

## 5. Risks and unknowns (each with the cheapest resolution)

Hazards grepped in `docs/archive/` + `docs/reference/subsystems/` as instructed;
each row names where it is recorded.

| # | risk / unknown | recorded at | cheapest resolution |
|---|---|---|---|
| R1 | **Stale thread slot on bind** — a bind that seeds only one of proc_slot/thread_slot lets `enter_ring3` read the previous occupant's slot and walk into thread-exit teardown for a process that never ran (triple fault) | `docs/archive/BUG_FIX_LIST.md:2343` | code read at S1: taoxichuan's `bind_child_task` (`child.rs:443`) must stamp both; assert in the hook |
| R2 | **Stale trap frame across a preempting switch** — a tick that switches must leave the interrupted frame on the interrupted task's own stack, or the next ring-3 trap lands on a foreign stack | `taoxichuan/docs/reference/scheduling.md` § two gates (`arm_trap_stack`); `docs/archive/CONTEXT_SWITCH_BUGS.md:49,97` (IRQs stay masked through the switch) | already paid for in slice 3; S2's new park must use the same `x86_yield` path — code read + the PK battery |
| R3 | **Switch while IRQs masked / stale CR3** — switching onto a dead fork-child's CR3 (exec-swept, zero pages) kills the resumed task; single-core reload discipline: CR3 per switch, 0 = leave for idle | `docs/archive/TAOXICHUAN_M4_SLICE3_PE_RESULT.md:161`; `taoxichuan/docs/reference/scheduling.md` § switch_to | S2: the exit path must tear the slot down before the slot number is reusable (pool `TERMINATED → FREE` cooldown is the crate's job — verify `x86_reaps` runs, amd64 needed a reaper for exactly this, `amd64/src/sched.rs:1421-1426`) |
| R4 | **fd-table refcounts on fork** — the first post-fork close destroyed a pipe under two live fds until the counted-`clone_ref` walk existed; the pipeline deadlock family came from this | `taoxichuan/docs/reference/processes.md` § real fork; `docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` | already crate-owned (`akuma-fdtable` fork_copy + `pipes.rs:247` clone_ref); S3's clone reroute must keep taking the counted refs — P1–P5 are the gate |
| R5 | **TLS/fs base on exec** — a depth-2 exec entered the new image with the outgoing task's TLS pointer; `fs:0x28` stack-guard read hit unmapped bss (PK2) | `taoxichuan/docs/reference/processes.md` § fs discipline; `RESULT-pj.md` pi findings | keep the exec-zeroes-fs rule verbatim through S3/S5; gate: PK2 |
| R6 | **FPU/SSE state is not saved across switches** (amd64 `FxArea` fxsave/fxrstor, `sched.rs:116-278`; taoxichuan `switch_to` has no FPU arm) — if ring 3 uses SSE today, preemption can corrupt FPU users; today's gates passing suggests either no FPU use or laziness UNCHECKED | gap, not yet recorded | one host-free probe: read `fault_trap` (`main.rs:262`) for a `#NM` arm + check CR0.TS at boot (one boot); if ring 3 touches SSE, an FxArea slice must precede S5 (stage 9 rustc needs it regardless). Not a blocker for S0–S4 on current evidence |
| R7 | **Orphan reaping** — the crate reparents orphans to init (`crates/akuma-exec` children.rs:310, per pj); today taoxichuan honestly leaves them unreapable (no init). Once init is a Process (S1) that answer becomes a leak: abandoned grandchildren pile up one slot each | `taoxichuan/docs/reference/processes.md` § wait4 is parent-owned; `RESULT-pj.md` step-5 story | S1 registers init's Process; S2 gives init a real wait loop it can park in — the boot program list wait (S4) is the reaper's first customer; idle-drain (amd64 `sched.rs:1418`) folds into S4 |
| R8 | **INITIALIZING-slot poison** — anything published READY with half-built context is unpickable-by-design but wakeable-by-bug; the M2 SPAWN child's pool slot stays INITIALIZING forever today (the supervisor's stall pointer) | `docs/reference/subsystems/scheduler.md:28-32`; pl pointer | S2/S3 code read: the new publish path must move INITIALIZING→READY exactly once, after the trap frame + CR3 + fs shadow are written; PK/F1 gates catch a wrong order as a first-tick fault |
| R9 | **linux-abi is unwritten-into anywhere** — S3 is the tree's first consumer of `libakuma/linux-abi` `spawn_full`; compile errors or ABI mismatches (x86_64 numbers at `libakuma/src/lib.rs:322`) are untested ground | premise 6 | S3 step 0: build sshd with the feature for the amd64-style target or on host (`cargo check`) before the boot; if it breaks, the fallback is a taoxichuan-local `posix_spawn`-shaped helper in libakuma (compose from the same five syscalls) — same syscall set, no kernel change |
| R10 | **The boot task's NO_SLOT residency window** — between kmain and init's registration there is still ring-3-free kernel time; the slotted-current tick gate must survive until S4 proves no resident remains | `taoxichuan/docs/reference/scheduling.md` § two gates | S4: keep the gate; delete only with the proof line in the commit message |
| R11 | **Stall interference with gating** — until S3, apk-path boots can still stall (~1-in-3 on this box, KVM-timing dependent); a slice's green gate could be drowned in retries | `RESULT-pl.md` (closed, no fix); brief 09m (supervisor: 10/10 clean under QEMU TCG) | the supervisor's local QEMU loop is the arbiter for any verdict a stall muddies; on the box, use `run_stallrate.sh`'s window discipline and retry |

## 6. Cost estimate (executor tool calls)

| slice | calls | of which boots | notes |
|---|---|---|---|
| S0 | ~15 | 1 battery | reference verdicts |
| S1 | ~40 | 1–2 | registration glue |
| S2 | ~50 | 2 | + one boot per `[tmp]` add/remove |
| S3 | ~60 | 3 | userspace rebuild ×2, kernel boots ×2–3 |
| S4 | ~40 | 1–2 | |
| S5 | ~50 | 1–2 | loader re-base |
| S6 | ~40 | 1–2 | + probe program |
| S7 | ~25 | 0 | docs + dead code |
| **total** | **~320** | **~10–13 boots** | ≈ 3 executor sessions of ~100 calls |

Cut to half (~160): S0 (assume pj's green battery), S6, S5, and fold S7 into
S3/S4 — leaving S1+S2+S3 ≈ 150 calls with boots ≈ 6–7. The thesis survives that
cut; stage 8 (`git clone`) loses its loader upgrade (dynamic-git risk, R6-class).

## 7. Docs read, and questions for the user

Read (in order): `taoxichuan/docs/README.md`, `gaps.md`, `reference/processes.md`,
`reference/scheduling.md`, `reference/syscalls.md`, `runbooks/gates.md`;
`/root/dev-cycle/taoxichuan-s7/RESULT-pl.md`, `RESULT-pj.md` (+ pi/pf findings
carried there); `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md`,
`docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` (hook tables);
`docs/reference/subsystems/scheduler.md` (§ threading model, slot state machine),
`scripts/check_unsafe_budget.py` (sites-not-lines; new crates start at budget 0);
`userspace/sshd/Cargo.toml`, `userspace/libakuma/src/lib.rs` (`linux-abi`,
`spawn_full`, `waitpid_status`, `spawn_pty`); `amd64/src/`: `exec_runtime.rs`
(all), `usermode.rs` windows (`sys_fork`, `sys_execve`, `sys_waitpid`,
`AKUMA_PRIVATE_BASE`, `run_init`, `X86_ONLY`), `sched.rs` windows
(`preempt_if_needed`, `idle_loop`, hook table), `lapic.rs::on_tick`, `boot.rs`
(late_init); crates' top docs: `akuma-slot-table`, `akuma-scheduler` (the
simulator), `akuma-threading`, `akuma-elf-exec`. UNCHECKED (not read):
`taoxichuan/src/sock.rs`/`net.rs` internals (not process-model),
`crates/akuma-exec/src/process/` module files (only their names and the pj-cited
children.rs line ranges), `taoxichuan/src/main.rs::dispatch_inner` arm-by-arm
(counted, not read), `userspace/libakuma/src/syscall/proc.rs` (SPAWN_FLAG_PTY
producer side), amd64 `fd.rs`/`mm.rs` bodies, `PLAN-pj.md` (superseded by its
RESULT).

Questions for the user:

- **Q1 (S3 blocker).** Where should `libakuma`'s `linux-abi` feature be switched
  on for the taoxichuan image — `userspace/sshd/Cargo.toml` default, the
  taoxichuan image build script, or a `--features` flag in the build incantation?
  No build in the tree enables it today (premise 6); the plan needs the one
  wiring point you want.
- **Q2 (S3 scope).** Must `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)` (musl
  `posix_spawn`) keep working at S3 for stage 8 (`git`'s helpers are fork+exec,
  but compile drivers and some helpers use `posix_spawn` —
  `TAOXICHUAN_PROCESS_MODEL_PLAN.md` § 1)? The plan assumes yes and keeps a
  clone-through-`fork_process` arm; confirm, or S3 deletes clone entirely.
- **Q3 (R6).** Is ring 3 expected to touch SSE/FPU on taoxichuan today (CR0.TS
  state at boot)? If yes, an FxArea slice must be inserted before S5 (and ideally
  before S2, since a preempted FPU user corrupts on any switch).
- **Q4 (S1 shape).** Is "the first boot-list program is registered as pid 1"
  acceptable init, or do you want a dedicated `/sbin/init` userspace binary now
  (amd64's `herd`-as-init shape)? The plan assumes the former (no new userspace).
- **Q5 (gating).** For verdicts the box's apk stall muddies (S0–S3), is the
  supervisor's QEMU TCG loop the arbiter of record, with `run_stallrate.sh`
  retries on the box only for regression *rate*?

## 8. The stall (one paragraph)

**The switch removes the stall by construction — but only at S3, and every slice
before S3 still boots the fragile path, so the stall must be tolerated, not
assumed fixed, until S3's gate passes.** After S3 there is no run-inside-the-
syscall child and no suspended spawn-parent frame: the parent's wait is a
registered thread park (`WAITING` in the pool, woken by the crate's child-exit
wake — the amd64 `sys_waitpid` shape), and the child is an ordinary published
slot the tick and `x86_yield` already pick up. pl's lost thing — the crossing
back from the spawn child into the suspended `CrossSave`/`EXEC_SAVE` frame,
which `[TICK post]`/`EXE resumed` never reached — has no counterpart in that
design; likewise pj-7's shape (the SPAWN parking with nothing to wake it)
disappears because nothing parks outside the pool's woken states. The
supervisor's pointer is consistent and explains the today-shape: an M2 SPAWN
child's pool slot stays `INITIALIZING` (reserved, never published READY —
`scheduler.md:28-32` says the pool will not pick such a slot), so even the
switching tick cannot rescue a crossing that loses its resume; under
`fork_process` children are published runnable, so the class dies with S3.
Until then: S0–S2 gate on the same SPAWN path and must treat a stall as a retry
(R11), never as their own failure.

## Decisions by the user (2026-10-30; supervisor recorded, glm's text above is unchanged)
- Q1: `linux-abi` is a **cargo feature** (sshd and herd). Q2: keep `clone` through `fork_process`; move it to not-implemented if it breaks. Q3: FPU/SSE save is needed (add an FxArea slice before S5). Q5: the supervisor's QEMU TCG loop is the arbiter; metal confirmation runs in the background afterwards.
- **Q4 replaces S1's shape: init is `herd`**, built with `libakuma/linux-abi` (fork+pipe2+dup3+execve, no private syscalls), with non-Linux-compatible parts switched off by the same feature in herd if it has none. So pid 1 is a real userspace program started by the kernel, registered as a `Process`; the "first boot-list program is pid 1" and "dedicated /sbin/init" options are closed. S1's gate becomes: herd starts as pid 1 and forks a child; the boot program list shrinks to "start herd". Executor reads `userspace/herd` for its non-Linux calls first (SPAWN-class and Akuma-private uses) and plans them into S1/S3.
