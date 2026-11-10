# Scheduling — the tick

Embedded inline in `../reference/scheduling.md`. Keep the two in the same
commit.

```mermaid
flowchart LR
    PIT["8254 PIT ch2 (ports 0x42/0x43/0x61)<br/>calibration reference"] -->|50 ms window| CAL["calibrate (lapic_init)<br/>counts/tick, tsc_hz"]
    CAL --> LVT["xAPIC LVT timer, periodic<br/>vector 32 (TIMER_VECTOR)<br/>akuma-lapic-amd64"]
    LVT -->|tick| TT["timer_trap (lib.rs)"]
    CAL --> CLK["net::calibrate → net::uptime_us (TSC)<br/>one clock: ExecRuntime, ThreadRuntime, primitives::clock"]
    TT --> G{"from ring 3 AND slot<br/>slotted (not NO_SLOT)<br/>AND PREEMPT_ARMED?"}
    G -->|yes| Y["akuma_threading::try_yield()<br/>→ switch_to on the per-slot trap stack"]
    G -->|no| R["return to the same instruction"]
    TT --> NP["netpoll backstop: one drained lap per tick<br/>(NET_UP / IN_LAP guarded)"]
    Y --> NP
    R --> NP
```
