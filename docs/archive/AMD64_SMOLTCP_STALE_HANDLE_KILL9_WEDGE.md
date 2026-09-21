# The `kill -9` wedge on the amd64 box: a stale smoltcp socket handle, not the kill path

**Written:** 2026-09-21. Root cause of the three wedges recorded in
`userspace/meow/docs/LITTER_EXPERIMENT_PHASE_4.md` §4/§4b, whose console photo
caught the panic: `[PANIC] … smoltcp … handle does not refer to a valid socket`.

**Status: root-caused and fixed** (guarded accessors in `akuma-net`, same day).
Every host test, the QEMU/TCG trial suite, and a 10-round kill -9 repro under
QEMU are green — see Verification. The bare-metal box has **not** run the fixed
kernel yet; until it has, treat "wedge is dead on the metal" as expected rather
than confirmed.

## Symptom

`kill -9` on a `/bin/meow litter live` process on the trashcan reliably took
the box fully dark within seconds — no ssh, no ICMP, recovery only by physical
power cycle. Once, the same darkness arrived with no kill at all. `kill -9`
itself was never the problem: the victim died correctly (zombie, `wait4`
returns 137). The kill merely *triggered* the cleanup path that dereferenced
the stale handle — which is why a wedge needed no kill on the third occurrence.

## Mechanism

A `SocketHandle` is copied out of the `SOCKET_TABLE` entry and then used raw:

1. Thread A parks in `wait_until` (blocked `connect`/`recv`/`send`/`accept`),
   its predicate holding the bare handle: `net.sockets.get::<tcp::Socket>(h)`.
2. `kill -9` (or any close of a shared fd from a sibling thread) runs
   `remove_socket(idx)` → `socket_close(h)` → TCP `Closed`, handle queued in
   `pending_removal`. meow is two-threaded (raft owner thread) with a shared
   fd table, so this lands while A is parked or preempted.
3. The next `poll()` GC pass reaches `Closed` and `sockets.remove(h)`. The
   handle is now dead but thread A still holds it.
4. Thread A's predicate runs once more → smoltcp's `SocketSet::get` **panics**.
   On bare metal a panic halts the core: the dark-box signature.

The single-threaded shape of the same class — a non-blocking connect closed
before it establishes, leaving a dead handle in `net.connecting` — was already
fixed as `purge_connecting` (2026-08-30, `lifecycle.rs`), and `poll()`'s two
sweeps plus the async `tcp_connect` guarded themselves with
`is_valid_handle`. **The blocking syscall paths in `socket.rs` and all of
`udp_api.rs` did not** — ~20 raw `get`/`get_mut` sites, several of them
written to expect `None` for a dead handle (`.unwrap_or(...)`,
`matches!(state, Some(...))`), which smoltcp's `get` never returns: it panics.

## The fix

Four guarded accessors in `smoltcp_net/stream.rs`, next to
`is_valid_handle`:

- `tcp_get` / `tcp_get_mut` / `udp_get` / `udp_get_mut` — membership test,
  then project; `None` when the handle is dead. `'s` (reference) and `'b`
  (buffer) lifetimes are split so both closures and the invariance of
  `&mut SocketSet<'static>` typecheck.

Every raw handle dereference in `socket.rs` and `udp_api.rs` now goes through
them; the pre-existing `None` fallbacks (`.unwrap_or(true)` on wait
predicates — a dead handle stops the wait — `ENETDOWN`/`EAGAIN` elsewhere)
absorb the `None`. `register_socket_waker` returns `false` for a dead handle
so the waiter falls back to its backstop park; `udp_socket_close` gained the
guard `socket_close` already had. Five host tests in
`crates/akuma-net/src/tests.rs` (`guarded_handle_tests`) pin the contract,
including the wedge's exact sequence: mint, remove, dereference.

## Verification

| gate | result |
|---|---|
| host `cargo test` (all crates) | 1468 passed, 0 failed (5 new) |
| clippy, `akuma-net` | clean |
| `amd64_trials.py --local-only --smp 4` | 740 passed, 1 failed — the pre-existing `mmap: the lazy path was actually taken` baseline failure (§8.4 of `AKUMA_AMD64_NO_SLOT_RECYCLER.md`), identical on the unfixed tree |
| QEMU/TCG `microvm`, SMP=4, sshd + meow on a real rootfs: 10 rounds of spawn `/bin/meow litter live` (two threads, hub socket) + `kill -9` from a second session | every round survived, ssh answered throughout, zero `CORRUPT HANDLE` / `[PANIC]` in dmesg |

Two traps cost time in that last gate, both already on record elsewhere and
worth repeating where they bit: `amd64/run.sh` **regenerates its default disk
every run**, so anything injected with `debugfs` must go onto a `DISK=` image
named explicitly; and a `dmesg | grep` over ssh counts **its own argv** —
`sshd` logs the executed command into the ring the runbook's bracket trick
(`grep 'PANIC\[?\]'`-style) exists for.

## Not this bug (resolved on the way)

- **`kill -9` delivery** — works. The victim goes zombie; a `wait4` from a
  real parent returns 137 promptly (`sh -c 'sleep & kill -9 $p; wait'` →
  `wait-rc=137`). The earlier reading of a hung `wait` during this
  investigation was the wedge panicking the box mid-test, not a wait4 defect.
- **The zombie that lingers after killing herd's meow** — that is the
  init-doesn't-reap gap already recorded in the Appendix of
  `AKUMA_AMD64_NO_SLOT_RECYCLER.md` (pid 1 never `wait4`s; only a parent's
  wait or a reboot removes the entry). Unchanged by this fix.

## Background

- [`userspace/meow/docs/LITTER_EXPERIMENT_PHASE_4.md`](../../userspace/meow/docs/LITTER_EXPERIMENT_PHASE_4.md)
  §4/§4b — the wedges, the console photo, and the "do not kill -9 meow" warning
  this doc retires.
- [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) — the
  reaper whose merge made the box bootable through this class of teardown, and
  whose Appendix documents the unrelated zombie gap.
- `NGINX_MISSING_SYSCALLS.md`, `AKUMA_NET_ISSUES.md` — earlier members of the
  same family: raw-handle/`transmute`-index dereferences into the `SocketSet`,
  fixed one at a time since 2026-08-30. This is the last class that reached a
  panic through the blocking syscall paths.
