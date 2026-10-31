# Taoxichuan: the crate-extraction plan (session X10, 2026-10-30)

**Stability: C** — a plan, not an extraction: this session changed no kernel
code. It extends [`taoxichuan/docs/reference/switch-to-crates.md`](../reference/switch-to-crates.md)
(the process/scheduling switch, slices S0–S7) with the *rest* of the tree; the
slice table there and the extraction list here are one programme read from two
ends. Composition rule as applied below: every row says **pulled** (the crate
already owns it — delete the local copy), **extract** (move to an existing
crate), or **write new (why)**, and names the crate.

## Premises re-checked 2026-10-30 (this tree, `taoxichuan-s4` merge of `rust-org-il-experiment`)

- **HOLDS** — `wc -l taoxichuan/src/*.rs` = 10,350 total (`main.rs` 5,374,
  `child.rs` 2,341, `sock.rs` 1,029, `exec_twin.rs` 627, `net.rs` 385,
  `pipes.rs` 355, `futex.rs` 201, `ring.rs` 38).
- **WRONG** — the "~47 `akuma-*` path dependencies" figure: it is **37** for
  `taoxichuan/Cargo.toml` and **78** for `amd64/Cargo.toml`.
- **HOLDS** — `ls crates/ | wc -l` = 107; 59+ carry `#![forbid(unsafe_code)]`
  (`docs/reference/crate-safety.md`; every crate named in the tables below
  that is marked FORBID was re-checked by grepping its `lib.rs`).
- **HOLDS** — `grep -c unsafe taoxichuan/src/*.rs`: main.rs 28, child.rs 26,
  exec_twin.rs 5, net.rs 2, ring.rs 1, the other four files 0 (word count, not
  site count; 62 total).
- **WRONG** (premise of the brief's still-missing list): `tkill` (200) is
  **served** (`TKILL`/`TGKILL` → `tkill_arm`, main.rs:4353-4354); epoll
  213/232/233/281/291, `prctl` 157, `statx` 326, `sendfile` 40, `linkat` 265
  and `getrusage` 98 are confirmed still missing (no arm in `dispatch_inner`).
- **UNCHECKED** — every "deduplicated against `amd64/src/`" figure is a
  read-of-the-other-tree estimate, not a measured diff; the amd64 side has not
  been compiled against any extraction proposed here.

---

## Per-file tables

Legend: **order** = the ranked extraction it belongs to (§1), or `—` = keep as
target glue / delete by a switch-to-crates slice (S#). "pure?" = host-testable
without a machine.

### `main.rs` (5,374 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| Early machine: `cmdline`/`cmdline_token`, trap stacks, GDT/TSS, `gdt_init`, IDT, `lapic_init`, `idt_init` | 116-260, 608-751 | how a CPU comes up on this machine | **pulled**: `akuma-gdt`, `akuma-idt-amd64`+`akuma-trap-amd64`, `akuma-lapic-amd64` | `amd64/src/idt.rs`, `lapic.rs` (same crates) | — (composition root keeps the wiring) | no | keeps the boot `unsafe` (irelevant to count: one-time) | — |
| Page walk + PTE type: `leaf_entry`, `Pte`, `PTE_W/_COW/_D`, `page_written`/`mark_written`, W^X gate `wx_on` | 315-395, 324-343 | how a VA→PA leaf is read and a PTE bit poked | partially **pulled**: `akuma-cow` (the `decide` policy) | `amd64/src/mm.rs` (own walker, same PTE bits) | **extract** walker into `akuma-pte-amd64`; W^X policy as `akuma-cow` trait calls | yes (over a mapped-table abstraction) | sheds ~4 sites (`&mut *pte` writes) | **X4** |
| CoW effects: `cow_resolve`, `cow_prepare_write`, `cow_privatize`, `cow_privatize` counters | 396-481, 346-353 | when a shared frame breaks, and who answers what | partially **pulled**: `akuma-cow::decide`; refcounts **pulled**: `akuma-pmm::cow_table` (flat chunked table, 2026-10-30) | `amd64/src/mm.rs` (same three effects) | **extract** the effect trio behind a `PteIo` trait into `akuma-cow` (impls stay kernel-side, holding the PTE `unsafe`) | yes (trait mocked) | sheds ~6 → impl keeps 2 | **X4** |
| Fault entry: `fault_trap`, `grow_user_stack`, `report_unhandled*`, `timer_trap` | 279-314, 482-607 | what a ring-3 fault means (stack grow vs CoW vs kill) | no (target glue) | `amd64/src/mm.rs` + `usermode.rs` (same shape) | — stays | no | keeps its PTE `unsafe` until X4 | — |
| Slab memory manager: `add_region`/`add_hole`/`carve_hole`/`mmap_alloc`/`release_pages`, `mprotect_arm`, `msync_arm`, `munmap_arm`, `shared_writeback`, `MM_EVENTS` | 1102-1308, 1173-1197 | where an anon/file mapping is placed and torn down | partially **pulled**: `akuma-mmap` (has `holes.rs`) | `amd64/src/mm.rs` (its own hole list) | **extract** slab+hole policy into `akuma-mmap` (it already owns the hole type) | yes | sheds ~3 (PTE pokes move under X4's trait) | **X9** |
| User access: `user_ok`, `copy_from_user`, `copy_to_user`, `user_str`, `struct_bytes`, `e()` | 1474-1563 | how a syscall touches user memory (regions + physmap copies + CoW prepare-write) | no — `akuma-user-access` exists but is amd64-shaped (`amd64/src/uaccess.rs` is its client) | `amd64/src/uaccess.rs` (618 lines, same contract) | **extract** into `akuma-user-access` (FORBID stays; the physmap copy + `cow_prepare_write` go behind a `CopyPage` hook) | yes (fake pages in tests) | sheds ~6 (the crate holds the two copy loops) | **X3** |
| Task records: `OpenFile`, `Task`, `TASKS`, `task_slots()`, `task()`, `my_tid`, `bind_session_fd`, std-fd rebinding (`std_closed`/`new_fd_min`) | 994-1100, 1445-1472, 2216-2222 | what a task *is* (fd table, cwd, brk/mmap cursors, signal mask) and who may alias it | no — per-slot array of `static mut` | `amd64/src/usermode.rs` (per-process records over `akuma-exec`) | **write new**: this is the kernel's identity core; move the *shape* onto `akuma-slot-table::SlotTable` (FORBID) and `BorrowCell` closures — see §2 | yes (table ops) | sheds 2 (`task_slots`/`task` raw derefs) | **X10-adjacent** (§2) |
| fd/file arms: `fd_table`, `open_common`, read/write/writev/pread/copy_range, `stat`/`lstat`/`fstat`/`statfs`, `getdents64`, dup/dup2/dup3, `resolve_at`/`path_at` | 1616-2407, 3174-3600 | one file-description model over ext2 + session fds | decisions **pulled**: `akuma-fdtable` (lowest-free/dup/close), wire **pulled**: `akuma-syscalls-linux`, fs **pulled**: `akuma-vfs`+`akuma-ext2` | `amd64/src/fd.rs` (3,819 lines, same model) | **extract** the `OpenFile`/session/stat-fill model into `akuma-vfs-glue` (exists; amd64-only dep today) | yes (over a `FileIo` trait) | sheds ~4 (all via X3's copies) | **X7** |
| eventfd cluster: `EVENTFDS`, `eventfds`, `eventfd2_arm`, read/write/close-slot | 2408-2496 | the 4-slot eventfd table | **pulled**: `akuma-syscalls-eventfd` (owns the counter algebra; the *table* is local) | `amd64/src/fd.rs` (EventFd ids) | **extract** the fixed table into the same crate (it is 90 lines of slot bookkeeping) | yes | none | **X5** |
| signal arms: `sigaction_arm`, `sigprocmask_arm`, `sigaltstack_arm`, `tkill_arm` | 1310-1454 | validate + store/reply for the signal family | algebra **pulled**: `akuma-syscalls-signal::procmask` (2026-10-30 fix), `SigTable`/wire | `amd64/src/signal.rs` (1,578 lines) | **extract** the marshal half into `akuma-syscalls-glue` (exists; amd64-only) | yes | none (uses X3 copies) | **X5** |
| `membarrier_arm` | 1418-1454 | probe the membarrier commands | **pulled**: `akuma_syscalls_mem::membarrier` | `usermode.rs` | **pulled** already; row stays as the arm | yes | none | — |
| trace rings + `nosys_note` + `dispatch`/`dispatch_inner` | 2498-2688, 2688-4372 | which number routes to which arm; legacy fold | fold **pulled**: `akuma-syscalls-abi::legacy` (2699); names **pulled**: `akuma-syscalls-abi` | `amd64/src/usermode.rs:1511,7506` (two-table shape) | dispatcher **stays** (target glue); `ring.rs` **extract** into `akuma-primitives` (write new module, 38 lines, FORBID-able) | yes (ring) | ring: sheds 1 | **X6** |
| boot program list: `Prog`/`Programs`/`parse_programs`/`run_program`/`task_done` | 4373-4634 | what the boot runs, in what order, with what argv | no | `amd64/src/boot.rs` (init= policy) | — stays (boot policy is per-kernel) | no | none | — |
| address-space fork/teardown: `copy_kernel_half`, `clone_user_half_into`, `clone_user_tree`, `free_subtree`, `in_shared_span`, `release_cloned_prefix` | 4635-4795 | how a root is duplicated subtree-by-subtree with shared leaves | **pulled**: `akuma-pmm` (frames), `akuma-cow` (counts) | `amd64/src/mm.rs` (fork_share) | **extract** the tree walk into `akuma-pagetable` (exists; amd64-only) over the X4 `PteIo` trait | yes | sheds ~3 | **X4** |
| `load_task` + `TaskImage` + `spawn` | 4796-5316 | ELF→address space placement, stack/arg block build | **pulled**: `akuma-elf-exec` (decisions) + `akuma-ring3-amd64` (stack build) | `amd64/src/loader.rs` (713 lines) | **extract** the placement loop onto `akuma-elf-exec` (switch-to-crates S5) | yes (image bytes in, layout out) | sheds ~5 (map/unmap behind `akuma-pmm` API) | **X8** |
| allocator + panic + halt | 125-135, 5317-5374 | the last-resort paths | **pulled**: `akuma-alloc` | n/a | — stays | no | keeps 1 (alloc error) | — |

### `child.rs` (2,341 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| SpawnSlot ring, `CrossSave`, `SlotStacks`, `SlotCross`, `SLOT_STATE`, `slots()`, `with_cross` | 40-265 | the run-to-completion child ring and its crossing save area | no (the thing S3 deletes) | none — amd64 never had this model | **write new: nothing — deleted** at S3 | — | sheds most of child.rs's 26 as the crossings die | S3 |
| child stdout buffer: `child_write`, `OUT_CAP`, `read_child_out` | 282-325, 2235-2257 | the fake pipe (`Vec` + cap, no reader blocks) | **pulled** for the real model: `akuma-pipes` | none (amd64 uses real pipes) | **write new: nothing — dies with SPAWN** at S3 | yes | none | S3 |
| argv/envp reader: `read_ptr_array`, `read_user_string`, `read_spawn_args`, `ARGV_MAX`/`ENV_MAX`/`STR_MAX`/E2BIG | 326-440 | the execve argument-block contract (448/256/32 KiB/512 total, `E2BIG`) | no | `amd64/src/usermode.rs:4923-5100` (same caps, same errno) | **extract** into `akuma-user-access` (`read_ptr_array` + caps) and `akuma-exec-core` (`SpawnArgs` policy) | yes (X3's fake pages) | none | **X3** |
| slot/task bind: `TASK_OF`, `task_of`, `bind_child_task`, `CHILD_BIND`, `sched_kstack_top`, `sched_task_slot_of`, `sched_root_of` | 446-631 | how a crate thread/fork lands on a ring slot with its own stacks | hook target **pulled**: `akuma-threading`/`akuma-exec` define the hooks | `amd64/src/exec_runtime.rs` (the same bind arm) | — stays (this *is* the twin's bind arm, the glue the crates require) | no | keeps ~4 (stack leak/init) | — |
| thread lifecycle: `group_pid`, `group_leader`, `kill_siblings`, `retire_thread_process`, `task_exit_thread`, `clone_thread_arm`, `is_thread_task`/`is_real_task` | 466-472, 611-798 | a thread's exit retires its Process; drain at entry | table **pulled**: `akuma-exec::process::table` | `usermode.rs` (same policy post `AKUMA_AMD64_THREAD_LIFECYCLE.md` §3) | **extract** the retire/drain policy into `akuma-exec` (a pure function over the process table) | yes | sheds 1 | **X10** |
| fs shadow: `SLOT_FS`, `slot_fs_of`, `set_slot_fs` | 799-826 | per-slot TLS base carried across switches | no (MSR glue) | `amd64/src/exec_runtime.rs` (fs in context) | — stays | no | keeps 1 (wrmsr via `akuma-cpu`-shaped call) | — |
| `PARENT_OF` + `stamp_child_parent`/`parent_of` | 827-840 | pid→parent stamp for parent-owned wait | no (replaced by `akuma-exec`'s `is_child_of_group`) | `usermode.rs` (crate table) | **write new: nothing — deleted** at S2 | yes | none | S2 |
| exec crossings: `exec_real_task`, `capture_user_context`, `register_shell_process`, `exec_replace`, `EXEC_SAVE` | 841-1696 | how a task replaces its image (fds survive; fs zeroed) | registration **pulled**: `akuma-exec`; loader **pulled**: `akuma-elf-exec` | `usermode.rs` `exec_replace` family | crossings **stay** (naked-asm glue); `register_shell_process` **extract** onto `akuma-exec` (it is amd64's `register_exec_process` re-typed) | no (asm) | keeps the naked-asm `unsafe` | S3/X10 |
| M2 spawn: `spawn_arm`, `child_exit`, `fork_arm`, `fork_arm_blocking` | 1142-1901 | the run-to-completion crossing | no | none | **write new: nothing — deleted** at S3 | no | sheds ~8 (the crossings) | S3 |
| wait family: `wait4_arm`, `wait4_real`, `wait4_arm_blocking_scan`, `waitpid_arm`, `close_stdin_arm` | 1902-2015, 2206-2234 | who may reap whom; park vs scan | decisions move to **pulled**: `akuma-exec` (S2) | `usermode.rs:6112 sys_waitpid` | **write new: nothing — S2 replaces with the crate's park/wake** | yes after S2 | sheds ~2 | S2 |
| root lifetime: `OWNED_ROOT`, `PENDING_ROOT`, `disown_space`, `park_root`, `adopt_exec_root`, `sweep_parked_roots`, `release_spawn_image`, `reap_process` | 2016-2234 | who frees a fork child's root and when | no | `usermode.rs` (`reap_exec_process`:4836) | **extract** "park a root until its last owner reaps" behind a `RootDrop` hook in `akuma-exec` | no (page-table free) | sheds 1 | **X10** |
| slot plumbing: `sleep_us`, `KSTACK_WORDS`, `leak_stacks`, `prepare_task_slot`, `write_user_context`, `read_user_context` | 2258-2341 | per-slot leaked stacks; ring-3 context capture | hooks **pulled**: `akuma-threading` (`X86ArchHooks`) | `exec_runtime.rs` | — stays (twin glue) | no | keeps ~5 (stack init/capture) | — |

### `sock.rs` (1,029 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| fd↔socket routing: `fd_socket`, `writev_socket`, `fd_nonblock`, `socket_refs`, `close_arm` | 31-80, 437-441, 909-937 | which fd is which socket; last-ref close | table **pulled**: `akuma-net`; fd decisions **pulled**: `akuma-fdtable` | `amd64/src/sock.rs` | refcount **extract** into the fd model (X7's `akuma-vfs-glue`) — a close sweep should not live in marshal code | yes | none | **X7** |
| readiness probe: `poll_arm`, `select_arm`, `fd_events` | 81-319 | which fd has which event, per lap | fdset algebra **pulled**: `akuma-syscalls-poll` | `amd64/src/fd.rs:1991 sys_poll`, `:2063 sys_select` | **extract** the per-kind probe into X7's fd model (both kernels' `fd_events` are the same function over different OpenFile enums) | yes | none | **X7** |
| socket options: `setsockopt_arm`, `getsockopt_arm` | 320-436 | timeouts real, SO_ERROR, accept-ignored list | no | `amd64/src/sock.rs:638 sys_setsockopt` | **extract** the option algebra (pure: `(level,opt,val)→action`) into `akuma-net` | yes | none | **X6** |
| socket calls: `socket`/`bind`/`listen`/`accept`/`connect`/`sendto`/`recvfrom`/`getsockname`/`getpeername`/`shutdown`/`read`/`write`, `sockaddr_in_from/to_user`, `connect_wait` | 442-908 | sockaddr marshal + the dial/accept wait | sockets **pulled**: `akuma-net` | `amd64/src/sock.rs` (same shapes) | **extract** marshal into `akuma-syscalls-net` (exists: it already owns the ifconf/procfs wire work) — wait loops stay kernel-side (they park) | yes (marshal half) | none | **X6** |
| msghdr: `read_msghdr`, `read_iovec`, `sendmsg_arm`, `recvmsg_arm` | 938-1029 | the 56-byte msghdr + iovec walk | **pulled**: `akuma-syscalls-linux` (msghdr layout exists) | `amd64/src/sock.rs:487-530` (`msghdr_off` module — the same offsets) | **extract** into `akuma-syscalls-net` next to the sockaddr marshal | yes | none | **X6** |

### `exec_twin.rs` (627 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| current-task statics: `CURRENT`, `SHELL_THREAD_TID`, `current_slot`/`set_current_slot` | 45-102 | who am I (the sanctioned current-task static) | hook **pulled**: `akuma-threading` | `amd64/src/exec_runtime.rs` (same, `%gs` ready) | — stays (SMP slice swaps the body, not the callers) | no | none | — |
| clocks: `uptime_us` | 87-103 | the one-clock rule (LAPIC ticks × 10 ms) | **pulled**: `akuma-lapic-amd64::ticks`; `akuma-primitives::clock` exists | `amd64/src/lapic.rs` | **extract**: both kernels read one `akuma-primitives::clock::now()` set at init (see §2, the two-clock seam) | yes | none | **X2** |
| `switch_to` | 104-156 | the five machine effects of a switch | hooks **pulled**: `akuma-threading` | `amd64/src/sched.rs` | — stays (naked glue) | no | keeps 1 (context asm) | — |
| FPU: `FxArea`, `fx_init`, `FX`, `fx_save`, `fx_restore`, `fx_reset` | 157-202 | eager per-slot fxsave/fxrstor | sizes **pulled**: `akuma-fpu-amd64` (FORBID? no — it holds the asm) | `amd64/src/sched.rs` (same pair) | **extract** the save/restore pair into `akuma-fpu-amd64` (`save_into(&mut [u8])`/`restore_from(&[u8])`) | no (asm, but self-contained) | moves 2 into the crate that owns FPU asm | **X10** |
| idle + gates: `IDLE_TID`, `seed_idle`, `tick_window`, `allow_tick`, `idle_slot`, `can_run`, `transfer_lock_depth` | 203-339 | the idle halt and the tick gates | hooks **pulled**: `akuma-threading` | `amd64/src/sched.rs:1387-1426` | — stays | no | none | — |
| runtime/config/hook tables: `thread_runtime`, `thread_config`, `process_hooks`, `exec_runtime`, `exec_config`, `twin_*`, `init` | 283-336, 341-508, 592-627 | the registration face the crates read | **pulled**: `akuma-exec::runtime::register`, `akuma-threading` | `amd64/src/exec_runtime.rs::init` (the twin of the twin) | — stays (this file *is* the target-glue pattern) | no | keeps ~2 (enter-user asm) | — |

### `net.rs` (385 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| TSC clock: `pause`, `rdtsc`, `TSC_PER_US`, `calibrate`, PIT constants, `use_lapic_rate`, `uptime_us` | 43-166 | what a microsecond is (PIT gate, LAPIC fallback — the 2026-10-30 clock fix) | **pulled**: `akuma-lapic-amd64::tsc_hz`; PIT device **pulled**: `akuma-port` | `amd64/src/net.rs`, `lapic.rs` | **extract** into `akuma-primitives::clock` (exists): `init(pit_or_lapic)`, `now_us()` — the PIT-walk itself is x86 glue that stays | yes (rate math) | sheds 1 (`rdtsc` → `akuma-lapic-amd64` already has a safe read) | **X2** |
| entropy: `has_rdrand`, `weak_fill`, `rng_fill` | 167-239 | where getrandom bytes come from | `akuma-primitives::rng` **exists** (src/rng.rs) — check whether it holds RDRAND | `amd64/src/net.rs:139-170` (the same trio) | **pulled+extract**: move the RDRAND/SplitMix64 pair into `akuma-primitives::rng` | yes (SplitMix64 half) | sheds 1 (`rdrand` asm → the crate) | **X2** |
| park/wake: `park_until`, `blocking_relax`, `wait_lap`, `noop_waker` | 240-275 | how a wait laps (sti;hlt;cli + netpoll) | no | `amd64/src/sched.rs` idle | — stays (the pump policy is kernel-owned) | no | none | — |
| netpoll + init: `NET_UP`, `IN_LAP`, `netpoll`, `net_init`, `sleep_until`, `runtime` | 276-385 | the one drained lap per tick; the NetRuntime hooks | **pulled**: `akuma-net` runtime traits | `amd64/src/net.rs` | — stays | no | none | — |

### `pipes.rs` (355 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| table + creation: `PIPES`, `table`, `pipe_arm`, `pipe2_arm`, `socketpair_arm` | 27-188 | pipe/socketpair creation and fd pairs | **pulled**: `akuma-pipes` (`PipeTable`, `Wakes`) + `akuma-fdtable` | `amd64/src/fd.rs` (pipe arms) | — stays (effects + flags are kernel-side by the fdtable rule: the crate computes, this file performs) | yes | none | — |
| transfer: `read_arm`, `write_arm`, `write_arm_gathered`, `write_no_sigpipe` | 189-301 | the read/write contract (EAGAIN, EPIPE, SIGPIPE suppression for the bridge) | **pulled**: `akuma-pipes` ring math | `amd64/src/fd.rs` | gather/SIGPIPE policy **extract** into `akuma-pipes` as pure helpers | yes | none | **X7** |
| lifecycle: `may_park`, `park_once`, `fire`, `clone_ref`, `readable`, `writable`, `release`, `release_open_fds` | 45-84, 302-355 | refcounts, wake fan-out, exit sweep | **pulled**: `akuma-pipes` (`clone_end`/drop) | `amd64/src/fd.rs` | — stays; the *sweep-on-exit* hook is already the crate's `Wakes` shape | yes | none | — |

### `futex.rs` (201 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| the whole syscall: `sys_futex`, `wait`, `wake`, `requeue`, `wake_op`, `queued_key`, `namespace`, `read_u32`/`write_u32`, `Waiter`, `resume`, `wake_key`, `purge_task` | 27-201 | futex val-compare, bitset, requeue, WAIT/WAKE-op algebra, deadlines | algebra **pulled**: `akuma-syscalls-sync` (`WaiterTable`, `op`, `key`, `deadline`) | `amd64/src/futex.rs:126 sys_futex` — a near-1:1 twin | **extract** the syscall body into `akuma-syscalls-sync` behind a 4-method `FutexMemory`+`FutexPark` trait (read_u32, write_u32, park, resume); both kernels keep ~30 lines of impl | **yes** — the trait is mockable, the crate's table is already differentially tested | sheds 0 here; *prevents* the next drift | **X1** |

### `ring.rs` (38 lines)

| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? | `unsafe` taken/shed | order |
|---|---|---|---|---|---|---|---|---|
| `Ring<T, N>` (Copy elements, BorrowCell, overflow-overwrite) | 11-38 | the bounded trace ring three statics use | no | no (amd64 traces differently) | **write new**: a module in `akuma-primitives` (why: it is generic, 38 lines, FORBID-able, and `dispatch`'s diagnostics deserve one home) | yes | sheds 1 | **X6** |

---

## §1 The ten extractions, ranked (lines removed from `taoxichuan/src` + deduplicated against `amd64/src`, divided by risk)

The seam is stated the way `CLAUDE.md` states them: what the crate owns, what
it must NOT depend on, which side keeps the locks and the `unsafe`.

1. **X1 — futex syscall body → `akuma-syscalls-sync`.** The crate owns the
   waiter table, op algebra and deadlines; it must NOT own threads, locks or
   user memory — it sees a `FutexMemory`/`FutexPark` trait, and the kernel
   keeps the `unsafe` uaccess reads and the park call. ~340 lines deduplicated
   (201 here + the twin's share), lowest risk in the list: the crate is
   already differentially tested and both call sites are shaped alike today.
2. **X2 — clock + entropy → `akuma-primitives` (`clock`, `rng`).** The
   primitives crate owns "what time is it" and "where random comes from"; it
   must NOT own devices — the PIT walk stays x86 glue and hands the crate a
   rate. ~200 lines deduplicated (`net.rs` 43-239, amd64's `net.rs` twins);
   kills the two-clock seam (§2); risk low, evidence is the boot banner.
3. **X3 — user access → `akuma-user-access`.** The crate owns the
   region-check + copy contract (`user_ok`, `copy_from_user`/`to_user`,
   `read_ptr_array`, the E2BIG caps); it must NOT own page tables — it sees a
   `CopyPage`/`Walk` hook, and the kernel keeps the physmap `unsafe` and the
   `cow_prepare_write` call. ~700 lines deduplicated (this kernel's 90 +
   amd64's `uaccess.rs` 618 + `child.rs` 326-440), medium risk because every
   syscall passes through it — gated by the full battery, twice.
4. **X5 — signal/eventfd/membarrier arms → `akuma-syscalls-glue`.** The glue
   crate owns marshal-and-reply for the small syscall families; it must NOT
   own task state — masks and altstack records stay in the kernel's `Task`,
   read and written through parameters. ~250 lines deduplicated
   (`signal.rs` marshal half, `usermode.rs` eventfd table), low risk (the
   algebra is already crate-side after the 2026-10-30 `procmask` fix).
5. **X6 — socket marshal → `akuma-syscalls-net`; trace ring →
   `akuma-primitives`.** The net-abI crate owns sockaddr/msghdr/sockopt wire
   decisions; it must NOT own sockets, fds or waits — `akuma-net` keeps the
   tables and locks, the kernel keeps the parking `connect_wait`. ~600 lines
   deduplicated (`sock.rs` marshal half + amd64's `sock.rs` msghdr module);
   risk medium-low, gated by the stage-6/7 network gates.
6. **X4 — page walk + CoW effects → `akuma-pte-amd64` + `akuma-cow`.** The
   pte crate owns the x86-64 leaf walk and bit pokes; `akuma-cow` owns the
   break/prepare-write/privatize policy over a `PteIo` trait; neither may own
   CR3 or the fault path — the kernel keeps `fault_trap` and the physmap
   `unsafe`. ~300 lines deduplicated (`mm.rs`), medium risk; gated by
   `cow_leak_gate.py` delta 0.
7. **X10 — thread/root lifetime → `akuma-exec` (+ FPU pair →
   `akuma-fpu-amd64`).** The exec crate owns the retire-on-exit/drain-at-entry
   policy and the parked-root sweep over its process table; it must NOT own
   stacks or CR3 — the twin's hooks keep the machine effects. ~150 lines
   deduplicated, low risk, and it closes the "Process table full" bug class by
   making the policy shared instead of re-typed.
8. **X9 — slab/hole manager → `akuma-mmap`.** The mmap crate owns placement
   (holes, brk/mmap cursors, region lists); it must NOT own PTEs — unmap
   callbacks return the pages to the kernel. ~250 lines deduplicated
   (`mm.rs`'s hole list), medium risk; gated by `memprobe` and the in-guest
   build's memory peak.
9. **X8 — image placement → `akuma-elf-exec` (switch-to-crates S5).** The
   elf-exec crate owns segment placement and the interpreter decision; it must
   NOT own the stack build (`akuma-ring3-amd64` does) or CR3. ~280 lines
   removed here, medium risk; gated by hello, `apk add curl`, and `abiprobe`.
10. **X7 — the file-description model → `akuma-vfs-glue`.** The glue crate
    owns the OpenFile/session/stat/getdents/poll-probe model over a `FileIo`
    trait; it must NOT own ext2, sockets or pipes (their crates keep their
    own), and `akuma-fdtable` stays the only lowest-free/close/dupdecider. The
    biggest prize (~1,500 lines deduplicated against `fd.rs`) and the biggest
    risk — last, after X3 made the copies crate-shaped. Gated by the full
    battery byte-identical stage 3 plus `ext2probe`.

## §2 Seams in the wrong place today

1. **The task table is a `static mut` handed out as `&'static mut`**
   (`main.rs:1061` `TASKS`, `:1445` `task_slots()`, `:1455` `task()`;
   `child.rs:246` `slots()` over `SLOTS`, `:1590` `EXEC_SAVE`). Three long-lived
   `&mut` aliasing dishes; the SAFETY comment on `task()` is the single-core,
   IF-off discipline stated in prose. Which extraction removes them: the task
   record moves onto `akuma-slot-table::SlotTable`/`BorrowCell` closures as
   part of **X10** (and §2 of switch-to-crates S3 deletes `slots()` with the
   ring). What a `BorrowCell`-closure version costs: mechanically, every
   `task().x` becomes a closure — but `dispatch` opens **one**
   `tasks.with_mut(current_slot, |t| …)` scope at the top of
   `dispatch_inner`, so the ~200 in-dispatch call sites collapse to one; the
   real costs are (a) the naked crossings (`child_enter`/`exit_cross`,
   `switch_to`) cannot hold closures and keep their sanctioned raw-pointer
   stashes inside `exec_twin`, and (b) every *crossing-adjacent* helper that
   stashes a `&mut Task` across a park point (the wait4 rsp save/restore,
   `bind_child_task`) must re-open the cell — the compiler enforces what the
   SAFETY comment hopes. `EXEC_SAVE` is really a per-slot record (one live
   crossing today by discipline, not by type); it dies at S3, but until then
   it belongs in `SLOT_STATE`'s per-slot array, not a single static.
2. **Two clocks by composition accident** (`net::uptime_us`, TSC-based, vs
   `exec_twin::uptime_us`, LAPIC-tick-based): the "what time is it" decision
   is welded to whichever file needed it; `scheduling.md`'s one-clock rule is
   honored for the twin tables and knowingly broken for the syscall-answer
   clock. **X2** moves both callers onto one `akuma-primitives::clock` set at
   init; the PIT/LAPIC *choice* stays x86 glue.
3. **Socket refcounts welded to marshal code** (`sock.rs:924 socket_refs`,
   called from the exit sweep through the marshal file): the last-ref close
   decision belongs to the fd model (X7), not to the file that happens to
   marshal `sendto`.
4. **`register_shell_process` re-types amd64's `register_exec_process` inside
   a crossings file** (`child.rs:991-1069`): process-table registration
   policy belongs in `akuma-exec` next to `register_exec_process` (X10);
   today a fix to one is silently absent from the other.

## §3 Gates per extraction

| extraction | proves behaviour unchanged | A/B to run (next executor session — the metal VM is busy today) |
|---|---|---|
| X1 futex | `cargo test -p akuma-syscalls-sync` (existing differential tests + new syscall-body tests over a mock `FutexMemory`); in-guest `userspace/futexprobe`; `scripts/kvm/goose/run_gates.sh` M-gates | boot old kernel vs new, diff the `futexprobe` transcript line-by-line |
| X2 clock/rng | boot line `calibrated <N> cyc/us` and the getrandom source line identical; `sleep 20` measures 20 s; `ncaprobe` | same image booted twice, once with the PIT present (TCG) once without (KVM): both must take the LAPIC rate |
| X3 uaccess | full `run_gates.sh` battery twice; `memprobe`; `abiprobe`; `cow_leak_gate.py` delta 0 | in-guest `md5sum` of a fixed binary pulled out over ssh, before vs after |
| X4 pte/cow | `cow_leak_gate.py` (20 rounds, `cowrefs=0`, delta 0); `shareprobe`; W^X probes (JIT refused, `wx=0` widens) | boot with `wx=0` and `wx=1`, diff the fault transcripts |
| X5 signal/eventfd | `run_gates.sh`; `forkprobe`; the in-guest `-j4` build (cargo's jobserver rides `tkill`/EINTR) | the 200-variable `env` E2BIG probe from `TAOXICHUAN_BUGS_FOUND_2026_10_30.md` §1, before vs after |
| X6 socket marshal | stage-6/7 gates (`apk add curl`, `curl -sS https://example.com`); `ncaprobe` | curl transcript diff, old vs new, on the same URL set |
| X7 fd model | battery stage 3 **byte-identical** `ls` output; `ext2probe`; the ash redirect/save-restore dance gates | `ssh ls -l /var` md5 diff; `poll`-heavy sshd session before/after |
| X8 elf-exec | hello gate, `apk add curl`, `tcc` compile+run (F1), `abiprobe` | `readelf`-equivalent layout dump of the same hello binary, before vs after |
| X9 mmap holes | `memprobe`; the in-guest kernel build's peak-memory figure must not regress past the 2026-10-30 baseline | same build, same commit, `-j4`: peak RSS table from both boots |
| X10 exec/teardown | the in-guest `-j4` build's `[reap] procs=` plateau (peak ~30, never `Process table full`); `forkprobe` | a spawn-churn probe (30 forks in one boot) that used to hit the 15-spawn stack limit |

## §4 Unsafe-site counts (`grep -c unsafe taoxichuan/src/*.rs`, word counts)

Now: **main.rs 28, child.rs 26, exec_twin.rs 5, net.rs 2, ring.rs 1,
futex.rs 0, pipes.rs 0, sock.rs 0 — total 62.** After each extraction
(cumulative, estimated — the count moves only when the `unsafe` word moves
into a crate):

| after | main.rs | child.rs | exec_twin.rs | net.rs | ring.rs | total |
|---|---|---|---|---|---|---|
| X1 futex | 28 | 26 | 5 | 2 | 1 | 62 |
| X2 clock/rng | 28 | 26 | 5 | 0 | 1 | 60 |
| X3 uaccess | 22 | 26 | 5 | 0 | 1 | 54 |
| X5 signal/eventfd | 22 | 26 | 5 | 0 | 1 | 54 |
| X6 sockets+ring | 22 | 26 | 5 | 0 | 0 | 53 |
| X4 pte/cow | 15 | 26 | 5 | 0 | 0 | 46 |
| X10 exec/FPU | 15 | 25 | 3 | 0 | 0 | 43 |
| X9 mmap holes | 12 | 25 | 3 | 0 | 0 | 40 |
| X8 elf-exec | 7 | 25 | 3 | 0 | 0 | 35 |
| S2/S3 (switch-to-crates) | 7 | ~8 | 3 | 0 | 0 | ~18 |

The ~18 residue is the honest floor for this architecture: trap entry, GDT/IDT
wiring, LAPIC bring-up, the naked crossings (`switch_to`, `child_enter`,
`exit_cross`, `twin_enter_user`), the FPU pair, and the physmap copies the
uaccess hook hides. It matches the shape of `amd64/src/`, whose per-file
counts the same extraction logic produced there.

## Background

[`switch-to-crates.md`](../reference/switch-to-crates.md) (the process/
scheduling switch this extends), [`docs/reference/crate-safety.md`]
(../../../docs/reference/crate-safety.md) (which crates forbid `unsafe`),
`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md` (the incidents that fixed
E2BIG, the clock, the CoW write holes and the thread-exit leak — every fix
cited above landed 2026-10-30), `docs/archive/TAOXICHUAN_FIXED_POINT.md`
(the K1 == K2 result these extractions must not disturb),
`CLAUDE.md` § Layout (the composition rules applied above).

## Status 2026-10-31 (extracted and verified on metal)

| step | done | commit |
|---|---|---|
| X2 clock/rng | `splitmix_fill`/`fill_from_words` in `akuma-primitives::rng` | earlier |
| X1 futex | `akuma_syscalls_sync::body::Kernel` + `sys_futex`/`wake`/`requeue`/`wake_op`/`wake_key`; amd64 (`Amd64`) and Taoxichuan (`Tx`) implement the trait once, keep the table lock, user reads, waker and their own park loops; +3 host tests (45 pass). `taoxichuan/src/futex.rs` 201 -> 160 lines | `ee6c93c5` |
| init (idle threads) | `akuma_sched_amd64::mark_idle`: daemon, idle, pinned, no space, no trap stack - used by amd64's `register_idle_task` and Taoxichuan's AP/BSP idle adoption. The claim/adopt calls stay in the kernels (the crate has no `akuma-threading` dependency, on purpose) | `aafa308a`, `e7661570` |

| X3 user access | new `akuma-user-walk` (`forbid(unsafe)`, no deps, 5 host tests): `in_regions` (the `user_ok` rule), `copy_from`/`copy_to` (page chunking), `read_cstr` (page-chunked, stops at NUL, so a string ending before a hole succeeds; `user_str` and `read_user_string` were byte-at-a-time), `read_ptr_array`. Taoxichuan keeps `TaskWalk: Walk` (physmap frame access + `cow_prepare_write`). The existing `akuma-user-access` is the SMAP/fault-recovery copy asm and stays amd64/aarch64's; the two share a range rule, not a copy | `57cda68d` |

Not started: X4-X10. The nested-crossing retirement (S3) is still open.

Verification on the ryzen (bare metal, Akuma host entry 29 = "KVM herd", `smp=8`), host kernel `e7661570` built on the ryzen:
* host boots, 7 APs online, wifi + sshd up;
* a clean `kbuild -c -j 4 -p taoxichuan` (rustc + cargo jobserver, the futex-heaviest thing the box does) three times: EXIT=0, 27-30 s, the same artifact md5 `933fe65d3252e5460e2eafaed06f72b5` each time;
* Taoxichuan guest (the same commit) as a 4-vCPU `kvm0` guest: `smp: 4 cpus online`, ssh round trip OK, no `[Fault]`/`PANIC`.

Honest limits: the guest's `/bin/sh` does not spawn on this metal image **with the pre-session guest kernel too** (`sh -c "echo hi"` over ssh: 0/10 on both), so most of `run_gates.sh` is not a usable oracle there (it passes/fails on the shell fallback); that is an environment gap to chase separately. The guest's own futex path is exercised only by sshd and `apk.static --version` (G2 passes on the new kernel) - the in-guest `-j4` build (Linux KVM on the ryzen, `build_run.py`) was not re-run for X1.

Metal re-check after X3 (host `e7661570` unchanged, guest `57cda68d`, 4 vCPUs, `smp: 4 cpus online`): `run_gates.sh` G1-G4, H1-H3c, F1, C1, D1, D2, A1-A3, M4curl, M4web, W1b-d PASS, no `[Fault]`/`PANIC`, host survived the whole run. W1a FAILs on every run since the patched image (it hashes `ls -lin /etc/hostname`, whose inode/mtime depend on the image, not on the kernel).
