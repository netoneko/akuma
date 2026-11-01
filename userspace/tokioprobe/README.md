# tokioprobe

The smallest check that tokio works on a kernel, instead of running goose. One static musl binary, a multi-thread tokio
runtime, two steps:

| step | what it does | what it exercises in the kernel |
|---|---|---|
| `RUNTIME` | builds the multi-thread runtime | `epoll_create1`, `eventfd2`, `sched_getaffinity`, `clone` (worker threads), futex |
| `UNAME` | `uname -a` through `tokio::process` | `pipe2`, `clone`/`execve`, `wait4`, SIGCHLD delivered to tokio's signal driver |
| `HTTPS` | GET `https://akuma.sh/` with reqwest + rustls | DNS on the blocking pool, non-blocking `connect`, `EPOLLET` readiness, the eventfd waker, TLS, timers |

Every step is bounded by a timeout (default 30 s), so a hang prints `FAIL ... timed out` rather than wedging the probe.

## Build

```bash
./build-musl.sh                 # aarch64 -> bootstrap/bin/tokioprobe
ARCH=x86_64 ./build-musl.sh     # amd64 (Taoxichuan, the amd64 kernel) -> bootstrap/bin/tokioprobe.x86_64
./build-musl.sh --serve         # also serve it on :8899 for a live VM
```

Needs `aarch64-linux-musl-gcc` / `x86_64-linux-musl-gcc` (`brew install FiloSottile/musl-cross/musl-cross`) and the
matching rust musl target. The dependencies are the ones the repo's other tokio probes already use; `Cargo.lock` started
as a copy of `nettest/rust/reqwest/Cargo.lock`. The build is `--locked`.

## Run

```text
$ tokioprobe
RUNTIME PASS multi-thread workers=4
UNAME PASS Linux taoxichuan 6.6.0-akuma #1 SMP ... x86_64 Linux (0.02s)
HTTPS PASS https://akuma.sh/ status=200 HTTP/1.1 bytes=20595 (1.31s)
  title: AKUMA // SYSTEM ONLINE
RESULT PASS
```

(Illustrative output; the shape is fixed, the values are not.) The exit status is 0 on `RESULT PASS`, 1 otherwise.

- `tokioprobe <url>` fetches another URL; `--body` prints the whole body instead of the `<title>`.
- `TOKIOPROBE_TIMEOUT=<secs>` changes the per-step timeout; `TOKIOPROBE_WORKERS=<n>` the worker count (`1` is the
  single-worker case where the eventfd waker is the only wake path, `TAOXICHUAN_TOKIO_MULTI_EPOLL_FINDING.md`).
- The guest needs `uname` on `PATH` (busybox is enough) and a resolver in `/etc/resolv.conf`. No CA bundle is needed:
  rustls uses the bundled webpki roots.

## Background

- `docs/archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`: the tokio syscall surface this probe touches.
- `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (2026-10-30, "goose + GLM on Taoxichuan - blocked: no
  epoll"): why a small probe beats running goose.
- `userspace/nettest/rust/reqwest` (nca's exact network stack, many modes) and `userspace/ncaprobe` (async subprocess,
  epoll, tty edge cases): the larger probes this one sits beside.
