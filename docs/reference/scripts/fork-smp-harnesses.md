# Fork / SMP regression harnesses

Grade: — (index)

Each of these boots `devbox-smoltcp` at a given `SMP=` level over SSH and
looks for a specific crash/corruption signature. They predate
`scripts/bkl_smp_regimen/`'s more general stress+attribution regimen but
exercise different, narrower scenarios (fork-hammer over many SSH
connections, a memory-split/VA-cap sweep, a specific WILD-DA repro) that
regimen doesn't cover — useful to re-run after any change touching fork,
`CLONE_VM`, or the scheduler.

| Script | What it does |
|---|---|
| [`validate_fork_smp.py`](../../../scripts/validate_fork_smp.py) | Fork-corruption validator at SMP=4: 16 concurrent SSH connections each fork-hammering via busybox; greps the boot log for SIGSEGV/corruption signatures. Success bar is 0 fault lines across N boots × M rounds. |
| [`quick_forktest.py`](../../../scripts/quick_forktest.py) | Quick forktest (Go) sanity pass at SMP=2 and SMP=4 — the fast subset of the matrix below. |
| [`forktest_smp_matrix.py`](../../../scripts/forktest_smp_matrix.py) | Runs forktest across parameter combinations (mmap, file I/O, signals, goroutine stress) at SMP=2/4 — the fuller version of `quick_forktest.py`. |
| [`test_memory_split.py`](../../../scripts/test_memory_split.py) | Boots at several `MEMORY=` sizes and compiles a program (tcc for small sizes, rustc for larger) at each, to characterize the kernel/user VA-split and identity-map cap. Writes `logs/split_summary.txt`. |
| [`sshd_crash_hunt.py`](../../../scripts/sshd_crash_hunt.py) | Repro harness for the SMP=4 fork-hammer WILD-DA `FAR=0x0` crash class. |
| [`test_sched_bklfree_ticket_fix.py`](../../../scripts/test_sched_bklfree_ticket_fix.py) | Regression check for the BKL fair-FIFO ticket-leak fix (`sched_bklfree_el0`, M5c step 2) — boots SMP=4 and confirms no ticket-accounting drift. Background: the `SMP_SHARED.md` ticket-leak entry in [`../../archive/BUG_FIX_LIST.md`](../../archive/BUG_FIX_LIST.md). |

Back to [`README.md`](README.md).
