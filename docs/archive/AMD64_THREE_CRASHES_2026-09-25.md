# amd64 bare metal: three console photos, two defects — 2026-09-25

Three photographs of the HP box's console (`IMG_6227`–`IMG_6229`, bare-metal
Akuma/amd64, `ip=192.168.1.120`, 5 cores, root on the USB/xHCI disk, running
the akuma-miot `kot` litter under herd). Two of them are one wedge and the third
is a separate crash. Both defects are fixed in the tree (uncommitted), and
**neither fix has been verified on the metal or on ryzen yet** — see §5.

| photo | what it shows | defect |
|---|---|---|
| 6227 | `[BKL] stuck: owner=3 waiter=1/4 tag=11` forever, `[TLB] stuck: 1 peer(s) unacked, generation=384300` every few lines, the generation never moving | **A** — shootdown never acked by a core that is polling the disk with the BKL dropped |
| 6228 | the same, with `[xhci] transfer timeout: CBW after 10000 ms`, `PORTSC=0x006202a0`, `ep … dw0=0x1` (Running), `aborting the live TD (stop ep)` interleaved | **A**, and it names the silent core's activity |
| 6229 | `[bkls>]` waits of 2^20–2^21 spins (1–2 s) with rotating owners, one `[TRAMP-MISMATCH]`, then `[EXCEPTION] #SS stack fault err=0 rip=0x1098c136 cs=0x23` and the machine stops | **B** — a ring-3 `#SS` was fatal to the kernel |

## 1. Defect A — a TLB shootdown nobody can acknowledge

### Reading the photo

* `tag=11` is the holder's syscall, and on this target the tag is the raw
  **x86_64** number (`usermode.rs` stamps `set_holder_tag(cpu, nr)`), so core 3
  is in **`munmap`**. `munmap` is a flush with `TlbTarget::AllCores`: core 3
  broadcast vector 33 and sits in `shootdown::wait_for_acks` holding the BKL.
* Waiters 1 and 4 are in the BKL ticket wait, and that loop calls
  `shootdown::bkl_spin_assist` on every spin — they acknowledge. The one
  missing peer is therefore core 0 or core 2: in neither the BKL wait nor
  ring 3 nor `hlt`.
* The generation is constant across the whole photo: one shootdown, stuck.

### The missing state

`shootdown.rs`'s deadlock argument listed four states a peer can be in while
the sender waits (ring 3, `hlt`, a lock-free interrupt handler, the BKL ticket
wait) and concluded that every peer acknowledges. There is a fifth:

**kernel code running IRQ-masked with the BKL deliberately dropped.**

* Syscalls run with `IF` clear (`syscall_handler`: "Runs on the dedicated
  syscall stack with interrupts off").
* `exec_runtime::bkl_free_io` drops the BKL around `execve`'s image read, the
  interpreter reads, `read_at`, `resolve_file_id` and `read_at_by_inode` — so a
  slow disk does not freeze every core (the 2026-09-11 stall).
* On the metal that I/O is `xhci.rs`, which **polls**: `transfer` spins
  `spin_us(10)` for up to `BULK_BUDGET_MS` = 10 s per BOT phase, then the
  recovery ladder spins again.

A core there takes no IPI and is not in the ticket wait, so it cannot ack. When
the disk is healthy the window is microseconds and a BKL holder waiting on it
only shows up as the 1–2 s `[bkls>]` samples in photo 6229. When the drive
stalls (photo 6228: CBW timeout, endpoint still Running, stop-endpoint abort),
the window is the whole of every timeout plus every retry, and the sender holds
the BKL for all of it — the other three cores queue behind it and the box is
dead to the network.

A second disk reader made it worse: `XHCI` is a `spinning_top::Spinlock`, and
a core contending for it spins IRQ-masked inside `lock()` with no hook — a
second core that can never ack.

### Fix

* `shootdown::masked_wait_assist()` — `service_pending()` gated on
  `online_cpus() > 1`. Servicing at an arbitrary instruction is equivalent to
  the IPI being delivered there, which is exactly what `IF=1` would do, so it is
  sound wherever the IPI is.
* `xhci::spin_us` calls it every iteration. Every poll in the driver (command
  completion, each bulk phase, port reset, controller reset, the recovery
  steps) waits through `spin_us`, so this one line covers all of them.
* `xhci::lock_xhci()` — `try_lock` + the assist, replacing all eight
  `XHCI.lock()` sites.
* `wait_for_acks`'s `[TLB] stuck` line now also prints `sender=` and a
  `missing=0x…` cpu mask. Photo 6227 had to *infer* which core was silent from
  the absence of its number in the BKL lines; the next capture states it.
* The deadlock argument in the module header now names the fifth state.

The slow disk still costs whoever reads it. What changes is that it no longer
freezes the other cores.

## 2. Defect B — a ring-3 `#SS` halted the machine

### What faulted

`rip=0x1098c136`, `cs=0x23` (ring 3). Static-PIE programs load at
`PIE_BASE = 0x1000_0000`, so this is offset `0x98c136` — and in the akuma-miot
`kot` build that offset is inside musl's **`__libc_free`**:

```
98c12b: 48 8b 6b f0     mov  rbp, [rbx-0x10]      ; meta = group->meta
98c12f: 48 89 d9        mov  rcx, rbx
98c132: 48 83 e9 10     sub  rcx, 0x10            ; rcx = group base
98c136: 48 3b 4d 10     cmp  rcx, [rbp+0x10]      ; assert(meta->mem == base)
```

This is mallocng's `get_meta`. `rbp` is a `meta` pointer read out of a heap
group header, and it was garbage. A non-canonical address with `rbp`/`rsp` as
the base register raises **`#SS`**, not `#GP`, and on this target vector 12 was
a generated `x86-interrupt` stub that went straight to `fatal()`. One process's
heap corruption took down all five cores.

(The kot binary I disassembled is the Mac's 05:16 build, and the one on the box
may be older. The instruction bytes match exactly around a `hlt` assert ladder
that only `get_meta` has, so I'm confident in the attribution, but the
build-id was not compared.)

The dump itself was also compromised: those stubs never `swapgs`, so from ring
3 `gs` was the program's, `percpu_installed()` read false, and the
`core=`/`task_slot=` line is missing from the photo.

### Fix

`#DE`, `#NP`, `#SS`, `#AC`, `#MF` and `#XM` now enter through `TrapRegs`-saving
stubs that `swapgs` on a ring-3 origin (`fixable_exception_entry!` for the
error-code vectors, a new `trap_entry_no_code!` shaped like `#UD`'s), and share
one dispatcher, `ring3_exception`. From ring 3 each one becomes the signal
Linux raises (`arch/x86/kernel/traps.c`):

| vector | signal | `si_code` |
|---|---|---|
| `#DE` | `SIGFPE` | `FPE_INTDIV` |
| `#MF`, `#XM` | `SIGFPE` | `SI_KERNEL` |
| `#NP`, `#SS` | `SIGBUS` | `SI_KERNEL` |
| `#AC` | `SIGBUS` | `BUS_ADRALN` |

It is delivered to the program's handler if one is installed. Otherwise
`user_fault` runs, which now takes the signal and kills **the process** with
it. From ring 0 each vector is still `fatal`. `#BP`/`#OF` are not included:
their gates are DPL 0, so `int3`/`into` from ring 3 already arrive as `#GP`
and become `SIGSEGV`.

`#DE` was the same bug waiting to happen: any C program dividing by zero
(`tcc` output, `awk`) would have halted the box.

**What this does not fix: kot's heap corruption.** After the fix, kot dies of
`SIGBUS` and herd restarts it. Whether the corruption is kot's own bug or the
kernel corrupting user memory is **not known**. One photo does not say, and it
deserves its own investigation if it recurs. Worth checking first: whether it
correlates with the defect-A windows, where a peer core runs BKL-free user
copies (`read_at_by_inode` into a user buffer) while a `munmap` on another core
is changing the mapping.

## 3. The `[TRAMP-MISMATCH]` line is not a third defect

`tid=17 THREAD_PID_MAP=322 but table scan found 39 — using 322`: a new process
(322) was spawned onto task slot 17, and an old `Process` (39) still records
`thread_id = Some(17)`. `resolve_thread_process` trusts `THREAD_PID_MAP` —
which is correct — and says so. The stale row is the "nothing reaps an orphan"
leftover (`AKUMA_AMD64_NO_SLOT_RECYCLER.md`). The ryzen guest's current log has
three of the same lines. It printed just before the `#SS`, but nothing
connects the two.

## 4. Verification done

| check | result |
|---|---|
| `cargo build -p akuma-amd64 --target x86_64-unknown-none --release` | clean, no new warnings |
| `cargo clippy` (same target) | no warnings in the three changed files; 7 pre-existing elsewhere |
| boot suite, local QEMU/TCG, `SMP=4` (`amd64_trials.py --local-only --smp 4`) | **797 passed, 0 failed** |
| ring-3 check, `SMP=4`, 20 sessions (`amd64_ring3_check.py --smp 4 -n 20`) | **OK** — grandfork, sigprobe, clockprobe, **trapprobe** all rc=0; guest alive after the traps; heap drift +180 kB |
| xHCI rig (`amd64/run-xhci.sh`, q35 + qemu-xhci) | disk reads fine; 474 passed / 5 failed, **identical** to a HEAD build on the same disk image (the 5 are the rig disk having no ext2 root) |
| `trapprobe` on real Linux (the Ryzen, 6.17) | all four rungs pass — `#SS` really is `SIGBUS` there |

New probe: `userspace/forktest/c_stress/trapprobe.c`, wired into
`amd64_ring3_check.py`. Four rungs: `#DE` caught with `FPE_INTDIV`, `#SS`
caught, and each one killing only a child when there is no handler, followed by
an ssh round-trip.

**QEMU TCG cannot exercise the `#SS` path**: its emulation raises `#GP` for a
non-canonical stack reference (measured: the TCG guest's `[Fault] #GP … rbp=0x8badf00ddeadbeef`).
The probe says `NOT EXERCISED` when that happens rather than passing silently.
Only KVM or the metal proves the `#SS` path. `#DE` is fully exercised under TCG.

## 5. Not verified yet

* **ryzen (Firecracker, KVM on the Ryzen 7 8845HS).** This is where the `#SS`
  path runs on real silicon. Restarting the live guest was declined by the
  permission gate, so it has not been done. The steps are below.
  `amd64/run-firecracker.sh`'s `FC_RESTART=1` **cannot** be used as-is on this
  host: the guest's firecracker runs as **root** out of
  `/home/netoneko/akuma`, and the script finds the process with
  `pgrep -u $(id -un)` and `~/akuma`. As `netoneko` it misses the running VM and
  starts a **second** one on the same `disk.img` and `tap0`; as `root` it looks
  in `/root/akuma`.
* **Defect A on the metal.** Firecracker's disk is virtio-blk, not xHCI, so
  ryzen cannot exercise the xHCI assist at all. Only the HP box can. Success
  looks like this: a drive stall that prints `[xhci] transfer timeout` no longer
  brings `[TLB] stuck` or a `[BKL] stuck … tag=11` storm with it, and the box
  stays reachable over ssh through the stall.

### ryzen, by hand

```sh
# 1. sync the guest (key: target/x86_64-unknown-none/release/amd64-ssh-test-key)
ssh -i target/x86_64-unknown-none/release/amd64-ssh-test-key -o IdentitiesOnly=yes \
    -p 2222 root@192.168.1.50 sync
# 2. on ryzen, as root: save the RUNNING kernel under a new name, stop, snapshot
cd /home/netoneko/akuma
cp -p akuma-amd64 akuma-amd64.prev-20260925-running
kill -TERM $(pgrep -f 'firecracker.*akuma-vm.json'); sleep 5
cp --reflink=auto disk.img disk.img.bak-20260925-pre-ssfix && sync
# 3. from the Mac: stage the kernel
scp target/x86_64-unknown-none/release/akuma-amd64 ryzen:/home/netoneko/akuma/akuma-amd64
# 4. on ryzen, as root: relaunch the host's own launcher, detached
cd /home/netoneko/akuma && mv boot.log boot.log.prev-20260925
TIMEOUT=0 setsid nohup ./run.sh >/dev/null 2>&1 </dev/null &
# 5. from the Mac, once ssh answers: push and run the probe
K=target/x86_64-unknown-none/release/amd64-ssh-test-key
ssh -i $K -o IdentitiesOnly=yes -p 2222 root@192.168.1.50 \
    'cat > /tmp/trapprobe && chmod +x /tmp/trapprobe && /tmp/trapprobe; echo rc=$?' \
    < userspace/forktest/c_stress/x86_64/trapprobe
ssh -i $K -o IdentitiesOnly=yes -p 2222 root@192.168.1.50 'echo alive; ps'
```

Pass = rungs 2 and 4 **without** `NOT EXERCISED`, `rc=0`, `alive`, and kot
back under herd. Look for the `#SS` kill line itself on the host:
`grep -a 'Fault\] #SS' /home/netoneko/akuma/boot.log`.

## 6. Left open

* **virtio-blk is the same shape on Firecracker.** `virtio-drivers`'
  `read_blocks` busy-polls IRQ-masked inside `bkl_free_io` too, with no hook,
  because the wait loop is inside the crate. The host usually serves a request
  in microseconds, so it has not wedged, but a host-side I/O stall would
  reproduce defect A there.
* The ring-3 `#UD` and `#DB` no-handler kills still report status `-11`
  (`SIGSEGV`) rather than `SIGILL`/`SIGTRAP`. `user_fault` now takes the
  signal, so this is a one-argument change at each call. It was left alone
  because it changes what a parent's `waitpid` sees.
* The generated stubs that remain (`#NMI`, `#MC`, `#TS`, `#CP`, …) still do not
  `swapgs`, so a ring-3 origin prints a dump without the `core=` line.

## Background

* [`AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`](AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md) —
  the earlier `[TLB] stuck` wedge (a core that halted with `IF` clear), and why
  `wait_for_acks` stands down during a fatal dump.
* [`AKUMA_AMD64_USB_XHCI.md`](AKUMA_AMD64_USB_XHCI.md) — the driver's budgets
  and the stall-recovery ladder.
* [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) — the
  stale `thread_id` rows behind `[TRAMP-MISMATCH]`.
