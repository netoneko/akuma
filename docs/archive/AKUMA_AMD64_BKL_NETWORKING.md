# amd64: does networking take the BKL?

Stability grade: **C** (fresh static audit, 2026-09-19, no runtime measurement).

## Answer

**Yes, and more than the AArch64 kernel does for the same calls.** Every socket
syscall on amd64 — `socket`/`bind`/`listen`/`accept`/`connect`/`sendto`/
`recvfrom`/`sendmsg`/`recvmsg`/`setsockopt` — runs its **entire body**,
including the user-memory copy and the blocking wait for readiness, under the
BKL. This is not a `no-bkl-network`-style carve-out that was disabled; amd64's
syscall entry point has **no opt-out mechanism wired in at all** — the
per-syscall bitmap that lets AArch64 skip the entry acquire for exactly this
syscall family (`nr::SOCKET`, `nr::ACCEPT`, `nr::CONNECT`, …,
`crates/akuma-bkl/src/policy.rs` `SYSCALL_BKL_OPTOUT_SEED`) is never consulted
by `amd64/src/usermode.rs`. The one thing that *is* BKL-free, and deliberately
so, is the netpoll daemon's smoltcp drain (the packet-processing side) —
mirroring AArch64's carve-out there. The blocking **wait** inside a socket
syscall, however, uses the general-purpose `yield_now`, which only opens a
4-`pause`-cycle window per lap rather than dropping the lock for the halt —
the AArch64-side fix for exactly this problem (`blocking_relax_net` /
`idle_halt`) was never ported to this target's `NetRuntime` wiring.

## 1. Call path: syscall entry -> socket body (representative: `sendto`)

1. `amd64/src/usermode.rs:824` `extern "C" fn syscall_handler(nr, a1..a6)` —
   the x86_64 `syscall` trap lands here with `nr` as the **native x86_64**
   syscall number (confirmed by the trace arm at `usermode.rs:906` matching
   x86_64's `open=2`/`stat=4`/`lstat=6`/`access=21`/`chdir=80`, not
   AArch64/asm-generic's numbers).
2. `usermode.rs:837` — `crate::smp::bkl_enter()`, **unconditional**. No
   syscall-number check of any kind precedes it. Contrast AArch64's
   `crates/akuma-entry/src/smp_shared.rs:114-115`, which imports
   `akuma_bkl::policy::{set_syscall_bkl_optout, syscall_bkl_optout}` and
   consults the bitmap before deciding whether to take the lock at all.
   amd64 has neither the import nor a call site — confirmed by
   `grep -rln syscall_bkl_optout src amd64 crates`, which returns
   `src/process_tests.rs`, `crates/akuma-bkl/src/{policy,bkl}.rs`,
   `crates/akuma-exceptions/src/lib.rs`, `crates/akuma-entry/src/smp_shared.rs`
   — **nothing under `amd64/`**.
3. `usermode.rs:843` `akuma_bkl::sync::set_holder_tag(cpu, nr)` — attribution
   only, not a gate.
4. `usermode.rs:1038` `syscall_dispatch(nr, ..)` decodes `nr` (raw x86_64
   number) and, for `sendto` (x86_64 44), calls into
   `amd64/src/sock.rs:256` `sys_sendto`.
5. `sock.rs:256-303` — no BKL guard of any kind (`grep -n "NetBklGuard\|
   bkl_drop_window\|bkl_run_unlocked" amd64/src/sock.rs` returns nothing).
   The function decodes the destination address (`fd::socket_index`,
   `sockaddr_in_from_user` — a `uaccess::read_bytes` user copy), then for a
   UDP socket copies the payload (`fd::copy_in`, `sock.rs:294`, another user
   copy) and calls `akuma_net::socket::socket_send_udp`; for TCP it falls into
   `send()` (`sock.rs:218`), which does `fd::copy_in` then
   `akuma_net::socket::socket_send`. **All of this — both user copies and the
   call into the socket layer — runs under the BKL taken at step 2.**
6. `crates/akuma-net/src/socket.rs:1432` `socket_send` (and `:1472`
   `socket_recv`, `:1178` `socket_accept`, `:1244` `socket_connect`) call
   `wait_until` (`socket.rs:686`) when the socket is not immediately ready.
7. `usermode.rs:991` `crate::smp::bkl_leave()` on the way out (skipped only
   if the thread is exiting, `usermode.rs:965 leaving`).

So a `sendto`/`recvfrom`/`sendmsg`/`recvmsg` call is BKL-held from trap entry
to trap return, full stop — there is no drop anywhere in the amd64 socket
path itself.

### `epoll_wait` / `poll`

Not checked file-by-file to the same depth (out of the audit's time budget),
but the same mechanism applies: `syscall_handler` takes the BKL unconditionally
for **every** `nr`, and nothing under `amd64/src/` imports the opt-out bitmap,
so whatever `sys_epoll_wait`/`sys_ppoll` does is BKL-held for the same
structural reason — confirm by reading `amd64/src/fd.rs`'s poll/epoll arms
directly if this needs a second citation; the entry-point argument above does
not depend on which syscall it is.

## 2. What is held across what — the interesting part

`wait_until` (`crates/akuma-net/src/socket.rs:686-745`) is the loop every
blocking socket op parks in. Per lap: it polls smoltcp up to a budget
(`socket.rs:707-712`), checks the condition, and if not ready calls
`wait_park` (`socket.rs:777`), whose `Promiscuous` arm (the build's active
policy per `active_wait_policy`, `socket.rs:754` — no `net-direct-waker` or
`net-waker-park` feature is on for amd64 by default) calls
`(runtime().blocking_relax)()` (`socket.rs:792`).

amd64 wires `blocking_relax` to the **plain** `crate::sched::yield_now`
(`amd64/src/net.rs:265-273`, specifically line 273:
`blocking_relax: crate::sched::yield_now`) — the same function used for pipe
reads, `wait4`, and every other generic kernel wait loop. Its own comment at
`net.rs:270-272` states the design explicitly: *"drops the BKL for a moment so
the other cores' syscalls get in — which is exactly what 'relax while
blocked' has to mean under one lock."* "A moment" is
`amd64/src/smp.rs:395-411` `bkl_drop_window()`: release the lock, spin
`DROP_WINDOW_SPINS = 4` (`smp.rs:360`) `core::hint::spin_loop()` iterations,
**re-acquire it**, then (`sched.rs:2039` in `yield_now`) perform the actual
`threading::yield_now()` context switch **while holding the BKL again**. The
4-spin window only gives the lock to a peer core that happens to already be
spinning on the ticket at that instant (`smp.rs`'s comment: "the lock is
FIFO... the gap only matters for a peer that arrives during it"); it does not
release the lock for the duration of the park/switch itself.

Compare `crates/akuma-threading/src/lib.rs:4707` `blocking_relax_net` — the
function `akuma-kernel-glue` wires into AArch64's `NetRuntime::blocking_relax`
(`crates/akuma-kernel-glue/src/lib.rs:1590`) instead of the generic
`blocking_relax`. Under `kernel_smp_shared` it calls `idle_halt()` with **no**
preceding `yield_now`, specifically because (its own doc comment,
`lib.rs:4682-4699`) dropping the BKL and halting immediately, rather than
scheduling first, measured **+27% HTTP throughput and half the p90 latency**
(1,028 -> 1,307 req/s; p90 2,411us -> 1,166us; A/B 2026-08-20). amd64 has no
equivalent `blocking_relax_net` — it reuses the one-size-fits-all
`yield_now`, which is exactly the shape AArch64's own measurement showed to be
worse for sockets specifically (that A/B was about the presence/absence of a
scheduler pass before the halt, but the underlying point — the net wait
should behave differently from a generic kernel wait loop — was never carried
over to amd64's wiring).

Net effect: on amd64, a core blocked in `recv`/`accept`/`connect` holds the
BKL almost continuously for the whole wait, with brief (~4 `pause`-cycle)
release windows each lap that help only an already-spinning peer. Every other
core's syscall entry (which unconditionally wants the same lock, per §1) is
serialized behind this for as long as the wait lasts.

## 3. What runs BKL-free: the netpoll drain

`amd64/src/net.rs:542-590` `netpoll_daemon` — the daemon that calls
`smoltcp_net::poll()`/drains RX-TX — explicitly wraps only the drain call in a
dropped-BKL window: `net.rs:571` `akuma_bkl::bkl::dropped_window_open()`,
`net.rs:572` `drain_step()`, `net.rs:573` `dropped_window_close()`. The
comment at `net.rs:554-563` states this mirrors AArch64's
`netpoll_drain_step` carve-out and that it was necessary: *"at SMP=4 this
daemon's near-continuous BKL ownership starved every other core's syscall
entry into a `[BKL] stuck` storm (owner=1 tag=503) and sshd never
answered."* So the packet-processing/poll side of networking is BKL-free by
design and was fixed after being measured as a starvation source. The
socket-syscall side (§1-2) was not given the same treatment.

No dedicated NIC interrupt handler was found in `amd64/src/idt.rs` for the
virtio-net or `rtl8169` device (`grep -n "rtl8169\|virtio_net\|nic_irq"
amd64/src/idt.rs` — no hits); RX/TX servicing on this target appears to be
pure cooperative polling through `netpoll_daemon`, not a hardware-IRQ path.
This is an **inferred** conclusion from an absence of hits, not something
positively confirmed by reading a working IRQ table — worth a second look if
xHCI-style IRQ wiring for the NIC exists somewhere this grep missed.

## 4. The opt-out bitmap's numbering hazard — currently latent, not live

`crates/akuma-bkl/src/policy.rs`'s `SYSCALL_BKL_OPTOUT_SEED` is built from
`akuma_syscalls_linux::nr` constants — **AArch64/asm-generic** numbers
(`crates/akuma-syscalls-linux/src/nr.rs`: `SOCKET=198`, `ACCEPT=202`,
`CONNECT=203`, `SENDTO=206`, `RECVFROM=207`, `FUTEX=98`, `GETRANDOM=278`).
amd64's syscall numbers are a completely different table
(`crates/akuma-syscalls-abi/src/lib.rs`: x86_64 `SOCKET=41`, `CONNECT=42`,
`ACCEPT=43`, and — the concrete collision — x86_64 `FUTEX=202`, the exact
number that is AArch64's `ACCEPT`).

Since §1 established amd64's `syscall_handler` never calls
`akuma_bkl::policy::syscall_bkl_optout` at all, **this mismatch cannot
misfire today** — there is no live consultation of the bitmap on this target
to get wrong. But it is a loaded trap for whoever wires the mechanism up on
amd64 later without going through a translation: if `syscall_bkl_optout(nr)`
were called with amd64's raw x86_64 `nr` against the AArch64-seeded bitmap,
bit 202 (opted out because AArch64's `ACCEPT` is 202) would incorrectly
**opt x86_64's `futex` out of the BKL** — the one syscall the AArch64 side
notes (`policy.rs`'s tranche-3 comment) needed a specific IRQ-masking
prerequisite (`FUTEX_WAITERS`  going from a bare spinlock to one with masked
sites) before it was safe to convert. `getrandom` (aarch64 278, present in
the seed) has no x86_64 collision with anything else load-bearing at that
number in the table checked here, but the general shape of the hazard — "any
seed entry's aarch64 number could alias an unrelated, un-audited x86_64
syscall" — holds for every entry in the list, not just the one collision
found by inspection.

## 5. Confidence

**Read directly in the code, high confidence:**
- amd64 `syscall_handler` takes the BKL unconditionally, with no per-syscall
  check (`usermode.rs:824-991`).
- No file under `amd64/` references `syscall_bkl_optout`/`SYSCALL_BKL_OPTOUT`
  (grep, exhaustive over `amd64/`).
- `amd64/src/sock.rs` contains no BKL guard/drop of any kind (grep,
  exhaustive over the file).
- `blocking_relax` for amd64's `NetRuntime` is the plain `sched::yield_now`
  (`net.rs:273`), and `yield_now`'s `bkl_drop_window` releases the lock for
  only 4 spin iterations before re-acquiring it (`sched.rs:2008-2048`,
  `smp.rs:360,395-411`).
- The netpoll daemon's drain runs in an explicit dropped-BKL window
  (`net.rs:571-573`).
- The x86_64/AArch64 syscall-number collision at 202 (x86_64 `futex` vs.
  AArch64 `accept`) is a fact of the two static tables, independent of
  whether it is ever exercised.

**Inferred, not directly confirmed:**
- That `sys_epoll_wait`/`sys_ppoll`/`sys_pselect6` on amd64 follow the exact
  same BKL-held shape as the socket calls audited — inferred from the
  entry-point argument (§1) rather than read function-by-function in
  `amd64/src/fd.rs`.
- That there is genuinely no NIC hardware-interrupt path (only the polling
  daemon) — inferred from an absence of grep hits in `idt.rs`, not from
  tracing PCI/MSI setup to its end.
- The *magnitude* of the latency/throughput cost this design imposes on the
  bare-metal box specifically. Nothing here is a benchmark.

**What would settle the magnitude question** (do NOT run this against the HP
box while its build is in flight — use a QEMU/Firecracker amd64 rig instead,
or wait):
```
# A/B: does BKL hold time during a blocking socket wait matter at SMP>1?
# 1. Boot SMP=4 amd64 (QEMU q35 or Firecracker rig, not the HP box) with a
#    server (httpd/echo) plus a concurrent client hammering short
#    connect+send+recv cycles from several client threads/processes.
# 2. Read akuma_bkl::sync's existing `[BKL] stuck owner=N waiter=M tag=…`
#    counters/lines during the run — `tag=` will show the x86_64 syscall
#    number holding the lock; a socket-family number (41-55 range) appearing
#    repeatedly as `owner` while other cores show `waiter` is the direct
#    confirmation this doc predicts.
# 3. For the magnitude AArch64 already measured for its OWN equivalent
#    change, replicate the same shape here: wire a `blocking_relax_net`
#    (idle_halt, no yield_now first) into amd64's `NetRuntime::blocking_relax`
#    behind a feature/toggle, and A/B request rate + p90 latency against the
#    current `yield_now` wiring, same session, same client — the method
#    `crates/akuma-threading/src/lib.rs:4684` already used on AArch64.
```

## Background

- `docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md` — the phase plan the
  opt-out bitmap and every `no-bkl-*` toggle belong to.
- `docs/archive/BKL_PHASE7F_OPTOUT_LIST.md` — the per-syscall opt-out
  mechanism this doc found amd64 does not use.
- `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` — why x86_64 and
  AArch64 syscall numbers are two separate tables in the first place, and the
  x86_64-88/`symlink` mis-dispatch bug that came from conflating them once
  already.
- `docs/archive/AKUMA_NET_SPLIT.md` — the networking crate split
  (`akuma-net`/`akuma-net-nic`/`akuma-net-unix`/`akuma-net-yarn`) this audit
  read across.
- `docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` — records `[BKL]
  stuck` tags being raw x86_64 syscall numbers on this target, i.e. the same
  "tag = native nr, not asm-generic nr" fact this doc leans on in §1 and §4.
- MEMORY.md `amd64_smp_bkl_first.md` — amd64 SMP is BKL-first by design;
  this doc is about one specific consequence (the network family) of that
  choice, not a claim that BKL-first itself is wrong.
