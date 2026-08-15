# Boot tests that could be host tests — audit

**Date:** 2026-08-13. **Grade: B.** Two different levels of evidence here, and the
difference matters:

- **Read directly** — every entry in §3 (the vacuous and mirror lists), §4 (dead
  tests), and the definition/registration counts in §1. Each of those tests was
  opened and read.
- **Classified from dependency signals** — the per-class counts in §2, derived
  from what each test references, with ~60 of them also read individually as a
  check. Treat those counts as ±10, not exact.

Four of the highest-consequence claims were re-checked against the tree
independently of the pass that produced them (§7).

The question: which of the kernel's boot self-tests could become `cargo test` host
tests, and what would each require? The premise going in was that
`akuma-exec`'s stub-runtime injection (`test_support::ensure_test_runtime`,
`runtime::register_config_for_test`) unlocks a large number "for free".

**The premise was half right, and the audit's most important finding is unrelated
to movability: 38 of the 553 registered boot tests cannot fail.** 25 of those
assert facts about local literals with no production symbol in scope; 13 are
explicit copies of production logic living in the test file. Those are deletions
and rewrites, not relocations.

---

## 1. Enumeration

**Method.** Every top-level `fn` in the nine boot-test modules
(`src/{process_tests,tests,rump_tests,async_tests,daif_tests,fs_tests,network_tests,pthread_tests,sync_tests}.rs`)
matched against the call sites inside its registration function. Counted exactly:
definitions, registrations, and the §3 lists.

**Registered, live, pass/fail boot tests: 553.**

| suite | registered | notes |
|---|---:|---|
| `src/process_tests.rs::run_all_tests` (`:115`–`:819`) | 286 | + 2 sub-suite calls; 6 registrations commented out |
| `src/process_tests.rs::run_network_tests` (`:13`) | 5 | |
| `src/syscall/net.rs::run_net_bounce_tests` (`:1153`) | 1 | one `[PASS]` line |
| `src/tests.rs::run_memory_tests` (`:22`) | 156 | 6 commented out |
| `src/tests.rs::run_threading_tests` (`:320`) | 17 | |
| `src/sync_tests.rs` | 52 | |
| `src/pthread_tests.rs` | 18 | |
| `src/rump_tests.rs` | 6 | `#[cfg(feature = "rump")]` |
| `src/daif_tests.rs` | 6 | |
| `src/fs_tests.rs` | 6 | |
| `src/network_tests.rs` | 1 | |
| benchmarks (not pass/fail) | 6 | `run_cow_benchmarks` ×3, `run_benchmarks` ×3 |

`process_tests.rs` defines 309 top-level fns and `tests.rs` 212; the gap between
definitions and registrations is helpers plus the dead tests in §4.

## 2. Classification

| class | meaning | count |
|---|---|---:|
| **A** | subject already in `crates/`; pure or config-only. **No production code moves** | 57 by signal / **~25 after reading them** |
| **B** | free with the existing `ensure_test_runtime()` stubs | 65 |
| **C** | needs a *working* fake — an arena-backed `alloc_page_zeroed` | 54 |
| **D** | needs production code moved into a crate first | 123 |
| **E** | genuinely boot-only (a thread must stop/resume, real MMU/TTBR, device MMIO, SMP timing) | 216 |
| **F** | **vacuous — tests nothing.** A class the audit did not set out to find | 38 |

**Class A is polluted by the signal-based pass.** ~30 of it is `src/tests.rs`'s
`Vec`/`String`/`Box`/`realloc` suite (`:502`–`:1592`, `:2737`–`:2838`), whose real
subject is the *kernel* global allocator and boot heap. Running those on the host
would test the host allocator instead — they are class E, not movers. Reading the
bucket leaves **~25**.

## 3. Tests that cannot fail

The headline finding. Two kinds; every test listed below was read directly, not
inferred from signals.

### 3a. Explicit mirrors — a copy of production logic, in the test file (13)

Same pattern as the `fork_code_start` mirror already fixed in
[`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
§5.11: the test exercises its own copy, so production can drift freely.

| test | file:line | mirrors |
|---|---|---|
| `test_encode_wait_status_sigkill_vs_sigterm` | `process_tests.rs:12393` | local `fn encode` ≡ `syscall/proc.rs:79` |
| `test_sys_kill_delivers_signal_not_hardkill` | `process_tests.rs:12417` | same `fn encode`, 3rd copy |
| `test_kill_process_exit_code_uses_negative_signal` | `process_tests.rs:12457` | same `fn encode`, 4th copy |
| `test_sigchld_not_fatal_by_default` | `process_tests.rs:10159` | local `signal_is_fatal_default` ≡ `syscall/signal.rs:85`; comment admits "we mirror that table inline" |
| `test_clone_flags_routing` | `process_tests.rs:11911` | local `fn route` — "mirrors `sys_clone_pidfd`'s routing logic" |
| `test_signal_mask_nodefer_blocks` | `process_tests.rs:7154` | "Mirror the kernel logic from `try_deliver_signal`" |
| `test_signal_mask_nodefer_flag_skips` | `process_tests.rs:7182` | same |
| `test_spsr_el0t_bits` | `process_tests.rs:13883` | sigreturn's SPSR validator, inline |
| `test_sigreturn_validates_spsr` | `process_tests.rs:13838` | same validator, inline |
| `test_sigreturn_validates_sp` | `process_tests.rs:13860` | SP validator, inline |
| `test_stack_top_within_48bit_va`, `_small_static`, `_go_sized_static`, `_boundary_512k` | `tests.rs:6594`, `:6696`, `:6725`, `:6764` | `compute_stack_top` (`elf/stack.rs:200`) re-typed with its constants. `:6696` reduces to `let r = if brk < T { D } else { 0 }; ok = r == D` |
| `test_eager_munmap_prefix_preserves_suffix`, `_suffix_preserves_prefix`, `_full_removes_all` | `tests.rs:4322`, `:4373`, `:4415` | shared helper `simulate_sys_munmap_eager` (`tests.rs:4273`), doc'd as "replicates the FIXED `sys_munmap` eager-region logic" |
| `test_ext2_orphaned_lock_recovery` | `tests.rs:9232` | local `is_thread_dead` + "Simulate the orphan recovery logic (from `ext2.rs try_read_state`)" |
| `test_block_on_noop_waker` | `tests.rs:2650` | local `block_on_limited` ≡ production `block_on` |

### 3b. Vacuous — no production symbol in scope at all (25)

Every one prints `PASSED` unconditionally. Verbatim examples:

- `process_tests.rs:11666` `test_nanosleep_returns_eintr_on_interrupt` —
  `let eintr = (-4i64) as u64; let expected_eintr = (-4i64) as u64; if eintr == expected_eintr`
- `process_tests.rs:11855` `test_fork_thread_pid_map_invariant` —
  `let map_has_child_entry = true; let resolved_pid = if map_has_child_entry { child_pid } else { parent_pid }; if resolved_pid == child_pid`
- `process_tests.rs:11689` `test_futex_wake_unmapped_returns_zero` — `wake_cmds.iter().all(|_| true)`
- `process_tests.rs:12224` `test_from_elf_default_cwd` — `let default_cwd = "/"; if default_cwd == "/"`
- `process_tests.rs:13914` `test_replace_image_preserves_pid` — `let resolved_pid = child_pid; resolved_pid == child_pid`
- `process_tests.rs:11966` `test_clone_thread_rejects_zero_stack` — `let stack = 0; let rejected = stack == 0;`
- `process_tests.rs:13936` `test_deactivate_does_not_free_shared_frames` — `size_of::<PhysFrame>() == 8`
- `tests.rs:7025` `test_direntry_has_is_symlink_field` — builds `DirEntry { is_symlink: true }`, asserts `.is_symlink`

Also `process_tests.rs:13962`, `:13986`, `:14005`, `:14057`, `:14076`, `:14471`,
`:14494`, `:14518` (all `let a = true; let b = true; if a && b`),
`:11881`, `:12001`, `:12028`, `:13762` (assert two's-complement, not the
`sys_clone_pidfd` high-bits guard they claim to cover), `:12131`, `:12237`,
`:12333`, `:13787`, `:13811`.

These cost boot time and dilute the `PASSED` count. **They are deletions** — there
is nothing to relocate. Several name real invariants worth covering; those should
be rewritten against production, not moved.

## 4. Dead tests

| what | where | status |
|---|---|---|
| `akuma_exec::kernel_tests::run_all_tests` + its **7** tests | `crates/akuma-exec/src/kernel_tests.rs:47`, `:59`–`:121` | **Zero callers anywhere** (re-verified). `#[cfg(target_os="none")]` so it compiles into the kernel and never runs. All 7 are trivially host-testable — `AsidAllocator` already has host tests at `mmu/asid.rs:46` |
| `test_alloc_mmap_resolves_tgid` registered **twice** | `process_tests.rs:425` and `:426` | Re-verified; a dupe scan of `run_all_tests` shows it is the **only** duplicate |
| `test_term_state_lock_bounded_acquire_does_not_starve_peers` | `process_tests.rs:2142` | Registration commented out at `:213` — "the test harness itself has an unresolved synchronization bug" |
| 5 msgqueue waker tests | `process_tests.rs:13401`, `:13460`, `:13516`, `:13576`, `:13604` | Commented out at `:603`–`:607` — "manipulate real thread slots which causes scheduler crashes" |
| 6 allocation-pattern tests | `tests.rs:1635`, `:1678`, `:1721`, `:1780`, `:1858`, `:1900` | Commented out at `:230`–`:235` — "hang during preemption" |
| `_removed_single_slot_test` | `process_tests.rs:14357` | Vestigial stub, no callers |
| `tests::run_all()` | `tests.rs:490` | No callers — `main.rs` calls the three sub-suites separately |
| `async_tests::run_all` | `async_tests.rs:5` | Called from `main.rs:992`, but the body is `print("Skipping…"); true` — 0 tests |

## 5. Scaffolding, by tests unlocked per unit of work

1. **An arena-backed `alloc_page_zeroed`/`free_page`** — unlocks ~54 class-C
   directly, and **105 registered tests call `make_test_process`**, which dies
   today because `ensure_test_runtime`'s `alloc_page_zeroed: || None` makes
   `UserAddressSpace::new()` (`mmu/mod.rs:588`) return `None`. Feasible because
   `phys_to_virt` is `#[inline(always)] paddr as *mut u8` — the identity — so real
   host-heap addresses serve as `PhysFrame.addr`. Pair with
   `mmu::init(arena_base, arena_len)`, whose identity-map side effect is
   `#[cfg(target_os="none")]` (`mmu/mod.rs:29`), so on the host it only stores the
   RAM-window atomics. Then move `make_test_process` (`process_tests.rs:5685`) into
   `akuma-exec` as a `#[cfg(test)]` fixture.
   **See [`PMM_EXTRACT.md`](PMM_EXTRACT.md) §6 — if the PMM becomes a crate you do
   not build this fake at all; you run the real allocator over a host arena.**
2. **Move `src/syscall/pipe.rs` (425 lines) into `akuma-exec`** — ~30 class-D
   tests, the best tests-per-line-moved ratio in the tree. One
   `Spinlock<BTreeMap<u32, KernelPipe>>` plus waker calls the existing stubs cover.
3. **A home for the pure arithmetic still in `src/`** — one module absorbs
   `encode_wait_status` (`syscall/proc.rs:79`), `signal_is_fatal_default`
   (`syscall/signal.rs:85`), `heap_grow_backoff` + `heap_grow_initial_pages`
   (`allocator.rs:175`), `net_bounce_size_plan` (`syscall/net.rs:51`),
   `epoll_wait_deadline_for_test` (`syscall/mod.rs:79`), `membarrier_cmd`
   (`syscall/mem.rs:885`), `user_va_limit_value` (`syscall/mod.rs:462`),
   `compute_heap_size` / `reserve_calc_ram` / `compute_memory_layout` /
   `compute_thread_limit` (`main.rs`), and the `exceptions.rs` decoders
   (`is_aarch64_svc:2916`, `far_in_kernel_identity_user_range:779`,
   `syscall_is_non_restartable:1284`, the STP-XZR decode). ~20 tests.
4. **Zero-scaffolding relocations — do these first.** The `ProcessMemory` cluster
   (11 tests; `types.rs:521/550/604` reads no runtime and no config, only
   `mmu::kernel_va_end()`'s documented fallback), `syscall_name` (1),
   `SharedFdTable::arc_clone` (1), and the four `compute_stack_top` mirrors
   rewritten against the real fn. **~17 tests, no production motion at all.**
5. **Extract `sys_munmap`'s eager-region split** as a pure fn — retires
   `simulate_sys_munmap_eager` and its 3 replica-driven tests.
6. **Delete, don't move** — §3b's 25 vacuous tests and §4's dead entries.
7. **`ensure_test_runtime()` sweep** — the pending-signal / signal-mask /
   wake-handle family (~15 class-B tests). Nothing to build; the inert
   `futex_wake`/`trigger_sgi`/`wake_core` stubs suffice, because nothing has to
   actually resume (§6.1's line).

## 6. Where the premise was wrong

- **`ExecRuntime` has 46 `fn` pointers and `ExecConfig` has 27 fields** — the
  premise had those numbers swapped and both wrong. Re-verified. The
  `register_config_for_test` doc comment (`runtime.rs:249`) also still says "27
  kernel function pointers"; it drifted with the struct.
- **`run_all_tests` spans `process_tests.rs:115`–`:819`**, not "around 400–520".
  Anchoring there misses ~180 registrations, including the whole SMP block
  (`:130`–`:205`) and the BKL/VFS/mm-guard block (`:700`–`:818`).
- **Host tests are not only in `crates/*`** — `userspace/` has three suites
  (`sshd`'s `wire.rs`, `box`'s `boxlib`, `akuma-ssh-crypto`). And
  `crates/akuma-exec/src/kernel_tests.rs` is a *boot*-test module living inside a
  crate, so "src/ = boot, crates/ = host" is not the real boundary.
- **The stub runtime unlocks the cheapest set, not the largest.** Class B (stubs
  suffice) is 65; class C (needs the arena) is 54 and class D is 123. The real
  bottleneck is one line — `alloc_page_zeroed: || None` — gating 105 tests. A ~40
  line arena unlocks about five times what the stubs do.
- **A class nobody was looking for.** The 25 vacuous tests (§3b) outnumber the 13
  explicit mirrors the audit was hunting.

## 7. What was independently re-verified

The audit was produced by a subagent; these claims were re-checked against the
tree before being written down, because the document will be cited:

| claim | result |
|---|---|
| `kernel_tests` has zero callers | **confirmed** — no references outside the file |
| `ExecRuntime` 46 fn ptrs / `ExecConfig` 27 fields | **confirmed** by field count |
| `test_nanosleep_returns_eintr_on_interrupt` is vacuous | **confirmed** verbatim |
| duplicate registration of `test_alloc_mmap_resolves_tgid` | **confirmed, line numbers corrected** to `:425`/`:426` (reported as `:429`/`:430`); an independent dupe scan of the whole registration function found it is the only one |

## Background

- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  §6.1 (the injection principle and why "needs a scheduler" was too wide a line),
  §5.9 (host-test coverage by crate), §5.11 (the first mirror found and fixed)
- [`PMM_EXTRACT.md`](PMM_EXTRACT.md) §6 — why extracting the PMM subsumes §5's
  arena scaffolding
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
  — the gate, and the two boot-log marker formats (`[PASS]` vs
  `[Test] … PASSED`) that make deleting a boot test easy to mis-measure
