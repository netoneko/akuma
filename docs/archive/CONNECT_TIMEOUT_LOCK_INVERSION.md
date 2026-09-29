# `poll()`'s connect-timeout sweep took `SOCKET_TABLE` under `NETWORK` — a two-core AB-BA deadlock that pegs both vCPUs silently

**Date:** 2026-09-29. **Branch:** `even-more-cats`.
**Found on:** the Ryzen 7 8845HS's live Firecracker guest (`akuma-vm.json`, 2 vCPU,
amd64 kernel `akuma-amd64` built 2026-09-27), caught mid-wedge.
**Status:** **root-caused from a live guest; fixed in the tree (uncommitted);
NOT live-validated.** The wedged ryzen guest was left untouched as evidence and the
race was **not reproduced** under QEMU (§7), so the fix is backed by the
disassembly-proven mechanism, host tests and a boot A/B — not by a before/after on
the trigger.

**One line:** `smoltcp_net::poll()` called `socket::mark_connect_timed_out` from
inside its `NETWORK` critical section; that function takes `SOCKET_TABLE`, which
every other path takes *first*. Two cores, one in each order, deadlock with IRQs
masked — no console output, no panic, no `[BKL] stuck` line, both vCPUs at 100%.

Reader's map: [`FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md`](FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md)
(the 2026-09-25 sighting this very likely explains — §8),
[`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) (the rule now
recorded there).

---

## 1. How it was found

Asked to look at "100% Firecracker spinning" on ryzen and on amd64 generally. The
live guest was already wedged when looked at (host-side, read-only):

```
fc_vcpu 0   99.9 %CPU   7:36 TIME+     (177182)
fc_vcpu 1   99.9 %CPU   7:37 TIME+     (177183)
firecracker  0.0 %CPU                  (177179, --no-api --config-file akuma-vm.json)
```

The console log (`boot.log`) had not been written since **05:30:22**, at guest uptime
1856 s. The host journal explains why:

```
05:30:31  systemd-sleep: Entering sleep state 'suspend'   (PM: suspend entry (s2idle))
16:08:08  PM: suspend exit                                (host asleep ~10.6 h)
```

So the guest did **not** hang at 05:30; the host was asleep. The 100% spin began at
**resume**, and the guest never recovered (still pegged 4 → 8 minutes later, no new
log bytes, disk untouched since 05:30:26).

The vCPU threads' accumulated CPU (283 s each) is *consistent* with that: a guest that
ran ~30 min mostly idle, slept 10.6 h with the host, then burned ~5 min.

## 2. Evidence: what the guest is doing

`/sys/kernel/debug/kvm/<pid>-<n>/` counters, sampled 10 s apart:

| counter | Δ over 10 s | reading |
|---|---|---|
| `halt_exits`, `io_exits`, `mmio_exits` | **0** | never HLTs, no I/O, no console writes |
| `irq_exits` | +22 654 | host interrupts only |
| `exits` | +62 630 | the rest is pause-loop exits |

A 2 s `tracefs` capture of `kvm:kvm_exit` (13 035 events): **every** exit is
`reason pause` or `reason interrupt`, and the exit RIPs collapse to four addresses —
two per vCPU, two bytes apart (`pause; movzbl` — a spin-wait test-and-test-and-set):

| vCPU | RIP | symbol (from `nm`/`objdump` of the deployed `akuma-amd64`) | waiting on |
|---|---|---|---|
| 0 | `0xffffffff80390d20/22` | `akuma_net::socket::mark_connect_timed_out` `+0x130` | `SOCKET_TABLE` (`0xffffffff805210d8`) |
| 1 | `0xffffffff80392f60/62` | `akuma_net::smoltcp_net::poll::with_network` (inlined into `listener_refresh`'s closure) `+0x150` | `NETWORK` (`0xffffffff80542910`) |

Both spin loops sit after a `cli` (`0xffffffff80390d0d`, `0xffffffff80392f42`, each
preceded by a `pushf`/`pop` saving the flags): the lock acquisition **masks IRQs on
both cores**, which is why no timer tick, no watchdog and no console line ever fires.

vCPU 1 is in `listener_refresh` → `with_table(…)` → `with_network(…)`: it **holds
`SOCKET_TABLE` and wants `NETWORK`**. vCPU 0 is inside `poll()`'s `NETWORK` section,
in `mark_connect_timed_out`: it **holds `NETWORK` and wants `SOCKET_TABLE`**. That is
the whole deadlock.

## 3. Root cause

The tree's lock order is **`SOCKET_TABLE` → `NETWORK`**. It is what `with_table`'s
own comment describes, what `listener_refresh` / `listener_backlog_census` /
`socket_can_recv_tcp` / the `SO_ERROR` path all do, and `poll()` says so itself:

```rust
// crates/akuma-net/src/smoltcp_net/poll.rs  (before)
// NETWORK lock is released here — safe to acquire SOCKET_TABLE.
// Acquiring SOCKET_TABLE while holding NETWORK causes AB-BA deadlock …
```

…but 90 lines above that comment, still inside the `NETWORK` section, the
`SynSent`-bounding sweep did:

```rust
if now_us.saturating_sub(started_at) > CONNECT_TIMEOUT_US {
    crate::socket::mark_connect_timed_out(handle);   // <- with_table(): takes SOCKET_TABLE
    net.sockets.get_mut::<tcp::Socket>(handle).abort();
    …
```

First appears in `14dc5603` (2026-08-20; `git log -S mark_connect_timed_out`), which
introduced the `connect_timed_out` flag so `SO_ERROR` could say `ETIMEDOUT`; carried
through the 2026-08-30 `smoltcp_net` submodule split (`1df4cf1f`). Live for 40 days.
Every other path was correct, and the comment describing the hazard sat in the same
function.

**Why the resume triggers it.** The sweep only fires for a `SynSent` socket older than
`CONNECT_TIMEOUT_US` (10 s). *Inference, not measured:* the guest's `uptime_us`
jumped by the length of the suspend (KVM guest TSC keeps counting across s2idle), so
**every** in-flight connect aged past 10 s at once, and the very next `poll()` —
while another core was in `listener_refresh` — took the inverted path. Which guest
sockets were mid-connect was not observed. The
guest-clock jump itself was not read out of the wedged guest.

It is **not** suspend-specific. Any connect that runs into its 10 s deadline on one
core while another core polls a listener can hit it; suspend just makes the
precondition (a timeout and a listener poll landing together) nearly certain.

**Why it was silent.** `NETWORK` and `SOCKET_TABLE` are plain spinlocks with no
stuck-lock diagnostic (unlike the BKL's `KernelLock`, which prints `[bkls>]` /
`[BKL] stuck`). Both cores spin with IRQs off, so nothing else runs to notice.
See §9(c).

## 4. The fix

`crates/akuma-net/src/smoltcp_net/poll.rs`, `crates/akuma-net/src/socket.rs`.

The sweep now only **collects** expired handles inside the `NETWORK` section and the
flag + abort are done after it, each lock taken alone:

1. **Inside `NETWORK`** — `sweep_connecting(&mut net.connecting, now_us, state_of)`
   retires entries that are no longer `SynSent` and moves those past the deadline
   into an `ExpiredConnects` (a fixed `[Option<SocketHandle>; 8]` on the stack). It
   is handed the list and a state probe **and nothing else**, so it cannot reach
   `SOCKET_TABLE` — the property is enforced by its signature, not by a comment.
2. **After `NETWORK` is released** (`expire_connects`): `mark_connect_timed_out` for
   each (`SOCKET_TABLE` only), then
3. `with_network` once to `abort()` each handle that is **still `SynSent`**, then
4. `clear_connect_timed_out` (new, `SOCKET_TABLE` only) for any handle that was *not*
   aborted.

Why not simply move the call after the abort: `abort()` makes the socket `Closed`, and
a reader that sees `Closed` with no flag reports `ECONNREFUSED` instead of
`ETIMEDOUT` — the distinction the flag exists to preserve. So the flag must be visible
**before** the abort. Between (2) and (3) the socket is still `SynSent`; if the
handshake completes in that gap, (3) leaves it alone and (4) un-flags it, so a
connection that made it is never left reporting `ETIMEDOUT`.

**Bounded, no allocation.** The batch is a stack array; overflow (more than 8 expiries
in one lap) leaves the surplus in `net.connecting` for the next lap — a timeout is
delayed by one poll, never dropped (`overflow_stays_tracked_for_the_next_lap`). The
`Vec` retain/`swap_remove` behaviour of `connecting` is unchanged. On the no-expiry
path (every poll) the added cost is a zero-length check.

**Known residual window.** Between (1) and (3) a handle could in principle be freed
and reused by another socket. That needs the user to close a `SynSent` socket *and* a
concurrent poll's GC to remove it *and* a new connect to take the slot, all in the
microseconds between two lock acquisitions; (3) re-validates the handle and requires
`SynSent`, so the worst case is one stray flag on a brand-new socket, cleared by (4)
unless that socket is also `SynSent`. Judged not worth a generation counter; stated
here so nobody has to rediscover it.

## 5. Audit: is this the only inversion?

Before fixing it, every path that could reach `SOCKET_TABLE` under `NETWORK` was
checked, since fixing one of two would leave the deadlock in place:

- `grep` of `smoltcp_net/*.rs` for `with_table|with_socket|SOCKET_TABLE|crate::socket::`:
  the only hits are `poll.rs:184` (the bug), `poll.rs:257` (the wake pass — *after*
  release, correct), and `udp_api.rs:40` (`listen_endpoint`, a pure function, called
  *before* `with_network`).
- A brace-matching scan of **all 63** `with_network(…)` closure bodies across
  `crates/akuma-net`, `crates/akuma-syscalls-glue`, `crates/akuma-syscalls-net` and
  `src/`, flagging any that mention `with_table`, `with_socket`,
  `mark_connect_timed_out`, `take_connect_timed_out`, `socket_can_*`,
  `listener_refresh`/`listener_ready`/`has_pending_connection`, `socket_add_waker`,
  `alloc_socket`, `remove_socket` or `SOCKET_TABLE`: **zero hits.**
- The other `NETWORK.lock()` sites are `init.rs` (boot, single-threaded) and
  `with_network` itself.

## 6. Tests

`crates/akuma-net` host tests: 51 → **55** (`smoltcp_net::poll::tests`), clippy clean
in the touched files. They drive `sweep_connecting` with real `SocketHandle`s minted
from a `SocketSet`:

| test | pins |
|---|---|
| `only_expired_synsent_entries_are_collected` | expired collected + removed, fresh one stays |
| `deadline_is_strictly_greater_than` | exactly-at-deadline is not expired (matches the old `>`) |
| `settled_and_dead_entries_are_retired_without_being_collected` | Established / Closed / dead handles are dropped and never reported as timeouts |
| `overflow_stays_tracked_for_the_next_lap` | cap of 8; surplus survives; every handle reported exactly once over two laps |

There is **no test that fails on the old code by deadlocking** — a host test cannot
cheaply stage two cores holding two `Spinlock`s in opposite order without hanging the
runner. The regression guard is structural (item 1 of §4: the sweep's signature has no
route to the socket table) plus the §5 audit, which is a one-off script, not a gate.

## 7. Verification actually done — and not

Done:
- `cargo test -p akuma-net --target <host>`: 55 pass. `cargo clippy … --all-targets`:
  no warnings in `poll.rs`/`socket.rs`; the remainder are pre-existing, in `tests.rs`.
- `cargo check -p akuma-net` for the workspace default (`aarch64-unknown-none`, also
  with `--features smp-shared`), and `cargo check/build -p akuma-amd64 --target
  x86_64-unknown-none --release`.
- **QEMU x86 boot A/B** (`-M microvm`, TCG, SMP=2, herd + sshd), OLD kernel (built
  before the edit, saved) vs NEW: both boot, get DHCP, start sshd, accept ssh with the
  tree's test key. A connect to a black-holed address
  (`wget -T 40 http://192.0.2.1:81/`) fails at **10.1 s on both**, which exercises the
  changed sweep on the NEW kernel (flag → abort path runs and the ssh session
  completes normally — no hang). Both report `Connection refused`, not `timed out` — see §9(b);
  that is **pre-existing**, identical on OLD.
- Idle host CPU of the QEMU process: ~1.7–1.9 % (busybox `sh` init), ~3 % (herd +
  sshd). No spin.

**Not done, deliberately stated:**
- **The deadlock was not reproduced on QEMU**, so there is no "OLD wedges, NEW doesn't"
  result. Two stress designs were tried on the OLD kernel (parallel looping black-hole
  `wget`s while the host hammered sshd): 12 loops overloaded the TCG guest until ssh
  handshakes timed out (idle CPU, alive console — overload, not the wedge signature,
  which is QEMU pegged at ~200 %); 4 loops ran clean but peak QEMU CPU was 1 %, i.e.
  the loops were probably not sustained, so it proves nothing either. Reaching the
  window needs a many-SYN_SENT generator inside the guest that doesn't spawn a
  process per attempt; not built.
- **Not validated on ryzen.** The wedged guest was not touched; the fixed kernel was
  not booted there, and the suspend/resume trigger was not replayed.
- **AArch64 was compiled, not booted.** The crate is shared, so the same bug and the
  same fix apply; only the amd64 kernel was actually run.

## 8. What this does to the 2026-09-25 sighting

[`FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md`](FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md)
(AArch64, Lima `vz` on a Mac) concluded the BKL wait *ended* within seconds and that
"something else, with no diagnostic on its path at all, is what's actually spinning".
That is precisely the shape found here: a wait on a plain spinlock with IRQs masked,
after the BKL wait had resolved. `akuma-net` is shared between the two kernels, so the
same inversion was live on AArch64. **This is now the leading hypothesis for that
sighting, not a proven cause** — there was no RIP capture, and a Mac sleeping (as this
ryzen did) is unverified for that run (the VM had run 2.5 h before going silent). If
it is a sleep/resume, the `[bkls>] … owner=2` lines just before silence could be the
freezer stalling one vCPU while it held the BKL rather than a livelock (on ryzen the
same line appeared right at the suspend), which would also explain why they look
"stuck" yet the wait ended.

The README triage row is updated accordingly, and the old doc carries a dated pointer
here. It is *not* marked fixed.

## 9. Noticed, not changed

a. **An abort doesn't ask for a wake pass.** `socket_state_changed` is `p1`'s result;
   the timeout `abort()` happens after `iface.poll()`, so a waiter on the timed-out
   socket is only woken if something else set the flag or its own backstop fires.
   Behaviour is unchanged from before (the old code aborted in the same spot); making
   `expire_connects` report "aborted ≥ 1" and OR it into `socket_state_changed` is a
   one-liner, left out to keep this change to the inversion.
b. **A blocking `connect` reports `ECONNREFUSED` on timeout, not `ETIMEDOUT`.**
   Identical on OLD and NEW at 10.1 s. The `SO_ERROR` path checks the flag first; the
   `wget` path evidently doesn't take that route. Not investigated.
c. **`NETWORK` and `SOCKET_TABLE` have no stuck-lock diagnostic.** That is why a
   textbook two-lock deadlock looked like an unexplained silent spin for a month. A
   bounded-spin report from the lock itself (holder site is already tracked for
   `NETWORK`: `NETWORK_HOLDER`, `NETWORK_LAST_SITE`) would have named it the first
   time. The same gap applies to the still-open
   [`POOL` gate wedge](../reference/subsystems/scheduler.md).
d. **The lock order is documented only in comments** (`with_table`, `poll()`), and one
   of them sat 90 lines from the violation. `locking.md` now states it as a rule.

## 10. How to catch the next one (no debugger needed)

This closes the tooling gap `FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md` §4 named
("Firecracker exposes no gdbstub"). On the *host*, as root, with the guest still
running:

```sh
P=<firecracker pid>; D=/sys/kernel/debug/kvm/$P-*/         # per-VM counters
top -H -b -n1 -p $P                                        # both fc_vcpu at ~100%?
cat $D/halt_exits $D/io_exits $D/exits; sleep 10; …        # Δ: HLT/IO zero = pure spin
T=/sys/kernel/tracing; echo 1 > $T/events/kvm/kvm_exit/enable; echo 1 > $T/tracing_on
sleep 2; echo 0 > $T/tracing_on; echo 0 > $T/events/kvm/kvm_exit/enable
grep kvm_exit $T/trace | grep -o 'rip 0x[0-9a-f]*' | sort | uniq -c | sort -rn | head
nm -n <the kernel ELF the guest booted> | …                # symbolize; objdump -d around it
```

`reason pause` at a stable handful of RIPs = a spinlock wait; the RIPs are the waiters,
so **two vCPUs at two different lock addresses is a lock cycle**. Also check
`journalctl -b | grep -i suspend` first — a host that slept explains "silent since
HH:MM" before any guest analysis.

**Don't raise `buffer_size_kb`.** It is *per CPU*: the 65536 used here on a 16-CPU
host allocated ~1 GiB of kernel memory (`buffer_total_size_kb` 1 048 608) on a box with
~4 GB free, and stayed allocated. The default (~1410 KB/CPU, ~22 MB total) should hold a 1–2 s
`kvm_exit` capture from a 2-vCPU guest (13 k events, only two host CPUs active) —
estimated, not measured; capture shorter rather than growing the buffer. If you must resize,
read `buffer_size_kb` first and put it back, and delete any `nm`/trace output you
leave in `/tmp`. (Reverted on ryzen 2026-09-29 after the user asked; it had been left
raised for a while first.)

## Background

- [`FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md`](FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md) —
  the 2026-09-25 AArch64 sighting; §8 above.
- [`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md) —
  "The `POOL` gate", the other "alive but unscheduled" wedge; same missing-diagnostic
  gap (§9c).
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — the
  lock-order rule added by this change.
- [`AKUMA_AMD64_BKL_NETWORKING.md`](AKUMA_AMD64_BKL_NETWORKING.md) — amd64
  networking/BKL background (not re-read for this change; listed for context).
- `docs/runbooks/selfhost-kernel-build-amd64.md` § "Second rig: ryzen" — the same
  host, different (fresh-boot) guest; its unexplained per-run `[BKL] stuck … tag=11`
  lines are a different matter and untouched here.
