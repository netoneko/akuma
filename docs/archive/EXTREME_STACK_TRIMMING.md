# Trimming the extreme-profile working set (thread + boot stacks)

Follow-on to docs/POST_EXIT_PMM_RECLAIM.md (which established there is no
post-exit *leak* — the floor is working-set vs RAM). This is about shrinking that
working set for the `extreme-size` target.

## Measurement: the stack high-water probe

`threading::report_stack_high_water()` (gated by the `STACK_USAGE_PROBE` const in
crates/akuma-exec/src/threading/mod.rs, default **false**) gives a *trustworthy
upper bound* on kernel-stack usage:

- `fill_stack_sentinel` paints every freshly-allocated stack with
  `0xABAB…`; any write — even a zero — breaks the sentinel, so a scan for the
  deepest broken word is an upper bound on peak usage (never an underestimate,
  which is what makes shrinking safe).
- `paint_boot_stack` does the same for thread 0's 1 MB boot stack (painted early
  in `kernel_main`, before the deep work, so the peak is captured).
- Peaks are recorded at teardown (`free_stack_for_slot`) so a short-lived user
  thread's high-water survives `WARM_FREE_USER=0`'s immediate free.

Flip `STACK_USAGE_PROBE = true`, boot, and read the `[Mem]`-adjacent `[Stack]`
line. Costs a memset per stack alloc (+ 1 MB at boot) — keep it **off** in shipped
kernels; it's a measurement/safety tool.

### Measured peaks (extreme-size @ MEMORY=6M, SSH + busybox + spawn workload)

```
[Stack] sys peak 79KB/128KB | user peak 48KB/64KB | boot peak 10KB/1024KB
```

- **System threads (async-main, SSH): 79 KB.** Confirms the history — 64 KB
  overflowed (ELR=0x0), the true peak is 79 KB.
- **User process threads: 48 KB** (busybox commands; tcc is shallower).
- **Boot stack (thread 0): 10 KB of 1024 KB.** A ~100× over-provision.

## Changes shipped (safe, verified)

All extreme-only, behind the new `kernel_profile_extreme` cfg (akuma-exec now
emits it too: `extreme = ["akuma-exec/extreme"]` forwards the bin's feature, and
crates/akuma-exec/build.rs mirrors the bin's discriminator logic).

1. **Warm-stack floor → 0** (`WARM_FREE_SYSTEM`/`WARM_FREE_USER`). System threads
   spawn once at boot and never recycle, so a warm system reserve is dead weight;
   the warm *user* stack was the 64 KB that lingered after a process exited.
   Free-on-recycle entirely. At the floor, ~serial workloads re-use the
   just-freed 16 contiguous pages, so no spawn-time contiguous-alloc penalty.
2. **System kernel stack 128 KB → 96 KB.** 79 KB peak + 17 KB (~21%) margin; the
   base canary trips first if a deeper path ever exceeds it. Saves 32 KB per live
   system thread (~64 KB at the idle 2-thread floor).

Verified at MEMORY=6M: idle free RAM **3100 KB → 3228 KB (+128 KB)**, system peak
79 KB sits in 96 KB with no canary trips, SSH + spawns healthy, all three
profiles (release/size/extreme) build.

User stack kept at 64 KB — 48 KB peak leaves only 16 KB, too tight to trim.

## The big lever, NOT yet taken: the 1 MB boot stack

Thread 0 uses **10 KB**. The other ~1 MB is reserved (`linker.ld`:
`STACK_TOP = STACK_BOTTOM + 0x100000`, mirrored by `boot.rs .equ STACK_SIZE`) and
sits inside `code_and_stack`, below the heap. The existing extreme reclaim only
trims the *guard above* the stack top (`STACK_GUARD` 1 MB→64 KB), not the stack
itself.

Reclaiming the ~768–896 KB of dead boot-stack slack (≈17% of RAM at 4.5 MB) needs
one of:

- **(a) Shrink the linker reservation** (`0x100000` → e.g. `0x40000`). Cleanest
  result, but touches the boot/linker guardrail that has had two misalignment
  bugs, and linker.ld can't see the cargo profile — needs a `--defsym` from
  build.rs or a separate extreme linker script.
- **(b) Hand the slack back to the PMM at runtime** (extreme-only). Keeps the
  boot asm/linker untouched (SP still = STACK_TOP), but requires the PMM to
  accept a second, disjoint free region `[STACK_BOTTOM, STACK_TOP − safe]`.

Both are larger and riskier than the changes above; deferred for an explicit
decision. The 10 KB measurement says either would be safe with a large margin
(e.g. a 256 KB boot stack still leaves 25×).
