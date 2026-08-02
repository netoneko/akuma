# The unresponsive-VM defect: execve stack leak → kernel-heap wall → lock-abandonment hang

**Date:** 2026-08-02. **Build:** `release-smp-shared` + `devbox-smoltcp`, SMP=1,
MEMORY=4096, HVF. **Status:** leak root-caused and fixed; hang deadlock class
identified by audit, final-spin PC not yet captured live.

## 1. Symptom

Under the `big.rs` verification hammer (rounds of 4 concurrent
`rustc -O big.rs` over SSH), the box died at ~12–24 min in three separate runs:

| log | build | outcome |
|---|---|---|
| `bigrs_prefix_HANG_EVIDENCE.log` | pre-dc4684a | no hang in 26 min (the stale-slot bug hung one job; load collapsed after) |
| `bigrs_HANG_from_phase2_clear.log` | dc4684a | serial froze ~T700-740, dead 22 min until killed |
| `bigrs_fixed2.log` | 0479cd8 | serial froze at T839.57, dead 15 min until killed |

Hang shape: ~100 % CPU, serial output stops **completely** (no PSTATS, no
panic, no `[OOM]`), SSH dead. `stale tid=` fired 0 times in all runs.

Log forensics that framed it: in both hung runs the sshd PSTATS `pmm=` series
collapsed in the final 60–90 s (590k→187k and 282k→144k free pages) and the
freeze lands right where the burn-rate extrapolates. Not a stale build
(binary mtime 19:22 = the 0479cd8 tree; repro rebuild was a no-op).

## 2. Repro and the intermediate state the old runs never showed

Fresh boot (same binary, gdbstub attached), same hammer. Rounds 1–4 clean
(~175–195 s each); PMM troughs deepened each round (917k baseline → 66k trough,
recovering only to ~510k). Round 5: sshd starts failing with
`failed to spawn '/bin/sh' for exec`, and the serial shows the state the
hung runs never printed:

```
[OOM] allocation of 1116408 bytes failed (heap 985MB / 1005MB used) — killing process
```

repeating once per spawn attempt, PSTATS alive, `pmm=141918` frozen exactly
(no user-side allocation at all), sshd `spawn=698`. The kernel heap — not the
PMM — is the exhausted resource. `[HEAP-GROW]` crossings at 512 MB and 768 MB
were both driven by `this_req=1116408`.

1116408 B = the size of `/bin/busybox` (`/bin/sh`). 698 spawns × ~1.1 MB ≈
770 MB. The heap is full of dead ELF images.

## 3. Root cause: successful execve leaks its whole syscall stack

`do_execve` (src/syscall/proc.rs) on the shared-SMP profile reads the **entire
binary** into a `Vec<u8>` (`fs::read_file`, BKL-dropped window), then commits
via `replace_image` and ends with:

```rust
proc.address_space.activate();
akuma_exec::process::enter_user_mode(&proc.context);   // -> !  (eret to EL0)
```

`enter_user_mode` never returns: the kernel stack of the execve syscall is
abandoned, **no destructor on it ever runs**. Every successful execve leaked:

- the whole-file ELF buffer (1.1 MB per `busybox sh`; more for bigger binaries),
- `args` / `env` vectors, `resolved_path`,
- on the shebang path additionally the script buffer and original argv
  (outer `do_execve` frame abandoned by the inner exec's eret).

The hammer's SSH polling architecture executes `busybox sh` twice per poll
(nohup wrapper + inner sh) plus rustc's own `cc`/`ld` execs — a steady
~1 MB/exec ratchet that hits the heap wall at T≈700–840 s. Deterministic
timing across runs; workloads without exec churn (llama, boot suite) never see it.

**Fix (same day):** `do_execve` closure now takes ownership and drops the ELF
buffer immediately after `replace_image` (success *and* failure), and drops
`args`/`env`/`resolved_path` explicitly before `enter_user_mode`;
`exec_shebang` takes owned `script_path`/`file_data`/argv and frees them
before recursing. General rule recorded in
`docs/reference/subsystems/thread-lifecycle.md` §4: **nothing heap-owned may
be live across an eret leaf.**

## 4. From heap wall to silent hang

The `[OOM] … killing process` loop is the benign face: `alloc_error_handler`
kills the requesting process when the failing allocation happens lock-free.
The hang is the malign face of the same wall, established by a full teardown
lock audit (see thread-lifecycle.md §3–§5):

1. An allocation fails while a subsystem spinlock is held — in-tree documented
   example: `pipe_write` growing its buffer under `PIPES` with IRQs masked
   (`syscall/pipe.rs:29-35`); same shape for `SHARED_L0_TABLE` insert on fork,
   `fds.table` clone in `close_all`, epoll/eventfd inserts.
2. `alloc_error_handler` → `return_to_kernel(-12)` — a `-> !` function. The
   held guard is abandoned, never released.
3. Teardown (`cleanup_process_fds`) and, since dc4684a, the pressure drain
   (`process/reclaim.rs` — which unconditionally drops *other* RETIRED
   processes' fd tables) re-acquire the abandoned lock.
4. Single core, non-reentrant spinlock, IRQs masked ⇒ 100 % CPU, frozen
   serial, no output. Matches the observed hang exactly, including the
   absence of `[OOM]` lines when the *first* failing allocation is already
   inside the lock.

dc4684a's drain did not create the class (step 3's `cleanup_process_fds` route
predates it) but widened it substantially, which is consistent with the hang
becoming reliable enough to block hammer runs the day it landed.

A second, independent route into the same class: the EL1 demand-pager runs
**before** the `copy_*_safe` EFAULT fixup (`rust_sync_el1_handler` order), so
"safe" user copies under a lock (futex word read under `FUTEX_WAITERS`,
msgqueue/timerfd/term/sigaction sites) can enter the pressure ladder → drain →
§3 lock set while their lock is held. Stale comments claiming the opposite:
`syscall/sync.rs:24-31`, `pmm.rs:770-775`, `process/reclaim.rs` module doc.

## 5. Corrections to earlier conclusions

- `STALE_THREAD_SLOT_KILL.md` §5.1 attributed a round-4 hammer hang to the
  PHASE-2 `thread_id`-clear edit ("that was tried and it hung the box"). The
  post-revert build hung identically at the same uptime, and the hang
  tracks the exec-count/heap ratchet, not that edit. The §5.1 *reasoning*
  about the backstop still stands on its own; its hang evidence does not.
- "Corroborating hammer run blocked by a separate unresponsive-box defect" —
  the defect is this leak+hang chain; with the leak fixed the hammer and the
  in-VM self-host build are unblocked (both are exec-heavy).

## 6. Open items

- Capture the hang PC live (gdbstub + lldb) to name the actual lock of step 4
  at least once — the class is proven by audit, the instance is not.
- Decide the structural fix for §4 route 2 (gate the lazy resolver behind the
  fixup check, or force no-drain allocation inside `copy_*_safe` windows).
- Sweep the §5.1 copy-under-lock sites toward the in-tree correct patterns
  (`fs.rs:318` drop-then-copy, `poll.rs:334` hoist, `exceptions.rs:1180`
  pre-map).
- `return_to_kernel*` upstream frames deserve the same "nothing heap-owned"
  audit as execve got (every process exit abandons its exit-syscall frame).
