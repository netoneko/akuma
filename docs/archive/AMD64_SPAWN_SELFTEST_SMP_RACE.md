# amd64: `spawn:` fd-table self-tests fail on some boots at SMP>1 — a test race, not a regression

**Status: ROOT-CAUSED 2026-09-25, not yet fixed.** The kernel is fine; the
self-test's assumption is not.

## Symptom

Some boots of the Ryzen Firecracker guest (`sora.service`, 2 vCPUs) end the
suite with

```
  spawn: sys_spawn returned a handle   [OK]
  spawn: pid is a real child pid   [OK]
  spawn: the child's registered table holds fd 0 as Stdin   [FAIL]
  spawn: fd 1 as Stdout   [FAIL]
  spawn: and fd 2 as Stderr   [FAIL]
  spawn: and the child has an I/O channel behind them   [OK]
  spawn: the child's stdout came back through the channel   [OK]
  spawn: waitpid reported the child's exit status   [OK]
  spawn: reading the child's stdout after the reap is EBADF   [OK]
  spawn: teardown leaks nothing   [OK]
Akuma/amd64 self-test: 794 passed, 3 FAILED
```

and other boots pass all 797.

## It is not a code change

On the Ryzen host, `~/akuma/boot.log.pre-oom-20260925` (794/3) and
`~/akuma/boot.log.prev` (797/0) came from **the same kernel file**. It was
staged at 02:27 and not replaced until the 16:44 redeploy. The test itself
(`spawn_test`, `amd64/src/usermode.rs`, the check at ~line 5714) is unchanged
since `e414f41d` (2026-09-11). One binary gives both results, so this is timing.

## Mechanism

1. `spawn_test` calls `sys_spawn("/bin/hello")`, then looks up the child with
   `with_process(pid, …)` and asks whether its fd table holds
   `Stdin`/`Stdout`/`Stderr` at 0/1/2.
2. The test's own comment says this is "the one moment the table is guaranteed
   populated and not yet swept by `close_all`". That is only true on one core.
   The `smp:` tests earlier in the suite have already brought the secondaries
   online and shown workers running on both cores, so at this point the child
   can be scheduled on the other vCPU at once.
3. `hello` is tiny. It can run to `exit_group` before the parent reaches the
   check. On exit, `run_process` calls `fds.close_all()` immediately
   (`amd64/src/usermode.rs`, the `exit_fds` sweep after `thread::drain`), but
   the `Process` record, and its `channel`, stays registered until the parent's
   `waitpid` reaps it.
4. So the parent finds a live record with a channel and an **empty** fd table:
   the three fd checks fail and the channel check passes. That is exactly the
   observed pattern, and everything after the check (stdout through the
   channel, the exit status, EBADF after the reap, no leak) passes on every boot.

**Scope:** harmless. The child's stdio works; only the moment of observation
is wrong. It is unrelated to the `[bkls>]` stalls on the same host.

## Fix (proposed)

Make the child unable to exit before it is observed: spawn a program that
blocks reading stdin, run the fd-table checks, then release it by closing its
input and continue with the output/exit/teardown checks. That keeps the check
strict at any vCPU count.

Not recommended: skipping the checks when `p.exited` is already set. That
makes the test pass by absence whenever the race goes the child's way, so a
real regression in spawn's stdio binding would pass whenever the child happens
to exit first.

## Verify

After the fix, boot the Firecracker guest at `vcpu_count >= 2` several times:
every boot must report `797 passed, 0 failed` (or the new total), and
`grep -a 'spawn:' boot.log` must show all `[OK]`.

---

**Background:** the stdio shape the test asserts is from
[`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md`](AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md);
the exit-time `close_all` ordering is from
[`AKUMA_AMD64_C2_SLICES_6_AND_7.md`](AKUMA_AMD64_C2_SLICES_6_AND_7.md).
