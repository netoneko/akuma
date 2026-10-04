# jitprobe — can a userspace JIT run under this kernel's W^X policy?

`c/jit_probe.c` answers one question: **does a JIT need a kernel change?** (Asked by
`netoneko/akuma-cli-wgpu`, whose wgpu backend interprets WGSL and wants to compile it.)

The kernel refuses a page that is writable *and* executable in a single call
(`amd64/src/mm.rs`: `sys_mmap` and `sys_mprotect` return `EINVAL` for
`PROT_WRITE|PROT_EXEC`). A JIT does not need such a page: it `mmap`s RW, emits code,
`mprotect`s to R+X, runs it, and flips back to RW to patch. The probe checks that whole
sequence works here.

## Arms

Each runs in a forked child, so a SIGSEGV is reported as `CRASH` instead of killing the probe.

| arm | expected | gates exit code |
|---|---|---|
| `wx_mmap` — `mmap(RWX)` | `EINVAL` (policy baseline) | no |
| `rw_to_rx` — RW, write `mov eax,42; ret`, `mprotect(RX)`, call | returns 42 | **yes** |
| `wx_mprotect` — `mprotect(RWX)` on a live page | `EINVAL` | no |
| `rewrite` — RX→RW→new code→RX, twice | fresh code each cycle (no stale instructions) | **yes** |
| `fork_exec` — RX page, `fork`, child calls it | child exits 0 | **yes** |
| `speed` — JITed `sum(1..1e8)` loop | correct sum; ns/iter printed | **yes** |
| `mmap_rx_anon` — `mmap(R+X)` directly | informational | no |

Ends with `JITPROBE OK` (exit 0) or `JITPROBE FAIL`.

## Run it

```sh
userspace/jitprobe/c/build.sh                 # needs x86_64-linux-musl-gcc (musl-cross)
userspace/jitprobe/c/build.sh --push-akuma    # + base64 over `ssh akuma` into /tmp
ssh akuma /tmp/jit_probe
```

`--push-akuma` uses the `akuma` ssh alias (port 2222) — see
`docs/runbooks/amd64-bare-metal-loop.md`. Re-run it after any change to `mm.rs`, the ELF
loader's protection handling, or the page-fault path.

## Result, 2026-10-04 (trashcan, kernel `0.0.8 94eda586-release-smp-shared`)

All gating arms passed; `mmap(RWX)` and `mprotect(RWX)` refused with `EINVAL` as designed;
the JITed loop ran at **0.394 ns/iter** (39 ms for 1e8). Conclusion: a JIT works today with
no kernel change and no `memfd_create`.

## Not covered

- Multi-threaded JIT (one thread flipping a page while another runs a neighbour).
- Code spanning more than one page, or W^X flips of a sub-range of a larger mapping.
- aarch64 (the arms hard-code x86-64 machine code; the aarch64 kernel also needs an
  icache flush after emitting, which x86 does not).
