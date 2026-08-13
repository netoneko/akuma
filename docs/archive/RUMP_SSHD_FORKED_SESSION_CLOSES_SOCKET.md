# rump devbox: ssh reset at kex — the parent's post-fork `close` destroyed the child's socket

**Status: FIXED, 2026-08-13.** `overlays/devbox/run.sh` (rump devbox, `RUMP_NIC=1`,
host `:2223`) now serves ssh: 10/10 sequential sessions, an interactive PTY
session, and `curl http://example.com/` → `200` through the rump stack.

Supersedes the diagnosis in
[`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) **Issue 10**, whose lead was wrong — see
"The lead that was wrong" below.

## Symptom

```
$ ssh -p 2223 root@localhost uname -a
kex_exchange_identification: read: Connection reset by peer
```

Everything up to that point looked healthy: `rump_server` alive and busy, sshd in
a normal non-blocking accept loop, no `PANIC`, no `[WILD-DA]`, no SIGSEGV.

## The lead that was wrong

Issue 10 concluded *"there is no rump DHCP lease in the log… so the suspicion is
the rump DHCP path, not sshd"*, because `run.sh` tells you to wait for a lease
and no lease line ever appears on the console.

**`rump_server` does not log to the console.** It logs to
`/var/log/box/0/rump_server.log` inside the image — the path is passed on its own
command line in `rump_proxy::start_default_stack`
(`--log /var/log/box/0/rump_server.log`). Reading it shows DHCP working perfectly:

```
$ docker run --rm --privileged -v "$PWD/devbox.img:/disk.img" alpine:latest \
    sh -c 'mkdir -p /mnt/d && mount -o loop /disk.img /mnt/d &&
           cat /mnt/d/var/log/box/0/rump_server.log'
...
virt0: Ethernet address b2:0a:87:0b:0e:00
RUMP_SERVER: ifcreate virt0 -> 0
dhcp: virt0: adding IP address 10.0.2.15/24
dhcp: virt0: adding route to 10.0.2.0/24
dhcp: virt0: adding default route via 10.0.2.2
lease time: 86400 seconds
RUMP_SERVER: dhcp_ipv4_oneshot virt0 -> 0
RUMP_SERVER: rumpuser_sp_init_fd(3) -> 0
RUMP_SERVER: SERVING sysproxy on fd 3 (net=up)
```

The absence of a console lease line is a **logging destination**, not a network
failure. Read that file before suspecting rump networking; `run.sh`'s "once you
see the rump DHCP lease" instruction describes something the console never prints.

## Root cause

**Rump sockets were the one refcounted fd family missing from
`SharedFdTable::clone_deep_for_fork`, so a forked child's descriptor was a bare
alias and the parent's `close` tore the socket down in `rump_server`.**

`clone_deep_for_fork` (`crates/akuma-exec/src/process/fd.rs`) takes a reference
per inherited descriptor for `PipeWrite`, `PipeRead`, `UnixSocket`, `EventFd` and
`Socket` — and fell through to `_ => {}` for `RumpSocket`. `proxy_close`
(`src/rump_proxy.rs`) then unconditionally sent a real NetBSD `close(rump_fd)` to
the box's `rump_server`.

`userspace/sshd` is process-per-session (`fork-sessions`), and its accept loop
does exactly the thing that breaks on a non-refcounted fd:

```rust
Ok(ForkResult::Parent(pid)) => {
    live_sessions += 1;
    // "It is refcounted, so this does not disturb the child"
    drop(stream);
}
```

That assumption holds for native sockets and did not hold for rump ones. So on
every connection: sshd accepted, forked the session child, dropped its own copy —
and the socket the child was about to speak SSH over was destroyed inside
`rump_server` before the banner exchange. The child then exited cleanly (no
fault, which is why nothing looked wrong in the kernel log) and the client saw a
reset at kex.

The kernel log says as much once you know what to look for — one fork, one quick
clean exit, no fault:

```
[AS-NEW] pid=4 l0=0x68b91000 asid=0x4 via=fork parent=3
[TERM] tid=9 pid=Some(4) by_tid=9 state=2 pending_kill=false at=src/syscall/proc.rs:291
[AS-FREE] l0=0x68b91000 asid=0x4 path=owner core=0
```

## Fix

A reference count for rump sockets, mirroring what the native stack keeps inside
the socket object. A rump socket has no kernel-side object to hang a count on —
it lives in `rump_server` and the kernel holds only an integer — so the count
lives in `rump_proxy` as `RUMP_FD_REFS: Spinlock<BTreeMap<(u64, i32), u32>>`.

- Keyed **`(box_id, rump_fd)`**, not `rump_fd`: each box has its own
  `rump_server` and two servers hand out the same small integers.
- `FileDescriptor::RumpSocket` therefore carries `box_id`, because
  `clone_deep_for_fork` sees descriptors, not the process they belong to, and has
  no other way to learn which server an fd is on.
- New `ExecRuntime` hook `rump_socket_clone_ref(box_id, rump_fd)` (no-op when the
  `rump` feature is off — that build can never construct the variant).
- `proxy_close` drops a reference and forwards the NetBSD `close` **only** when it
  was the last. An untracked pair (a socket predating tracking) still closes,
  so this cannot leak rump fds.

## Verify

```bash
scripts/build_devbox.sh
RUMP_SSH_PORT=2223 overlays/devbox/run.sh 2>&1 | tee rump.log
until grep -aq "Started sshd" rump.log; do sleep 3; done
```

Then, via Python (the `ssh` CLI is blocked by policy — see CLAUDE.md), expect
every one of these to succeed:

- 10 sequential `ssh -p 2223 root@localhost echo sN` runs — sequential runs are
  the point: a refcount that leaked would exhaust rump fds after a handful.
- `ssh -tt -p 2223 root@localhost` interactive, `echo` then `exit`.
- `ssh -p 2223 root@localhost 'curl -s -o /dev/null -w %{http_code} http://example.com/'`
  → `200`, which exercises socket create/connect/close on the same path.

Boot-suite guard: `[PASS] test_rump_fd_ref_survives_fork` (`src/rump_tests.rs`)
drives the exact accept → fork → parent-close → child-close sequence and asserts
the parent's close is *not* last and the child's *is*, plus per-box independence
and the untracked-pair case.

## Notes

- This is unrelated to
  [`EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`](EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md),
  fixed the same day, which also presented as "ssh sessions die instantly" — that
  one is a kernel-stack overrun on the `extreme-size` profile. Same symptom
  sentence, entirely different cause. Check which profile you are on first.
- Both bugs date from the same architectural change: sshd moving to userspace with
  process-per-session. Anything that assumed "one long-lived server process" is
  worth re-examining against `fork`.

## Background

- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 10 — the original report and its
  A/B establishing the fault predates the `akuma-virtio` extraction (correct;
  it predates it by however long `fork-sessions` has been on).
- `userspace/sshd/docs/OPTIONAL_PARALLELISM.md` — the process-per-session design
  and the refcount assumption it rests on.
- `src/rump_proxy.rs` — `intercept_box_syscall`'s "HARD ISOLATION GUARANTEE"
  choke point, and the `socketpair`/`recvfrom` precedent for "check the fd, not
  just the syscall number".
- `crates/akuma-exec/src/process/fd.rs` — the fd-family refcount table this
  bug was a hole in.
