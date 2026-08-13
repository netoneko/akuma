# extreme-size: every ssh session died instantly — a 64 KB kernel stack overrun into a page table

**Status: FIXED, 2026-08-13.** `ssh host <cmd>` and interactive PTY both work on
the `extreme-size` profile again, at 64 MB and at the 4.0 MB floor.

This is the failure
[`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
§8.5 recorded as *"ssh sessions die instantly on the extreme-size profile — a
degradation of unknown origin, not bisected, owned by no phase"*, and which made
`acceptance/05_meow_tcc_extreme_4mb.md` unrunnable.

## Symptom

On `scripts/build_extreme_size.sh`, every ssh connection died before
authentication. The session child faulted with byte-identical registers on every
attempt and on every memory size:

```
[Fault] Data abort from EL0 at FAR=0x10, ELR=0x415330, ISS=0x7
[Fault]  x0=0x0 x1=0x201fffd588 x2=0x0 x3=0x1
[Fault] Process 2 (/bin/sshd) SIGSEGV after 0.07s
```

Invisible on every other profile: release, devbox-smoltcp and devbox-rump all
ssh fine.

## Root cause

**The extreme profile's 64 KB user-thread kernel stack is ~10 KB too small for
the sshd session path, and the overrun landed in a page table.**

`config::USER_THREAD_STACK_SIZE` was 64 KB on `extreme`, sized (per its own
comment) for *"tcc's syscall depth — open/read/write/mmap/brk — is shallow"*.
The deep path in this kernel is SSH exec / shell spawn, and at the time 64 KB was
chosen that path ran on an **SSH system thread**, covered by the 96 KB
`SYSTEM_THREAD_STACK_SIZE` measured for it.

Moving sshd to userspace with process-per-session (`userspace/sshd`,
`fork-sessions`) moved that same path onto a **user** thread: each session is a
forked process, and its syscalls run on a user-thread kernel stack. The measured
requirement did not change; the budget applied to it dropped from 96 KB to 64 KB.

`threading::report_stack_high_water` (with `STACK_USAGE_PROBE`) measures the path
at **74 KB** — the same probe measured 79 KB when it drove the system stack. So
every session overran its stack by ~10 KB.

### Why it presented as a nonsensical SIGSEGV

Thread stacks come from the PMM, so the pages *below* a stack are ordinary
allocations. The run-off wrote into whichever frame happened to sit there. In
this configuration that was the session process's **own L3 page table**:

```
[FORK-PROBE]   share eager 0x10410000 len=0x10000 -> 16     <- fork mapped all 16 pages
[FORK-PROBE]   child-check 0x10410000 pages=16 missing=0    <- child's tables complete
...
[TP-watch] nr=301 a0=0x10410c60 a1=0x10410c80 pte 0x600000400bdf4f -> 0x0
```

Syscall 301 is `SPAWN`; the PTE died inside `vfs::resolve_symlinks`, called at
the top of `spawn_process_with_channel_ext` when sshd spawns the login shell.
Three consecutive PTEs (L3 indices 16–18) were zeroed, unmapping the child's
malloc arena at `0x10410000`. The process then faulted on memory it had every
right to expect, at an address with no relationship to the corruption — and the
kernel additionally took EL1 aborts trying to read `write()` buffers in the
now-unmapped arena.

Nothing anywhere said "stack". The three PTE-clearing primitives in
`akuma-exec::mmu` (`unmap_page_no_flush`, `unmap_and_free_page_no_flush`,
`try_evict_ro_page`) were each instrumented and **none of them fired** — because
the write did not come through the MMU code at all.

### Second defect: the canary was painted but never checked

`ENABLE_STACK_CANARIES` is `true`, `init_stack_canary` paints every stack base,
and `check_all_stack_canaries()` exists — with **zero callers anywhere in the
tree**. A guard nothing calls is not a guard, which is why a 10 KB overrun could
corrupt a page table in silence and be misfiled for weeks as an unexplained
SIGSEGV.

## Fix

Two parts, in `config.rs` and `akuma-exec::threading`:

1. **`USER_THREAD_STACK_SIZE` on `extreme`: 64 KB → 128 KB.** Not 96 KB: the
   system stack's 96 KB leaves only a 17 KB margin over the same measurement, and
   a user thread additionally carries a nested IRQ trap frame. Stacks are
   allocated on demand here (`WARM_FREE_USER == 0`), so the cost is per *live*
   user thread — one or two at the 4.0 MB floor — not per slot.

2. **The canary is now checked**, in two places:
   - `ThreadPool::free_stack_for_slot` — thread teardown, the last moment the
     evidence exists before the frames go back to the PMM. Free: that code
     already walks the stack for the high-water probe.
   - `threading::report_overrun_stack_canaries()`, called from thread 0's idle
     maintenance pass, for **live** threads — the case teardown cannot cover,
     since an overflow whose damage hangs or panics the box never reaches
     teardown. Latched per `(slot, stack base)` so it prints once per broken
     stack, and disarmed again whenever the canary reads intact.

   Both print `[STACK-OVERFLOW] tid=N ran off its NKB kernel stack (base=…)`.

## Verify

```bash
scripts/build_extreme_size.sh
ELF=target/aarch64-unknown-none/extreme-size/akuma
MEMORY=64M SNAPSHOT=1 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee extreme.log
until grep -aq "sshd started" extreme.log; do sleep 2; done
```

Then, via Python (the `ssh` CLI is blocked by policy — see CLAUDE.md):

```python
subprocess.run(["ssh","-o","StrictHostKeyChecking=no","-p","2222",
                "root@localhost","uname -a"])
```

Expect `Akuma 0.1.0 Akuma-OS aarch64`, and **no** `[Fault]`, `[WILD-DA]` or
`[STACK-OVERFLOW]` line in the log. Repeat at `MEMORY=4096K` — the acceptance/05
floor — which must also boot and serve ssh.

To re-measure the headroom, flip `MEM_MONITOR_ENABLED` on in `src/config.rs`
(`STACK_USAGE_PROBE` in `threading/mod.rs` is already `true`) and read:

```
[Stack] sys peak 74KB/96KB | user peak 74KB/128KB | boot peak 11KB/32KB
```

The boot suite guards the detector, not the size:
`[Test] stack_canary_overrun_is_reported PASSED` (src/process_tests.rs) asserts
both halves — no false positive on a healthy boot, and a deliberately broken
canary on an idle slot detected exactly once.

## Notes for whoever reads this next

- **`sys peak 74KB/96KB` is a 22 KB margin on the system stack**, against the
  same path. It has not overflowed, but it is the next-tightest number in the
  tree and worth watching.
- The `[MMAP-STALE-PTE]` line that appears in some captures of this crash is a
  *downstream* symptom, not a lead: `sys_mmap` refusing a VA whose PTE looked
  valid is the same corrupted table seen from the other side.
- Two A/Bs were run and both were red herrings worth recording so nobody repeats
  them: forcing the eager ELF loader (`HEAP_SLURP_MAX` non-zero on extreme)
  changes the crash into a hang, and disabling CoW fork
  (`COW_FORK_ENABLED = false`) reproduces the fault byte-for-byte. Neither the
  demand-paged loader nor CoW is involved.

## Follow-on: the playbook needed one more fix

With ssh working again, `acceptance/05`'s compile step still failed —
`tcc: error: file 'tcc' not found`. That is a **second, unrelated** bug, in
`paws` (extreme's login shell), which passed the command name to `spawn` as an
argument on top of the `argv[0]` the syscall already prepends. It reproduces on
`release` via `paws -c`, so it is not a profile bug; it had simply never been
reachable on `extreme` while ssh was down.
See [`PAWS_DUPLICATED_ARGV0.md`](PAWS_DUPLICATED_ARGV0.md). With both fixed,
`tcc -static -B /usr/lib/tcc` compiles and the output runs at the 4.0 MB floor.

## Background

- [`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  §8.5 — where this was logged as an unowned, unbisected degradation, with the
  A/B that (correctly) cleared the Phase 2a ELF-parser merge of causing it.
- `acceptance/05_meow_tcc_extreme_4mb.md` — the playbook this unblocks.
- [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) — the move to userspace
  sshd that shifted the deep path from a system thread to a user thread.
- `userspace/sshd/docs/OPTIONAL_PARALLELISM.md` — the process-per-session design.
- [`RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`](RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md)
  — the other "ssh dies instantly" bug fixed the same day, on the rump devbox,
  with a completely unrelated cause. Two identical-looking symptoms; check which
  profile you are on before assuming either.
