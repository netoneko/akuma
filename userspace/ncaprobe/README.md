# ncaprobe — async-subprocess, epoll and terminal probes

A **std + musl + pthreads** guest binary that isolates Akuma-vs-Linux behaviour
differences on the exact runtime surface tokio, Go and hyper use. Sibling in
spirit to [`../nettest/`](../nettest/README.md), which does the same job for the
network stack; this one covers child processes, pipes, `epoll` readiness and the
tty.

Written for
[`docs/archive/TOKIO_PIPE_EPOLL_HANG.md`](../../docs/archive/TOKIO_PIPE_EPOLL_HANG.md)
— `nca` reporting `execute_bash — Command timed out after 30s` on every shell
tool call while the kernel log showed the child exiting in 50 ms. **Outcome
(2026-08-17): two kernel defects found and fixed** — `read()` on an
`O_NONBLOCK` pipe ignored the flag and blocked, and a pipe's `EPOLLET` `EPOLLIN`
edge could never be re-armed, so the EOF transition was swallowed and
`read_to_end` waited forever on a pipe already at EOF. The probe stays as the
regression harness. Procedure:
[`docs/runbooks/debug-async-subprocess-hang.md`](../../docs/runbooks/debug-async-subprocess-hang.md).

## Why not a libakuma probe

The rest of `userspace/` is `no_std` on `libakuma`, which exercises Akuma's own
`spawn`/channel syscalls — **not** the `pipe2` + `posix_spawn` + `epoll` +
`pidfd` path a real runtime takes. A libakuma probe would have passed while nca
hung. Going the other way, a whole app (nca, crush) tells you *that* something
hangs, never *which syscall lied*. This sits in between: 40–100 lines per
mechanism, on the real stack.

## The A/B is the point

The **same static binary** runs under Docker on real Linux, so every result is a
comparison rather than a judgement call:

```bash
cd userspace/ncaprobe/target/aarch64-unknown-linux-musl/release
docker run --rm --platform linux/arm64 -v "$PWD":/p:ro alpine /p/ncaprobe tokio
```

Passing on Linux and failing on Akuma is proof of a kernel defect in one
command, and it retires "maybe the app misuses the API" permanently. That single
step is what turned this investigation from a week of guessing into an
afternoon.

## Build and run

```bash
userspace/ncaprobe/build-musl.sh            # -> bootstrap/bin/ncaprobe
scripts/populate_disk.sh                    # ship it to /bin
```

For a VM that is already running — do **not** write `disk.img` under a live
QEMU, it corrupts the image:

```bash
userspace/ncaprobe/build-musl.sh --serve    # host, :8899
```
```bash
# guest
curl -s -o /tmp/ncaprobe http://10.0.2.2:8899/ncaprobe && chmod +x /tmp/ncaprobe
/tmp/ncaprobe tokio
```

## Subcommands

Each isolates one layer, so a pass eliminates that layer permanently:

| Subcommand | Question it answers |
|---|---|
| `tokio [--workers N] [--tui]` | Does `tokio::process::Command::output()` complete at all? The end-to-end repro. |
| `eofedge` | After draining a pipe with the writer still open, does the **EOF edge** ever arrive? The minimal form of the bug. |
| `ptyedge` | pty-shaped version of `eofedge`: after draining an initial byte, does a *second, later* `EPOLLET` edge on the pty's slave fd arrive after an idle gap? Written for the nca TUI input-freeze finding — see `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`'s "New finding 2026-08-18" section. |
| `epoll [main\|thread] [--late] [--zero]` | Raw `pipe`+`posix_spawn`+`epoll(ET)`+`pidfd`, on the main or a worker thread, registering before or after the child exits. |
| `cross` | One thread parked in `epoll_wait` while another `epoll_ctl(ADD)`s into it — tokio's shape. |
| `fds` | Which fds are open, which are epoll instances, and whether two fds alias one instance. |
| `waitid` | `waitid(P_PIDFD, …)`, tokio's reaping call, including the returned `siginfo`. |
| `pollbench` | Does a short `epoll_wait` **timeout** actually shorten the wait? Answers whether any poll-interval tuning knob can work at all. |
| `sleepbench` | What does a short `nanosleep` **actually** cost? A flat ~35 ms floor regardless of the request means every poll-loop in the system runs at ~27 Hz. |
| `termbench [--net]` | Latency distribution of writes to stdout — the SSH terminal path — optionally with a concurrent download. Stutter is the **tail**, so this reports p50/p90/p99/max and counts writes over 10 ms. |
| `pipebench [--epoll N]` | Per-iteration pipe write+read cost, optionally with the read end registered in N epoll instances (the re-arm walks every instance). |
| `raw [main\|thread\|split]` | `tcsetattr(raw)` + `read(0)`, set and read on the same thread or different ones. Needs a real tty and a keypress. |

Healthy output on a fixed kernel:

```
$ ncaprobe tokio
--- call 0: sh -lc "echo TOKIO_OUT"
    OK in 197ms status=Some(0) stdout="TOKIO_OUT\n" stderr=""

$ ncaprobe eofedge
   read = 6 "EARLY\n"
   read = EAGAIN -- back to epoll_wait for the EOF edge
round 2: READY events=IN|HUP (0x11)
   read = 0  EOF -- read_to_end can finish
```

`raw` is the one with an open question still behind it — see "Still open" in the
archive doc. It has never actually been run: it needs an interactive tty and a
human pressing ESC.

## Adding a probe

Keep them boring: one mechanism, no arguments beyond a mode, a timestamped line
per syscall and a single `RESULT:` line. A probe that needs interpretation will
be re-litigated. State the expected Linux behaviour in the doc comment so the
A/B is checkable by someone who has never seen the bug.

Not a member of `userspace/Cargo.toml` (that workspace is `aarch64-unknown-none`
`no_std`) and not built by `userspace/build.sh` — same arrangement as `nettest`,
built by its own script.
