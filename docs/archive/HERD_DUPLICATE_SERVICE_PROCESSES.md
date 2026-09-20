# `herd` starts the same service again while it is still running

**Status: OPEN**, found 2026-09-20 on both amd64 targets (bare-metal trashcan and
the Ryzen Firecracker guest) while standing up a one-agent litter.
`userspace/herd/src/main.rs`.

## Symptom

One enabled service, several live processes, and the count grows:

```
PID   USER     TIME  COMMAND
    1 0         0:00 /bin/herd
   19 0         0:11 /bin/llama-server -m /models/smollm2-135m.gguf --host 127.0.0.1 --port 8081 ...
   20 0         0:00 /bin/meow-live litter live
   22 0         2:40 /bin/meow-live litter live
   24 0         0:00 /bin/llama-server ...
   25 0         4:00 /bin/llama-server ...
   26 0         0:00 /bin/llama-server ...
   27 0         0:00 /bin/llama-server ...
   28 0         0:00 /bin/llama-server ...
```

Two units enabled (`llama`, `sherlock`); five `llama-server` and two
`meow-live`. One of each has real CPU time and is doing the work; the rest sit
at `0:00`.

**They are separate processes, not threads, and not zombies.** Checked, because
`ps` alone cannot tell those apart here:

```
pid 19: Name: llama-server  Tgid: 19  Threads: 1
pid 24: Name: llama-server  Tgid: 24  Threads: 1
pid 25: Name: llama-server  Tgid: 25  Threads: 1     <- the one with 4:00 of CPU
...
pid 19: State: R (running)   (all of them)
```

Distinct `Tgid`s, all `State: R`.

Why it matters beyond waste: for an agent, two processes sharing one
`MEOW_HOME` means **one name, one signing key and two racers for the same hub
bind** — the litter's whole leader election is "whoever holds the socket", so a
duplicate is a second claimant to the same identity.

## Two measurement traps hit while diagnosing this

Both cost time here and will cost it again:

* **`Threads:` in `/proc/<pid>/status` is not trustworthy on this kernel.** It
  reads `1` for a `meow litter live` whose raft thread is demonstrably running
  (its hub answers), and `1` for `llama-server`. An earlier conclusion in this
  session — "panther has no raft thread" — was drawn from that field and was
  **wrong**; what settled it was making meow's raft thread flip a flag the
  parent waits on (`RAFT_ALIVE`, `src/tools/litter/live.rs`), which reported the
  thread up.
* **`dmesg` could not confirm herd's own logging.** herd prints
  `[herd] Service <name> exited with code N` before every restart, and the ring
  had already been filled by the boot suite, so `dmesg | grep Service` came back
  empty — which proves nothing either way. `dmesg -c > /dev/null` before the
  window you care about, or boot `skiptests`
  (`docs/runbooks/amd64-bare-metal-loop.md`).

So the mechanism below is a **hypothesis with the evidence available**, not a
confirmed trace.

## Where to look

`check_process_exits` (`main.rs`) polls `waitpid_status(pid)` for every service
in `Running`, and treats **any** `Some(status)` as "it exited", then restarts
per policy. `libakuma::waitpid_status` maps the Akuma-native `WAITPID` (303)
as:

```rust
if result == 0 { None }                 // child still running
else if (result as i64) < 0 { None }    // error / no such child
else { Some(WaitStatus { pid: result as u32, raw: status }) }
```

So **any positive return is read as an exit**. If the kernel's `WAITPID` ever
answers with a positive value for a live child, herd restarts a service that
never died, and does it again on the next pass — which is the growth seen above.
That is the first thing to check, from the kernel side, with a service that is
known to be alive.

A second candidate, not exclusive with the first: `start_stopped_services` is
called twice per supervisor pass (once directly, once after `reload_config`),
and `reload_config` re-parses every unit every 20 s. A service whose state is
not `Running` at the instant either call runs gets started. `start_delay_ms`
made this reliably reproducible — a unit carrying it produced two live agents on
the guest every time, and dropping it fixed *that* case — but duplicates appeared
later **without** `start_delay_ms`, so the delay is an amplifier, not the cause.

## Two smaller herd gaps found alongside

* **No working directory.** `workdir` was added for this session's llama
  service and cannot be used: it has to go through `SPAWN_EXT`, and that call
  triple-faults the kernel for an unboxed service. Written up separately in
  [`AMD64_SPAWN_EXT_WORKDIR_CRASH.md`](AMD64_SPAWN_EXT_WORKDIR_CRASH.md).
* **`args` has no quoting.** `config.args` is `value.split_whitespace()`, so a
  command string cannot be passed as one argument — `sh -c "cd /tmp && exec ..."`
  arrives as four separate argv entries. Anything needing a composed command has
  to go in a script file.
* **stdout capture for services started by a config reload.** `sshd.log` exists
  and is written; a service enabled at runtime produced **no** log file at all
  while its process ran. Whether herd registers the stdout fd for polling on the
  reload path, or whether the child simply wrote to stderr, was not established —
  but a service whose output goes nowhere is hard to debug, which is how the
  llama working-directory bug stayed invisible until `[PSTATS]` was read instead.

## Reproducing

```sh
# on the box, with herd as pid 1
userspace/meow/litter/herd_akuma.sh start sherlock   # writes units, enables them
# wait past one 20 s config reload, then:
ps | grep -c llama-server
for p in $(ps | grep llama-server | grep -v grep | awk '{print $1}'); do
    grep -E '^(Tgid|State)' /proc/$p/status; done
```

Distinct `Tgid`s with `State: R` is the bug. Same `Tgid` would be threads, and a
`State: Z` row is the *other* known issue — nothing reaps an orphan on this
target, see [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md)
appendix.

## Background

* `userspace/meow/litter/herd_akuma.sh` — why the litter runs under herd at all
  (an agent started from an ssh session is orphaned and never reaped).
* [`HERD_PLUS_BOX.md`](HERD_PLUS_BOX.md) — herd's service model and the box
  spawn path that does *not* show this.
