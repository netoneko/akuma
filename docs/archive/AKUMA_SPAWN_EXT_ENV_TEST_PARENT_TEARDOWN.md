# `test_spawn_ext_passes_env` panics the boot suite under KVM (lima)

Found 2026-09-10 on lima (nested KVM, `scripts/lima_aarch64_run.sh`): the aarch64
boot suite panicked at `src/process_tests.rs` `test_spawn_ext_passes_env` —
2 of 2 cases failed, both with **empty child output** — before userspace came
up. Committed HEAD reproduced it identically, so it was pre-existing, not a
regression of the in-flight work. Root-caused and fixed the same day.

```
[Test] spawn_ext env FAILED, child saw: 
[Test] spawn_ext default env FAILED, child saw: 
[Test] spawn_ext_passes_env FAILED (2 of 2)

!!! PANIC !!!
Location: src/process_tests.rs:3411
```

## Why the 2026-09-07 closure was wrong about the cause

`AKUMA_SELF_HOSTING_AMD64.md` issue 4 was closed 2026-09-07 by re-measuring:
`MEMORY=2048 cargo run --release` — that is, **locally, under HVF or TCG** —
reached `PASSED`, and the failure was written off as "a transient state of the
branch". The re-measurement was honest but the conclusion attributed the
failure to the wrong variable. The real variable was the **accelerator's
timing**, not the branch state and not the memory size: the bug is a race the
test itself sets up, which fast KVM scheduling wins and slower HVF/TCG
scheduling loses. Re-verified 2026-09-10: committed HEAD passes under local
HVF (MEMORY=2048 and 256M) and local TCG, and fails under lima/KVM — same
binary semantics, different scheduler pacing.

## The mechanism

The test (`src/process_tests.rs`, `test_spawn_ext_passes_env` → `run_env`)
drives the real `SPAWN_EXT` syscall entry with a fake parent process
(`register_at_syscall_process(7710, …)`) and then reads what the child
(`/bin/busybox env`) printed, via `get_child_channel`. The old order was:

1. `handle_syscall(SPAWN_EXT, …)` — spawns the child.
2. `unregister_at_syscall_process(7710, …)` — tears down the fake parent
   **immediately**, before reading anything.
3. Drain loop: poll `get_child_channel(child)` up to 2000 yields.

Step 2 is what kills it. `sys_spawn_ext`
(`crates/akuma-syscalls-glue/src/proc.rs:1728-1730`) stores the child's stdout
channel in two places: the global `CHILD_CHANNELS` map **and** a
`FileDescriptor::ChildStdout(pid)` in **the spawner's fd table**. The spawner
here is the fake parent — so unregistering it retires pid 7710, and when the
retired slot is reclaimed, `FdTable::drop → close_all → remove_child_channel`
(`crates/akuma-exec/src/process/fd.rs:235-237`) deletes the `CHILD_CHANNELS`
entry **with whatever the child wrote still buffered in it**.

The instrumented failing run (lima/KVM) pinned the shape exactly:

```
[DBG] spawn r=76 child=118
[DBG] child=118 iters=11 chan_missing=1 exited=2 out_len=0
```

Eleven scheduler yields after the spawn, the channel was gone. The child had
run fine — in the fixed run the same child produces 45 bytes (`busybox env`'s
composed environment) and exits cleanly — the test just destroyed the only
handle to the output before draining it. Both cases failed identically because
both go through the same `run_env`.

## Why it is timing-sensitive

The reclaim of the retired fake parent and the child's exec-to-output are
racing, and the drain loop is the scoreboard:

- **KVM (lima):** guest scheduling runs near-native; the child execs and the
  retired-slot reclaim lands within ~11 yields — inside the drain window, so
  the channel vanishes before or as the loop polls it.
- **HVF/TCG (local QEMU):** everything is slower relative to the loop's
  2000-iteration budget, so the drain wins the race and the test passes.

That is also why the failure reproduced "line-for-line" on every lima boot:
nothing about it is flaky at KVM speed — the race is decided the same way
every time.

## The fix

Move the teardown after the drain — the parent must outlive the read of its
child's output, exactly as a real spawner does:

```rust
// drain loop ...
unregister_at_syscall_process(pid, tid);   // was: immediately after handle_syscall
alloc::string::String::from_utf8_lossy(&out).into_owned()
```

One line moved, plus a comment at the site stating the obligation. No kernel
code changed — the kernel's behaviour (`ChildStdout` lives in the spawner's fd
table; reclaiming the spawner closes the channel) is correct and is what a
real process teardown should do.

## Verify

On lima (KVM), from a build of the fixed tree:

```
limactl shell fc sh scripts/lima_aarch64_run.sh
```

→ `[Test] spawn_ext_passes_env PASSED (composed + default)`, the suite does
not panic, and boot reaches a working sshd. Locally the same binary still
passes under `MEMORY=2048` HVF and TCG. Host tests and clippy green.

Operational note for anyone re-running this: lima's mount served a **stale
staged ELF** on one re-run (`cp -f` in `scripts/lima_aarch64_run.sh` copied
old bytes through the guest's view of the mount — the staged
`/tmp/akuma-lima/akuma` md5 did not match the host ELF). If a lima run reports
a failure you believe you fixed, compare `md5` of the staged kernel against
the host's `target/aarch64-unknown-none/release/akuma` before believing it;
copying the ELF to a fresh filename and pointing `KERNEL=` at it sidesteps the
stale view.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` issue 4 — the original sighting
  and the (mis-attributed) closure; corrected 2026-09-10 to point here.
- The channel-lifecycle rules this test tripped over live in
  `crates/akuma-exec/src/process/children.rs` (`reap_child_channel`'s header
  explains why a reaped zombie's stdout must survive until drained — the same
  "output outlives the reader's teardown" principle, from the other side).
