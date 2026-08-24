# The IRQ handler table: a self-deadlock hiding behind a `Vec` (2026-08-24)

**Status: fixed. The mechanism is established by construction; the link to one
observed boot freeze is strong but rests on a single reproduction.** Read §5
before quoting this as the cause of any particular `[BKL] stuck` storm.

> **The one-line answer.** `register_handler` enabled an interrupt line **while
> holding the same non-reentrant spinlock that `dispatch_irq` takes from the
> interrupt vector** — and it held that lock across up to 49 heap allocations,
> on the boot path, with the BKL in hand. If the line it had just enabled
> delivered on that core before the guard dropped, the core spun against itself
> forever, and every other core piled in behind the BKL it was still holding.

---

## 1. The code, before

```rust
static IRQ_HANDLERS: Spinlock<IrqHandlers> = Spinlock::new(IrqHandlers {
    handlers: Vec::new(),          // spinning_top::Spinlock — no IRQ masking
});

pub fn register_handler(irq: u32, handler: IrqHandler) {
    let mut handlers = IRQ_HANDLERS.lock();

    while handlers.handlers.len() <= irq as usize {
        handlers.handlers.push(None);      // (a) allocates, inside the lock
    }
    handlers.handlers[irq as usize] = Some(handler);

    crate::gic::enable_irq(irq);           // (b) enables the line, inside the lock
}
```

And the other end, in interrupt context (`src/exceptions.rs:2672`, `:2696`):

```rust
pub fn dispatch_irq(irq: u32) {
    let handler = {
        let handlers = IRQ_HANDLERS.lock();   // same lock, from the IRQ vector
        handlers.handlers.get(irq as usize).copied().flatten()
    };
    if let Some(handler) = handler { handler(irq); }
}
```

## 2. Why it deadlocks

`Spinlock` here is `spinning_top::Spinlock`: not reentrant, and it does **not**
mask IRQs. So on the registering core, between (b) and the guard dropping:

1. `enable_irq(48)` unmasks the virtio-net SPI in the GIC distributor.
2. If that line is pending — or goes pending in the next few instructions — the
   GIC delivers it **to this core, right now**.
3. The vector calls `dispatch_irq(48)`, which takes `IRQ_HANDLERS.lock()`.
4. That lock is held by the code we interrupted, on this same core, which
   cannot make progress until we return. It never returns.

The core is now spinning in an IRQ handler forever. **It is still holding the
BKL**, because `register_handler` runs on the BKL-held boot path — which is
what turns a one-core self-deadlock into a whole-machine stall: every other
core blocks on the BKL and the console fills with

```
[BKL] stuck: owner=1 waiter=2 tag=511 (aff0+1)
```

`(aff0+1)` means the numbers are core+1, so `owner=1` is **core 0**. `tag=511`
is always meaningless — the holder-tag profiler is off by default; read
`owner=`, never the tag.

The allocation in (a) is not the deadlock, but it is what made the window wide
enough to hit: growing a `Vec` from empty to INTID 48 one `push(None)` at a
time is up to 49 allocator round-trips **inside the lock**, before `enable_irq`
is even reached.

## 3. The fix

Both halves, because either alone leaves the bug:

```rust
const MAX_IRQ: usize = 256;

struct IrqHandlers { handlers: [Option<IrqHandler>; MAX_IRQ] }

pub fn register_handler(irq: u32, handler: IrqHandler) {
    let idx = irq as usize;
    if idx >= MAX_IRQ { return; }

    with_irqs_disabled(|| {
        IRQ_HANDLERS.lock().handlers[idx] = Some(handler);
    });

    crate::gic::enable_irq(irq);   // outside the lock
}
```

- **Fixed table**, so the hold is O(1) and allocation-free. `MAX_IRQ = 256`
  costs ~2 KB of `.bss` and covers everything registered in this tree: the
  timer PPI (27) and virtio-mmio SPIs (`VIRTIO_INTID_BASE` = 48 on qemu-virt,
  32 slots).
- **`enable_irq` moved out of the lock** — this is the half that actually
  closes the deadlock.
- **`with_irqs_disabled` around the publish**, so an already-enabled line
  cannot re-enter the lock mid-update either. That case is real: IRQ 27 is
  registered **twice** (`main.rs:1107` installs a probe handler, `:1112`
  replaces it with the real one), the second time with the line already live.

There are only three `register_handler` call sites in the tree, all at boot.

## 4. Why the `Vec` audit walked past it

[`VEC_AUDIT.md`](VEC_AUDIT.md) found this exact code and filed it as finding
#3, then ranked it **last of three** with:

> "Lower priority than #1/#2: registration happens at boot only, and the
> per-IRQ-dispatch lookup is O(1) either way — the cost here is design
> cleanliness, not runtime cost."

Both halves are backwards:

- "Registration happens at boot only" is what makes it **dangerous**, not safe.
  Boot is the one moment a line gets enabled underneath the lock its own
  handler needs.
- It is not a cleanliness issue; it is a deadlock.

And the remedy the audit prescribed — swap `Vec` for an array — is **necessary
but not sufficient**. It removes the allocations and shrinks the window to a
few instructions, leaving a rarer, harder-to-diagnose version of the same
deadlock.

The reusable lesson is about the audit's *method*, not its diligence: it
classified every hit on the axis it set out to measure — *is this container the
right shape?* — and never asked the question that mattered here: **what else
runs under this lock, and from what context?** Any `Vec` behind a lock that
IRQ or fault context can also take deserves that second pass.

## 5. Evidence, and its limits

**Observed once, directly.** An `SMP=4` boot froze between two adjacent prints:

```
[SmolNet] Initialized successfully (VirtIO + Loopback)
                                        <-- froze here, inside register_handler
[Net] virtio-net IRQ: slot 0 -> INTID 48
```

with `[BKL] stuck: owner=1 waiter=2/3/4 tag=511 (aff0+1)` repeating and the log
still growing — core 0 holding the BKL, all three secondaries behind it.
`nic_slot()` is a plain atomic load, so `register_handler` is the only code
between those two prints.

**What this does not establish.** The mechanism in §2 is sound by construction
and needs no reproduction to believe. The claim that it caused *that* freeze
rests on one observation, and the failure is intermittent by nature — it needs
an interrupt inside the window. Specifically:

- A 6-boot A/B at `SMP=4` produced **0 storms in 6** on the *unfixed* kernel
  (under `SNAPSHOT=1`), so that experiment cannot resolve a fix either way.
  It says the conditions did not reproduce it, nothing more.
- `[BKL] stuck` appears in **91 of 308** logs in `logs/`. It is a long-standing,
  frequent, load-driven class
  ([`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md)), and this fix should
  **not** be assumed to close all of it. `docs/README.md`'s matrix row for this
  symptom states the discipline directly: **measure it as a rate when A/B-ing,
  not a boolean** — which is exactly what 6 clean boots fail to do.
- The one storm reproduced this session was under the redis sweep: raw
  `devbox.img`, a `net-profile` build, redis under load.

Treat this as "one real deadlock removed from a crowded field", not "the SMP=4
storm is fixed".

## 6. Verification

- 4/4 clippy configs clean (`--release`, `extreme-size`, `devbox-smoltcp`, rump
  `devbox`) — the rump config also surfaced three genuine `unused_mut` warnings
  in the `poll.rs` wait-loop rewire, fixed at the same time.
- Host tests: 28 suites, 0 failures.
- Boot suite on the default build: 301 PASS / 0 FAIL.
- Guest boots and accepts SSH on `devbox-smoltcp`; `nettest-unix poll` returns
  `verdict=OK`.

## Background

- [`VEC_AUDIT.md`](VEC_AUDIT.md) — the survey that found the `Vec` and misjudged
  it; its finding #3 now carries this correction.
- [`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md) — the `[BKL] stuck
  owner=N tag=511` storm class: the ON_CPU gate closed its EL1-crash half; a
  storm without crashes still reproduces on gated kernels.
- [`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) §5 — the
  session this was found in, and the boot-log reading traps that go with it
  (power-of-two sampled counters; non-atomic console lines at SMP>1).
