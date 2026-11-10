# sshd: a client that vanished left its shell running for ~2 minutes (2026-11-10)

**Symptom.** A client killed or dropped mid-command: the server's shell (and the `ssh` the user thought was gone)
lingered. Found while verifying the first teardown/reap change; measured on akuma-ryzen (amd64), `sshd`
(cooperative+fork-sessions build) inside a `box`, `--idle-timeout-secs 6`.

**Cause.** `bridge_process` read a TCP EOF (`try_read == Ok(0)`) as "client finished sending" and set `client_done`,
which also *stopped reading the socket*. After that nothing could end the session except the idle watch: a RST provoked
by the keepalive probe was never read, and a live client's probe replies were never counted as activity. A dead client
therefore lived until `idle_timeout + DEFAULT_IDLE_PROBE_GRACE_MS` (30 + 90 s by default; 6 + 90 measured), while a
*healthy* `ssh host longcmd </dev/null` (CHANNEL_EOF sent, still answering probes) was killed at the same point.
OpenSSH treats a bare TCP EOF as the peer being gone. The Akuma client reports "done sending" with `CHANNEL_EOF`, never
a half-close.

**Fix** (`userspace/sshd/src/protocol.rs`):
- TCP EOF now tears the session down at once (`teardown_child`), before and after `CHANNEL_EOF`.
- After `CHANNEL_EOF` the socket is still polled: a reset ends the session, inbound bytes (probe replies) count as
  activity and are decrypted-and-dropped to keep the cipher sequence in step.
- `teardown_child` SIGKILLs the shell, `kill(-pgid)` (served by the amd64 kernel), every `/proc` descendant
  (`session_policy::descendants`/`parse_ppid`, host-tested), closes the fds and **reaps** the shell (bounded `waitpid`).
  Any error out of the bridge does the same instead of orphaning the child.
- Client (`client/protocol.rs`): keepalive 15 s x 4 -> 5 s x 3 (`client_resilience::DEFAULT_ALIVE_*`).

**Verified** (box on this machine; the KVM route needs a `kvmctl` boot, which this boot was not): kill -9 of the client
mid-`sleep 300`: shell gone in 92 s before, **under 1 s** after, session process exited, no zombie. Healthy paths: plain
exec, piped stdin (`cat`), exit status 7, 800 KB of output intact, and a 110 s command with stdin at EOF **survives**
(it was killed at ~96 s without the poll after `CHANNEL_EOF`).

**Not verified live.** Descendant kill and `kill(-pgid)`: a box root has no `/proc` and sshd does not `setsid` its
children, so the grandchild (`sleep`) survived the test; only the unit tests cover the walk. The `[SSHD] Session pid ..`
teardown lines did not reach the log file in the box (the forked session's stderr); the process facts above are from
`ps` and the kernel's PSTATS lines. Open: give children their own process group (`setpgid` is not in libakuma).

**Recipe.** Box root with real copies (not symlinks: exec does not follow them in a box) of `busybox`, `sh`,
`/lib/ld-musl-x86_64.so.1`, the new `sshd`/`ssh`, `/etc/sshd/{authorized_keys,id_ed25519}`; a throwaway client key
(never the host's `/etc/sshd/authorized_keys`); `box open t --root R /bin/sshd --port 2300 --idle-timeout-secs 6`.
