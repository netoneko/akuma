# Taoxichuan: tokio's multi-thread runtime only makes progress when the timeout fires (found 2026-10-31, on TCG)

**Symptom.** `nettest-reqwest` (tokio + hyper + reqwest + rustls; `userspace/nettest/rust/reqwest`) against the host delay server
(`scripts/net_delay_server.py`, guest -> `10.0.2.2:18080`) completes a GET of a 3-byte body at **exactly the client timeout**:

| runtime | `NETTEST_TIMEOUT` | first byte at |
|---|---|---|
| multi-thread (default) | 5 s | 4,989 ms |
| multi-thread | 9 s | 8,959 ms |
| multi-thread | 25 s | 24,968 ms (a POST: 24,972 ms; the delay sweep fails every row at 25,000 ms, even `/delay/1`) |
| `NETTEST_RT=current` | 5 s | **29 ms** |

The current-thread runtime is fast, so networking, TLS-less HTTP, DNS-less connect and the stack are fine. In the multi-thread runtime one worker is parked
in `epoll_wait` while another thread connects, writes and waits.

**What the kernel does meanwhile.** A trace inside `epoll_wait` (stage-12 `epoll.rs`, `[tmp] epw park`) shows the lap loop doing exactly what it should - a
10 ms park (`dl-now=10000`) every lap, `netpoll()` first, then `scan_into` - and **`reported=0` on every lap for the whole timeout**. The park is not the
problem; the readiness scan never reports the socket (connect completion `EPOLLOUT`, then `EPOLLIN` for the reply) until the timer wake makes tokio poll the socket
directly. Candidates, in order: the `EPOLLOUT` edge of a non-blocking `connect` under `EPOLLET` is never produced (armed-state/`last_ready`), the interest entry
registered from a different thread than the waiter is not in the snapshot the waiter scans, or `fd_state` for a connecting/connected TCP socket answers not-ready
because `netpoll()` returned early (its `IN_LAP` guard) while another thread's lap was in flight.

**Reproduce (TCG, no ryzen).** Build the probe: `ARCH=x86_64 userspace/nettest/rust/build-musl.sh reqwest`. Put it in the fork-sshd test image as
`/bin/nettest-reqwest` with `scripts/kvm/goose/tokio_probe_guest.sh` as `/etc/tp.sh`, run `scripts/net_delay_server.py --port 18080` on the Mac and boot
`init=/bin/busybox initargs=sh,/etc/tp.sh` (the previous session's `runqemu_new.sh` rig). On the ryzen the same probe works through the host relay against any
reachable HTTP URL (`nettest-reqwest get http://example.com/`).

**Status.** Open at the time of writing; handed to glm (stage 12). Not the same as the `goose run` spin (one vCPU at 100%, no output), which may be this bug
seen from a worker that waits with an infinite timeout - the probe has a timeout, goose does not.
