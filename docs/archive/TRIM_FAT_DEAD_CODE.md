# Test-only dead code: candidates for deletion (2026-08-11)

Scope: functions defined in `src/` and `crates/` whose *only* callers are test
code. Different question from
[`DEAD_CODE_SWEEP_FINDINGS.md`](DEAD_CODE_SWEEP_FINDINGS.md) (which measured
"never called at all" via `--force-warn dead_code`) — everything here **is**
called, just only from a test. That sweep flagged its own blind spot for this
exact case:

> Sweep coverage limit: `pub` items in `crates/` are exempt from `dead_code`
> (rustc assumes public API is used), so the crates are under-reported.

This doc closes that gap for `crates/`, and adds `src/` for completeness.
Read as-is at commit `1f9ed7a` (branch `more-devbox-fixes`); nothing here was
edited or booted to confirm.

## Resolution (2026-08-11)

Everything below this point is the original analysis, unedited. Addendum only —
here's what actually happened to each Section A candidate, added after the
fact rather than folded into the findings above:

- **`akuma-net/src/stats.rs`** — deleted (whole module, plus `pub mod stats;`
  in `lib.rs` and the `stats_tests` module in `tests.rs`). Confirmed no
  production caller anywhere in the tree before removing.
- **`akuma-rump/src/syscall_translation.rs::FdMap`** — deleted (struct, impl,
  its `#[cfg(test)]` case, and the now-unused `BTreeMap` import).
- **`akuma-net/src/dns.rs::is_loopback`** — deleted. Trimmed the two tests
  that only exercised it (`loopback_detection`, `loopback_edge_cases`) from
  `dns_tests`; kept `dns_error_messages`.
- **`akuma-net/src/locks.rs::network_lock_holder` / `socket_table_lock_holder`**,
  and **`get_lock_stats` / `reset_lock_stats` + the orphaned `lock_tests.rs`**
  — **no change, and the "orphaned" framing above is now stale.** Commit
  `a4b35ba` ("enabled back some tests that used to be disabled"), which landed
  *after* this doc's `1f9ed7a` snapshot, added `#[cfg(test)] mod lock_tests;`
  to `akuma-net/src/lib.rs`. `lock_tests.rs` is compiled and its five tests run
  now, and they call both holder accessors directly — so neither the file nor
  the accessors are dead by this doc's own test-only-caller definition
  anymore. (Caught this the hard way: almost deleted `lock_tests.rs` per this
  doc's stale claim before `git log -p` on `lib.rs` turned up `a4b35ba`.)
  **Resolved 2026-08-30: the whole module is deleted** — `locks.rs` *and*
  `lock_tests.rs`, 504 lines. The near-miss above was real but is a different
  question: `lock_tests.rs` alone should not have gone, because it covered code
  that still existed. Removing *both* together does not hit that trap, and the
  "tests keep it alive" argument is circular once the tests are the only
  consumer. The open decision in §"doc comment describes a caller that doesn't
  exist" below is settled the second way it offers — delete, don't wire up —
  because the machinery could not have worked: see
  [`REDIS_ROUND_TRIP_STAGE_TRACE.md`](REDIS_ROUND_TRIP_STAGE_TRACE.md) §2.
- **`akuma-vfs/src/memfs.rs::with_max_size`** — kept, deliberately not deleted.
  Unlike the items above, this isn't unwired scaffolding for an abandoned
  path — it's a correct constructor with real test coverage
  (`max_size_enforcement`, `stats`), just not yet called from a production
  `MemFs` construction site. Deleting it would only cost real coverage for no
  maintenance win.
- **`akuma-exec/src/elf/types.rs::parse_elf64_ehdr`** — deleted. Its twin
  `parse_elf64_ehdr_checked` (`elf/mod.rs`) was made `pub(crate)` and the
  three tests repointed at it (`Option`/`is_none()` assertions rewritten as
  `Result`/`is_err()`).
- **`akuma-exec/src/box_mod/access.rs::cascade_kill_order`** — wired in, not
  deleted. Box nesting turned out to be a real, live feature — exercised by
  `src/process_tests.rs`'s `BOX_NESTED` case (`#[cfg(feature =
  "sc-containers")]`, on by default) — so the gap this doc flagged was real:
  `process::kill_box` now snapshots the registry and calls
  `box_access::cascade_kill_order` to kill descendant boxes leaf-to-root
  before unregistering, instead of leaving nested `BoxInfo` entries orphaned
  pointing at a dead parent.

Section B was left untouched, per this doc's own recommendation.

One more finding this doc's scan missed entirely, caught later by a stray
`cargo test` warning (`unexpected cfg condition value: kernel-tls`):
**`akuma-net/src/tests.rs`'s `tls_tests` and `tls_verifier_tests`** modules
were gated `#[cfg(all(test, feature = "kernel-tls"))]` and imported
`crate::tls::TlsOptions` / `crate::tls_verifier::matches_hostname` — but
commit `bade6ab` ("remove unnecessary profiles and all crypto"), well before
even this doc's `1f9ed7a` snapshot, deleted the `kernel-tls` feature and both
of those modules from `akuma-net` (`docs/archive/BUILTIN_SSH_REMOVAL.md`).
The cfg was permanently false and the imports would not have resolved even if
it weren't — 100% dead, never compiled once since `bade6ab`. Deleted both
modules. The original scan's method (grep for a function's callers) can't see
this class of dead code at all, since the modules reference names that don't
exist rather than names with no callers.

## Method

A custom scan (not `cargo check`, which can't see this — see "Method
limitations" below): parse every `fn` definition in `src/**/*.rs` and
`crates/*/src/**/*.rs`, then grep the whole tree for other occurrences of each
name, classifying each occurrence as test or non-test by:

- file name (`*_tests.rs`, `tests.rs`, anything under a `tests/` dir), or
- lying inside a `#[cfg(test)]` `mod { … }` block in the same file (tracked by
  brace-depth from the attribute).

A definition is a candidate if every occurrence found is test-classified.
Every candidate below was then read in context by hand — the raw scan also
produced false positives, kept here as a record of what doesn't hold up:

- **`crates/akuma-exec/src/bkl_model.rs`** (`explore`, `deadlocked_states`,
  `max_wait`) — the scan doesn't see that the *whole module* is declared
  `#[cfg(test)] mod bkl_model;` in `lib.rs`. Everything in it is already
  test-only by construction (a host-side BKL model checker); nothing to trim.
- **`crates/akuma-exec/src/sync.rs`** (`try_lock_shared`, `unlock_shared`,
  `lock_exclusive`, `unlock_exclusive`) — trait method impls of
  `lock_api::RawRwLock`. Called by the `lock_api` crate's generated
  `RwLock`/`RwLockReadGuard` machinery, which grep can't see because the call
  site is in a dependency, not this tree.

Two axes distinguish the real findings:

- **Where the test lives.** `src/{tests,process_tests,sync_tests,pthread_tests,network_tests}.rs`
  are gated `#[cfg(kernel_tests)]`, which is *on by default* — this is the
  in-kernel boot self-test suite (`main.rs` calls `tests::run_memory_tests()`
  etc. every boot). Code reachable only from there **is compiled into the
  release kernel and does run**, just never from a production code path.
  `crates/*/src/tests.rs` and `lock_tests.rs`-style files, by contrast, are
  `#[cfg(test)]` — compiled only under `cargo test` on the host, never linked
  into `akuma.bin` at all.
- **Whether it looks intentional.** Several items below read as scaffolding
  for a feature that was never finished wiring up, in the same shape as
  `DEAD_CODE_SWEEP_FINDINGS.md` §1 (`cleanup_box_queues`). Those are called
  out individually — the fix there may be "wire it in," not "delete it."

## Section A — dead outside `cargo test` (delete candidates)

These never compile into the kernel at all; nothing links them except a host
unit test. Safe to delete outright unless noted.

### `crates/akuma-net/src/stats.rs` — whole module, nothing increments it

```rust
pub fn increment_connections() { NET_STATS.lock().connections += 1; }
pub fn add_bytes_rx(bytes: u64) { ... }
pub fn add_bytes_tx(bytes: u64) { ... }
pub fn get_stats() -> (u64, u64, u64) { ... }
```

`grep -rn 'increment_connections|add_bytes_rx|add_bytes_tx|get_stats' crates/akuma-net/src src`
outside `tests.rs` returns nothing but the definitions. `NET_STATS` is a real
counter (`Spinlock<NetStats>`) that nothing in the async net stack ever
touches — it looks like the intended per-connection accounting was never
threaded through `async_net`, or was replaced by whatever backs
`crates/akuma-net/src/smoltcp_net.rs`'s own counters (`poll_count`,
`tx_drop_count`) and just left behind. Delete the module, or wire it in if the
connection/byte counters are wanted for observability.

### `crates/akuma-rump/src/syscall_translation.rs::FdMap` — whole struct, no instance outside its own test

```rust
pub struct FdMap { ... }
impl FdMap {
    pub fn new(first_box_fd: i32) -> Self { ... }
    pub fn insert(&mut self, rump_fd: i32) -> i32 { ... }
    pub fn to_rump(&self, box_fd: i32) -> Option<i32> { ... }
    pub fn is_rump(&self, box_fd: i32) -> bool { ... }
    pub fn remove(&mut self, box_fd: i32) -> Option<i32> { ... }
}
```

`grep -rln FdMap crates userspace src` returns only this one file — no other
module even names the type, let alone constructs one. Reads as scaffolding
for a box-fd ↔ host-rump-fd translation table that the live rump proxy path
never adopted (`crates/akuma-rump` does its syscall translation by number
elsewhere). Delete the whole struct and its one `#[cfg(test)] mod tests`
block.

### `crates/akuma-net/src/dns.rs::is_loopback` — unused, `resolve_host` reimplements the check inline

```rust
pub fn is_loopback(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1"
}
```

Only caller is `crates/akuma-net/src/tests.rs` (`#[cfg(test)]`). `resolve_host`
a few lines below does its own `if host == "localhost"` check rather than
calling this. Not the same function as `smoltcp_net.rs::is_loopback_frame`
(that one *is* used, checks an Ethernet frame, unrelated). Delete.

### `crates/akuma-net/src/locks.rs::network_lock_holder` / `socket_table_lock_holder` — doc comment describes a caller that doesn't exist

```rust
/// Current holder of the network lock, or `LOCK_HOLDER_NONE` if free.
///
/// Used by the SSH stall watchdog in `src/main.rs::memory_monitor` to detect
/// long-held locks. ...
static NETWORK_LOCK_HOLDER: AtomicU32 = AtomicU32::new(LOCK_HOLDER_NONE);
...
pub fn network_lock_holder() -> u32 { NETWORK_LOCK_HOLDER.load(Ordering::Relaxed) }
pub fn socket_table_lock_holder() -> u32 { SOCKET_TABLE_LOCK_HOLDER.load(Ordering::Relaxed) }
```

`grep -n 'network_lock_holder\|socket_table_lock_holder' src/main.rs` matches
nothing — `memory_monitor` (`src/main.rs:1648`) never reads either atomic.
The underlying `NETWORK_LOCK_HOLDER`/`SOCKET_TABLE_LOCK_HOLDER` atomics *are*
written on every lock acquire/release in `locks.rs`, so the tracking machinery
is live — only the accessor pair and the watchdog integration the comment
promises are missing. This is the same shape as the `cleanup_box_queues` gap
in `DEAD_CODE_SWEEP_FINDINGS.md` §1: a self-test-only accessor sitting next to
a half-finished feature. Two honest options: wire `memory_monitor` to actually
poll these for stall detection (what the comment says should already be
happening), or delete the accessors and correct the comment on the atomics.
Worth a decision, not a blind delete.

### `crates/akuma-net/src/locks.rs::get_lock_stats` / `reset_lock_stats` — test-only, plus an orphaned second test file

Both are called only from `locks.rs`'s own `#[cfg(test)] mod tests` (lines
293–356) and from `crates/akuma-net/src/lock_tests.rs`. The latter is worth
flagging on its own: **`lock_tests.rs` is never `mod`-declared anywhere** —
`grep -rn 'mod lock_tests' crates/akuma-net` finds nothing. It has
`#![cfg(test)]` as an inner attribute but no module path includes it, so it
is not compiled even under `cargo test`; its five `#[test] fn`s never run.
Either delete the file or add `#[cfg(test)] mod lock_tests;` to
`crates/akuma-net/src/lib.rs` to actually run it — right now it's dead weight
disguised as coverage, the same trap called out in
`DEAD_CODE_SWEEP_FINDINGS.md` §3 for `src/tests.rs`'s six orphaned allocator
tests.

### `crates/akuma-vfs/src/memfs.rs::with_max_size` — unused constructor, size limit never applied

```rust
pub fn with_max_size(max_bytes: u64) -> Self { ... max_size: max_bytes, ... }
```

Only caller is `crates/akuma-vfs/src/tests.rs`. Every production `MemFs` is
built with `MemFs::new()` (`max_size: 0`), so tmpfs-style size limiting is
implemented (the `max_size` field is real and presumably checked on write —
not verified here) but never turned on anywhere. Delete `with_max_size`, or
wire a real limit through whatever creates the box's `/tmp` `MemFs`.

### `crates/akuma-exec/src/elf/types.rs::parse_elf64_ehdr` — superseded twin, `elf/mod.rs` uses a different function

```rust
pub fn parse_elf64_ehdr(data: &[u8]) -> Option<Elf64Ehdr> { ... }
```

The actual loader (`crates/akuma-exec/src/elf/mod.rs:848`) has its own
private `parse_elf64_ehdr_checked(buf) -> Result<Elf64Ehdr, ElfError>` doing
the identical field-by-field parse with richer errors, and that's what
`load_elf_from_path` calls. `parse_elf64_ehdr` (the `Option`-returning one)
is only reached from its own three `#[cfg(test)]` cases in the same file.
Note the asymmetry: `parse_elf64_phdr` right below it (same file, same
vintage) *is* still used by `elf/mod.rs` — only the ehdr half was replaced.
Delete `parse_elf64_ehdr` and repoint its three tests at
`parse_elf64_ehdr_checked` (or drop them if that path already has coverage).

### `crates/akuma-exec/src/box_mod/access.rs::cascade_kill_order` — designed, never wired into `kill_box`

```rust
/// Get the ordered list of box IDs to kill when cascade-killing `target_box_id`.
/// Returns descendants in reverse depth order (deepest children first)
/// so that cleanup proceeds leaf-to-root.
pub fn cascade_kill_order(
    registry: &BTreeMap<u64, BoxInfo>,
    target_box_id: u64,
) -> alloc::vec::Vec<u64> { ... }
```

Only called from its own two `#[cfg(test)]` cases. The real kill path,
`akuma_exec::process::kill_box` (`crates/akuma-exec/src/process/mod.rs:451`),
does not call it — it kills only processes with `p.box_id == box_id` and
unregisters that one box:

```rust
pub fn kill_box(box_id: u64) -> Result<(), &'static str> {
    ...
    let pids: Vec<Pid> = table::collect_pids(|p| p.box_id == box_id);
    for pid in pids { let _ = kill_process(pid); }
    unregister_box(box_id);
    Ok(())
}
```

So killing a box with children leaves the children's `BoxInfo` registry
entries pointing at a dead parent — registered but orphaned, box-hierarchy's
analogue of the message-queue leak in
`DEAD_CODE_SWEEP_FINDINGS.md` §1. Whether that matters depends on whether
box nesting (`register_box` under an existing box's root, per
`can_register_box`/`validate_nested_root` in the same file) is actually used
anywhere yet — if nothing creates nested boxes today this is latent, not
live. Worth the same one-line fix as §1 if it is used
(`process::kill_box` calling `cascade_kill_order` before the existing
single-box teardown) rather than deleting the helper.

## Section B — boot self-test seams (`src/`, do not delete)

Everything below is called only from `src/{tests,process_tests,sync_tests,
pthread_tests,network_tests}.rs`, all `#[cfg(kernel_tests)]` — **on in the
default build**, run every boot via `main.rs`'s `run_memory_tests()` /
`run_threading_tests()` / `run_benchmarks()` (plus the process/sync/pthread
suites). `DEAD_CODE_SWEEP_FINDINGS.md` §2 already named this pattern for
`msgqueue_add_recv_poller` and seven siblings — "external test seams... dead
because the tests are off, not because the feature is unwired... Keep them."
Same call here for the rest of the family found by this scan. Listed for
completeness / so a future sweep doesn't re-flag them as newly-dead:

| Item | File | Suite that calls it |
|---|---|---|
| `dropped_window_open_for_tid_test` | `crates/akuma-exec/src/bkl.rs` | process_tests |
| `test_publish_core_l0`, `pending_ttbr_free_stats`, `shared_l0_stats` | `crates/akuma-exec/src/mmu/mod.rs` | process_tests |
| `clear_interrupted` | `crates/akuma-exec/src/process/channel.rs` | process_tests |
| `register_system_thread_channel` | `crates/akuma-exec/src/process/channel.rs` | sync_tests |
| `fd_table` | `crates/akuma-exec/src/process/fd.rs` | tests |
| `set_pressure_reclaim_enabled` | `crates/akuma-exec/src/process/reclaim.rs` | process_tests |
| `reclaim_retired_processes_force`, `process_count`, `register_thread_pid`, `unregister_thread_pid` | `crates/akuma-exec/src/process/table.rs` | tests, sync_tests, process_tests |
| `thread_tag`, `core_tag`, `reset_wait_by_holder` | `crates/akuma-exec/src/sync.rs` | process_tests |
| `set_thread_state`, `get_woken_state`, `set_woken_state`, `on_cpu_flag`, `on_cpu_count`, `spawn_user_thread_fn`, `trap_frame_ptr_for_thread`, `set_trap_frame_ptr_for_tid_test` | `crates/akuma-exec/src/threading/mod.rs` | tests, process_tests |
| `is_dhcp_configured` | `crates/akuma-net/src/smoltcp_net.rs` | network_tests |
| `poll_count` | `crates/akuma-net/src/smoltcp_net.rs` | tests |
| `allocated_bytes` | `src/allocator.rs` | tests, process_tests |
| `spurious_svc_count` | `src/exceptions.rs` | process_tests (already in `DEAD_CODE_SWEEP_FINDINGS.md` "No action needed" as dead-under-`size`; here it's the mirror finding — live but test-only in default) |
| `stale_window_heal_count` | `src/exceptions.rs` | process_tests |
| `cow_event_count`, `discount_uaf_detections` | `src/pmm.rs` | process_tests |
| `set_fault_bkl_drop_enabled`, `set_exec_bkl_drop_enabled`, `set_mm_bkl_drop_enabled`, `set_drivers_bkl_drop_enabled`, `set_irq_bkl_drop_enabled`, `set_sched_bklfree_el0_enabled` | `src/smp_shared.rs` | process_tests |
| `cores_that_ran_userspace`, `user_traps`, `spawn_migration_probe`, `migration_core_count`, `spawn_worker_demo`, `spawn_blocking_relax_waiters`, `cores_that_ran_workers`, `worker_ticks` | `src/smp_shared.rs` | process_tests — self-described in-file as "M2c/M4 self-test" demo infra, explicitly intentional |
| `epoll_wait_deadline_for_test`, `get_qemu_stp_xzr_ec15`, `user_va_limit_value`, `ensure_user_pages_mapped_for_test` | `src/syscall/mod.rs` | tests, process_tests, pthread_tests |
| `msgqueue_add_recv_poller`, `msgqueue_add_send_poller`, `msgqueue_recv_pollers_count`, `msgqueue_send_pollers_count`, `msgqueue_is_recv_poller`, `msgqueue_push_direct`, `msgqueue_pop_direct`, `msgqueue_message_count` | `src/syscall/msgqueue.rs` | process_tests — already documented, see `DEAD_CODE_SWEEP_FINDINGS.md` §2 |
| `pipe_is_poller_registered`, `pipe_pollers_count` | `src/syscall/pipe.rs` | process_tests — same poller-introspection pattern as msgqueue |
| `vfork_waiters_len`, `test_vfork_complete_mechanism`, `vfork_waiters_insert_for_test`, `vfork_waiters_contains_for_test` | `src/syscall/proc.rs` | process_tests |
| `drop_key` | `src/syscall/sync.rs` | process_tests |

None of these are recommended for deletion. If the boot self-test suite
itself is ever trimmed (a much bigger call — it's the only in-kernel
regression coverage this project has), re-run this scan; whichever of these
lose their last caller at that point become real Section-A-style candidates.

## Method limitations

- **Trait impls and macro-generated call sites are invisible to grep.** The
  `RawRwLock` false positive above is the general case: anything called only
  through a trait object, a derive, or an external crate's generated code
  (`lock_api::RwLock<RawRwSpinlock, T>` here) has no textual call site in this
  tree to find. Every candidate in Section A was hand-verified against this
  before being listed, but the negative space (real dead code this scan
  *didn't* flag because of some other invisible dispatch) is not measured.
- **Compile-time reachability, not runtime coverage**, same caveat as
  `DEAD_CODE_SWEEP_FINDINGS.md`: Section B items are compiled in and *do* run
  every boot, but nothing here checked that any Section A candidate's
  disappearance wouldn't be masked by a `#[cfg]` combination not scanned
  (the scan reads source text, not per-feature-set expansions).
- **Common names create noise the scan already filtered by hand** — short,
  generic identifiers (`new`, `len`, `get`, …) were excluded implicitly by
  the "occurrences found only in test files" test only firing when *every*
  occurrence of that exact name across ~95k lines happens to be test-only,
  which generic names never satisfy. It underreports rather than overreports
  for that reason.

## Background

- [`DEAD_CODE_ANALYSIS.md`](DEAD_CODE_ANALYSIS.md) — earlier, narrower sweep
  (brk syscall, `KERNEL_CONTEXTS`).
- [`DEAD_CODE_SWEEP_FINDINGS.md`](DEAD_CODE_SWEEP_FINDINGS.md) — the
  `dead_code`-lint sweep this doc's Section A extends into `crates/`; §1 and
  §2 are the precedent for how the `cascade_kill_order` and
  `network_lock_holder` findings above are framed.
