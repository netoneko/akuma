# goose serve pins a core while it waits — open, needs a probe

**Status: open, observed 2026-10-24, not reproduced under a probe.** Written for whoever picks it up.

## Symptom

On the ryzen (Akuma, bare metal, AMD Ryzen 7 8845HS), the fans run constantly, including between builds
and while no stage is compiling. The user's words: "it's been like this for a while even without a build"
and "for unknown reason it becomes a processor hog while it waits."

## What was measured (22:5x IDT, Taoxichuan stage 4 running, goose idle between tool calls)

- `/proc/stat` aggregate: 21% busy, 78% idle (about 1.7 of 8 cores). `/proc/loadavg` reads 0.00 and is not
  informative on Akuma.
- `top` showed the busiest process at 13%. It is the wrong view: sampling `utime+stime` (fields 14+15 of
  `/proc/<pid>/stat`) for every pid over 10 s showed one thread dominating:

  | thread | ticks / 10 s |
  |---|---:|
  | `goose serve` `tokio-rt-worker` pid 417 | 1,001 |
  | `goose serve` pid 412 | 97 |
  | `goose serve` pid 415 | 23 |
  | sshd | 15 |
  | all others | about 1 each |

  If the tick rate is 100/s (**not confirmed**) that is one full core.
- Cumulative CPU of the `goose serve` threads at that time was about 4.5 h in roughly 9 h of box uptime
  (412: 2h16, 416: 58m, 417: 42m, 418: 33m, 415: 4m). That average cannot come from a process that is only busy
  while a model call or tool call is running; the server was also quota-stopped 19:56-21:39.
- Ruled out: the build (nothing compiling), the KVM guest (`vm-serve-mc`, 2 vCPUs pinned to cores 4 and 6,
  those cores about 7-8% busy), sshd, herd, klogd, powersaved.

## Second sample, and the restart (23:30-23:33 IDT)

- **Spins with no prompt running.** After the supervisor sent an ACP `session/cancel` (turn ended
  `stopReason: cancelled`, driver gone, nothing in flight) the same thread (pid 417) still used 1,002 ticks
  per 10 s. So the spin does not need a running turn or a build; the server's session was merely idle.
- **A restart clears it.** `herd stop goose-glm; herd start goose-glm` (there is no `herd restart`; it prints
  usage and does nothing) gave a new pid, and a 10 s sample taken right after, with the server idle, had no goose
  thread in the top three (sshd 17 ticks was first). Open question for the probe: how long after a restart, or
  after how much work, the spin comes back. The next full session (stage 4b, started 23:33) will be sampled at
  its end.

## What is NOT known

- Whether the spin is continuous or only while a request is in flight (e.g. a client holding the ACP session
  open, an unresolved tool-call future, a streaming read that returns zero bytes in a loop).
- Whether it is goose, the musl/Akuma runtime under it (a `poll`/`epoll` that returns immediately, a
  timer that does not park), or both. A busy `tokio-rt-worker` on a kernel whose readiness path has known
  divergences from Linux is a live candidate (`docs/reference/subsystems/syscalls/poll.md`); a goose bug is
  equally possible.
- Temperature. Akuma has no `/proc/thermal` and no fan interface, so the fans run on firmware defaults and
  the link between this CPU use and the fan noise is inferred, not measured.

## Side finding: per-cpu counters in `/proc/stat`

Two samples 6 s apart gave nonsense deltas for cpu0-cpu3 (negative and > 100000%), so those rows cannot be
used to find the busy core. Aggregate and cpu4-7 looked sane. Separate accounting bug; see
`KVM_CPU_ACCOUNTING.md` for the earlier one.

## Workaround

`herd restart goose-glm` (the user's choice: do it **between** sessions, never mid-stage, because the
supervisor's ACP driver loses its session). After a restart, re-sample for a minute to see whether the
spin returns on its own, then again after the next session starts and ends. That one comparison already
separates "spins when idle" from "spins only with an open client".

## Probe to write (belongs in `scripts/benchmarks/`, not in a scratch file)

1. Per-thread CPU sampler: the 10-second `/proc/<pid>/task/*/stat` delta used above, as a script that prints
   the top threads with names; run on a schedule and log to a file.
2. Cases, one sample window each: (a) fresh `goose serve`, no client; (b) one ACP session open, no prompt;
   (c) prompt in flight waiting on the model; (d) prompt in flight waiting on a long tool call
   (`sleep 60`); (e) session closed.
3. For the thread that spins, count syscalls per second by number (the kernel's per-process syscall counters,
   `[PSTATS]`) to see whether it is `epoll_pwait`/`futex`/`clock_nanosleep` returning immediately.
4. Control: the same `goose` binary under Linux (`docs/archive/` has the Linux A/B probe technique) so a
   goose bug is separated from an Akuma one.

## Background

- Found while supervising the Taoxichuan ladder: `MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`.
- `KVM_CPU_ACCOUNTING.md`: an earlier "cpu at 100%" that was accounting rather than load.
