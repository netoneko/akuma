# `md5sum`/`sha*sum` return wrong digests for correct bytes

**Date:** 2026-08-28
**Status: ROOT-CAUSED AND FIXED.** `sync_el1_handler` (`src/exceptions.rs`) saved
only x0-x3/x29/x30, but the two EL1 fault paths that *resolve* a fault
(`try_resolve_el1_cow_fault`, `try_resolve_el1_user_copy_lazy_fault`) `eret` back
to **re-execute the faulting instruction**. A retry is only correct if that
instruction's input registers survived the handler — and x4-x18 did not. While
`__arch_copy_user_memory` was a byte loop holding its one live datum in x3 the
defect was invisible; the 2026-08-27 widening put live data in x3-x10, so a
`read(2)` whose destination page was faulted in mid-copy stored **leftover
handler state** instead of file bytes, 24 bytes per page, with `read` reporting
the full count. Fixed by making the vector transparent to x4-x18; guarded by
`test_el1_sync_exception_preserves_gprs`.
**Grade: A** — established by single-variable A/B on the vector's save/restore
(below), with the clobbered register set matching the corrupted byte offsets
register for register.

## The fix, and the evidence for it

Two lines of evidence, both from 2026-08-28 at `SMP=4`, `MEMORY=2048`, HVF, on
the default `cargo build --release` kernel with the **widened** copy loop:

| `sync_el1_handler` saves | `md5probe whole` | busybox `md5sum` | boot test |
|---|---:|---:|---|
| x0-x3/x29/x30 (as shipped 2026-08-27) | **10/20 wrong**, always 90 bad words | 21/40 wrong | `[FAIL] mask=0x7ff3` |
| + x4-x18 (the fix) | **0/40** | 0/40 | `[PASS] 1 abort, x4-x18 intact` |

Nothing else changed between those rows. Full post-fix matrix — 268 measurements,
zero failures: `md5probe whole`/`whole-mmap`/`whole-warm`/`whole-touch` 0/40 each,
`whole` on a 1 MB file 0/12, `whole` under concurrent anon+file-cache pressure
0/24, busybox `md5sum` 0/40 (64 KB) and 0/12 (1 MB), and `md5sum`/`sha1sum`/
`sha512sum`/`cksum` each returning exactly one distinct digest over 12 runs.

`mask=0x7ff3` names the damage precisely: x4, x5 and x8-x18 came back clobbered,
x6 and x7 survived. That is the same register set the corrupted bytes decode to
(see "How the byte offsets name the registers"), which is what makes this a
diagnosis rather than a correlation.

### Why the register save was ever enough

Because the copy loop used to live entirely in registers the vector already
saved. The pre-2026-08-27 loop is `ldrb w3 / strb w3`: one live datum, in x3.
Widening it to 64/16/8-byte tiers spread live data across x3-x10 — and x4-x10
were destroyed by every resolved fault. So the widening did not *introduce* a
bug; it **exposed** a latent one in the exception vector, which is why reverting
the widening (`USER_COPY_TIER = 1`) made the symptom vanish and looked like a
fix. It was a timing-and-register-allocation mask.

That also retires the "damage scales with store width" reading in § ROOT CAUSE
below: 360 B lost with 64-byte groups vs 120 B with 16-byte pairs is not a race
against wall-clock work, it is simply how many live registers each loop shape
holds across the faulting store.

### Two claims in this document were wrong; both are corrected here

1. **"Under TCG it fails 100 % of the time; under HVF ~20 %."** Re-measured
   2026-08-28 on the same tree (HEAD `2386520c`): under TCG, `md5probe whole` is
   **0/24** and busybox `md5sum` **24/24 correct** — TCG does not reproduce it at
   all. HVF reproduces at **~60 %** (24/40, 23/40, 25/40 across three probe
   arms). The reproducer is **HVF, not TCG**; anyone continuing this class of
   work should not spend a TCG run expecting a signal. (Why TCG hides it is not
   established and no longer matters: whether QEMU's software MMU takes the same
   translation fault at the same point in the loop is an emulator detail.)
2. **"The 2026-08-27 user-copy widening is the cause."** It is the *trigger*, not
   the cause. The A/B that identified it was sound and its measurements stand;
   the conclusion drawn from it was one level too shallow. The cause is the
   exception vector, and the tell was there in the elimination table all along:
   a mechanism that survives page-splitting, IRQ masking and every allocator
   arm, yet dies the moment the copy uses fewer registers, is about **registers**.

### How the byte offsets name the registers

The measurement that cracked it was a histogram of *in-page* offsets of the bad
words, rather than a first-bad offset. The damage is not one contiguous window —
which is how it was read for two sessions — but **the same few words in every
destination page**:

```
malloc dest (b = page+0x20):  bad in-page offsets 8,12,16,20,24,28   x 15 pages = 90 words
mmap dest   (b = page+0):     bad in-page offsets 8,12,40,44,48,52,56,60 x 16   = 129 words
```

Line the malloc case up against the 64-byte tier. The group that first stores
into the new page runs `stp x3,x4,[x0,#0] / stp x5,x6,[x0,#16] /
stp x7,x8,[x0,#32] / stp x9,x10,[x0,#48]`. Its first store into that page faults
(first touch of a lazy page), `try_resolve_el1_user_copy_lazy_fault` maps the page
and ERETs to retry — and the retried stores write whatever the handler left
behind. The bytes that come back wrong are exactly the second half of the
`x7,x8` pair and both halves of the `x9,x10` pair: **x8, x9, x10**. In the mmap
case, whose group boundaries sit differently against the page, the wrong bytes
are **x4** plus **x8, x9, x10**.

And the leftover values are unmistakably the fault handler's own working set —
per destination page, three consecutive words:

```
page 0x10421: 0x10421  0x123  0
page 0x10422: 0x10422  0x124  6
page 0x1042f: 0x1042f  0x131  0
```

The first word is the faulting page's own VA >> 12; the second increments once
per page. Those are `page_va`, a frame index, and a small flag — the handler's
locals, stored into the user's buffer by the retry, while `read` reported 65536
of 65536.

## Summary

`busybox md5sum` returns a **wrong, non-deterministic digest** for an unmodified
file, roughly **40–50 % of invocations**, for files larger than one page. The
bytes are not wrong: the same file verifies byte-exact through `read(2)` and
through `mmap`, and `busybox cksum` and `busybox base64` — which read every byte
of the same file — are perfectly stable across runs.

**The answer is at the top of this file**: the EL1 exception vector did not
preserve the registers the widened user-copy loop holds live across a store, so a
resolved page fault mid-copy made the retry store handler state. Everything from
here down is the elimination trail — worth keeping,
because it retires a dozen plausible mechanisms (ext2, the page cache, mmap,
NEON/FP state, `.rodata`, PIE/RELR, signals, the shared file-page cache) that
this symptom otherwise invites, and because *why* it looked like a userspace
compute bug for so long is itself the lesson.

This is **silent data corruption in userspace computation**, and it matters well
beyond `md5sum`: in-guest integrity checking is built on exactly these
algorithms. `apk` verifies package digests, `cargo` verifies crate checksums,
and both run in the guest during self-host work. A ~50 % false digest is
indistinguishable from a corrupt download.

It was first noticed as a one-off in
[`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md)
§ "Resolution", recorded there as *"signature points at ext2 page-cache
coherence"*. **That guess is wrong** — see the elimination table.

## Reproduction

Deterministic enough to bisect against. On a booted default kernel:

```python
# push a file whose 4-byte word at offset o contains o itself
blob = b"".join(o.to_bytes(4, "little") for o in range(0, 65536, 4))
ssh("cat > /tmp/ident.bin", input=blob)
ssh("i=0; while [ $i -lt 20 ]; do busybox md5sum /tmp/ident.bin; i=$((i+1)); done")
```

Measured (host md5 = `f67ea8aa…`):

```
f67ea8aaa3735fcf05215a86495be8f7  x10   <= CORRECT
45ec135e83b2d9d4a1468ce96d0b2861  x9    <= WRONG
e6194d3a0c2feef3646375a820469f8f  x1    <= WRONG
```

The wrong values are **not random noise**: one dominant wrong value recurs, with
an occasional second. They are also **not a digest of any simple corruption of
the file** — ~300 candidate contents were tested against the observed wrong
digests (every page- and 512-aligned truncation, each of the 16 pages zeroed,
every page↔page aliasing pair, pages filled `0xAA`/`0xFF`, doubled file):
**zero matches**. So busybox did not hash a plausibly-damaged version of the
file; it computed something else entirely.

### The two signatures that constrain everything

**1. First file wrong, second file right — within one process.** Hashing the
same file twice in a single invocation:

```
busybox md5sum /tmp/ident.bin /tmp/ident.bin
```

15 invocations, 8 of them failing, and in **all 8** the first digest was wrong
and the second correct. Never the reverse. Same code, same constant tables, same
process — so this is a first-pass effect, not a corrupt binary and not a corrupt
input.

**2. One page is the threshold.** A 4000-byte file: **20/20 correct**. The same
content at 65536 bytes: ~50 % wrong. That is also where a single `read(2)`
becomes several.

### It is algorithm-specific, not binary-specific

All of these are the *same* `/bin/busybox` multi-call binary:

| applet | distinct results over 12 runs |
|---|---|
| `md5sum` | **3** |
| `sha1sum` | **2** |
| `sha512sum` | **2** |
| `cksum` (CRC32) | 1 — stable |
| `base64` (reads and emits all 64 KB) | 1 — stable |

`base64` is the strongest exoneration of the data path in the set: it consumes
every byte and reproduces every byte, and its full output was byte-identical
across 10 runs. `cksum` likewise walks the whole file. The unstable set is
exactly the md5/sha family, which in busybox share libbb's hash driver and carry
a multi-word context plus a partial-block buffer **across** each `read(2)`,
where `cksum`/`base64` keep essentially no state between chunks.

## What it is NOT

This is the expensive part; do not re-run it. Every arm below is a purpose-built
static probe (sources in the session scratchpad, not yet landed — see "Next
steps"), each calibrated to be clean on the same kernel and each **failing to
reproduce** while busybox reproduced freely in the same boot.

| Ruled out | How |
|---|---|
| **Wrong bytes from `read(2)`** | Self-identifying file (word at offset `o` == `o`), read back and compared byte-for-byte: **0 wrong words**, 60 iterations, chunk sizes 4096 and 65536, on both a self-written file and one pushed in over ssh |
| **Wrong bytes from `mmap`** | Same file mapped `PROT_READ`/`MAP_PRIVATE` and `memcmp`'d: **0/60** |
| **ext2 / page-cache coherence** | Implied by the two rows above, and by `cksum`/`base64` stability. The original "page-cache coherence" guess is retired |
| **Accumulator corrupted across a syscall** | FNV-1a folded *into* the read loop so the accumulator is live across every `read(2)` — md5sum's exact shape: **0/60** wrong |
| **Destination-buffer memory class** | Read destination in heap, in a 128 KiB **stack** array, in a fresh untouched **anonymous mmap**, and in **`.bss`** (virgin, one iteration per fresh exec, 60 execs): all **0 failures** |
| **GPR computation under preemption** | Integer chain over a 256 KiB warm buffer, 400 iterations: **0 wrong** |
| **FP (D-register) state across preemption** | `double` chain, same loop: **0 wrong** |
| **128-bit NEON (Q-register) state** | 16 live `uint32x4_t` accumulators folded so every lane matters, low and high 64 bits compared separately, 300 iterations: **0 wrong** in both halves. (This was a strong hypothesis — a save/restore storing `d0..d31` rather than `q0..q31` would spare a `double` workload and corrupt every vector one — and it is dead) |
| **First-touch of file-backed `.rodata`** | 256 KiB `.rodata` table, word `i` a pure function of `i`, verified as the first act of `main()`, 80 fresh execs: **0 mismatches**. So md5's constant table is not being faulted in wrong |
| **Corrupt busybox on disk** | The correct digest occurs, repeatedly, for the same file — and `busybox md5sum /bin/busybox` itself yields the correct host digest 3/8 while giving one *consistent* wrong value 5/8. A corrupt image would be deterministic |
| **RELR shared-page accumulation** (the still-open bug in [`instr_abort_relr_wedge`-class work](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)) | The accumulation shape predicts a rate that grows with exec count. Measured across 600 additional `busybox true` execs: **8/20 → 8/20 → 4/20**, i.e. flat/noisy, not growing |
| **Path/`open` specific** | Fails via stdin too (`busybox md5sum < /tmp/ident.bin`: 12 correct, 8 wrong over 20) |

## Three more eliminations (same day, after the table above)

**It is not our busybox binary, our build, or PIE-vs-static.** A completely
independent busybox — Alpine's own `/bin/busybox`, a different version built by a
different toolchain, **dynamically linked PIE** — staged into the guest via
Docker, fails the same way:

| binary | wrong / 16 | distinct |
|---|---:|---:|
| Alpine busybox (dynamic PIE, different build) | **4** | 3 |
| our busybox (static, control, same boot) | **8** | 2 |

That kills the PIE/RELR hypothesis outright — a static non-PIE build and a
dynamic PIE build both miscompute — and it moves the defect firmly kernel-side:
two independently-built programs get it wrong on the same kernel.

**It is not the shared file-page cache.** One-flag A/B on
`config::SHARED_FILE_PAGES_ENABLED` (`src/file_page_cache.rs`'s documented kill
switch), full rebuild, fresh SMP=4 boot: **11/20 still wrong, 4 distinct
values**. A per-box shared page handed out wrongly after eviction was a natural
suspect — the files here are tiny, so there is nothing to evict — and the flag
settles it.

**The wrong digests are not a digest of any simple mangling of the file.** The
forensic search was extended from ~300 to ~1300 candidate contents, now
including length-changing mutations: every 4096-byte page duplicated, every page
skipped, an offset-reset at each page boundary, and the same three at 64-byte
md5-block granularity. Against eight distinct observed wrong digests: **zero
matches**. So busybox is not hashing a re-ordered, duplicated, truncated or
shifted version of the bytes. Whatever it hashes is not a permutation of the
file.

### Staging non-busybox binaries via Docker (recipe, and where it stops)

Worth recording because the obvious comparison — a non-busybox `md5sum` — is
still not made. Docker on the host can produce arm64 Alpine binaries:

```bash
docker run --rm --platform linux/arm64 -v "$OUT:/out" alpine sh -c \
  'apk add --no-cache coreutils >/dev/null; cp /usr/bin/md5sum /out/; \
   cp /lib/ld-musl-aarch64.so.1 /out/; cp -L /usr/lib/libcrypto.so.3 /out/'
```

Push them in over ssh and `chmod +x`. The guest has **no** `/lib/ld-musl-aarch64.so.1`
of its own, so the loader must be staged too — after which Alpine's busybox runs
(that is how the comparison above was made; note it dispatches on `argv[0]`, so
it must be named `busybox`).

Coreutils `md5sum` still could not be run: it pulls a chain of shared libraries
(`libcrypto.so.3`, `libacl.so.1`, `libattr.so.1`, then `libutmps.so.0.1`, …).
Either keep staging them, or build a static non-busybox md5 with the local
`aarch64-linux-musl-gcc` — the latter is probably faster and also gives a
*static* non-busybox implementation, which is the missing cell in the table.

## Session 2 (same day): a controlled reproducer, and five more falsifications

**The headline: this is no longer a busybox story.** A purpose-written 300-line
static probe (`md5probe`, own MD5, calibrated correct on the host) reproduces it,
so the reproducer is now something we control end to end.

### The reproducer

`md5probe whole <path>` does exactly what `[PSTATS]` showed busybox doing —
measured from the kernel side, busybox issues `fstatat` and **one** `read` for a
64 KB file, no `mmap` of the input:

```c
fstat(fd, &st);
b = malloc(st.st_size);        /* NOT touched */
read(fd, b, st.st_size);       /* ONE large read */
md5(b, st.st_size);
```

Run one iteration per **fresh exec**: **8–10 of 16 runs wrong**. Run several
iterations *inside* one process: all correct — the same first-pass-only signature
busybox has.

### The damage is deterministic, and `read` lies about it

| destination | bad words | first bad offset | content found there |
|---|---:|---:|---|
| untouched `malloc` | **90** (of 16384) | **4072** (24 B before the page boundary) | `0x10421` — foreign, not from this file |
| untouched `mmap` | **129** | **12** | **zero** — the page's own initial zeroes |

In every failing run `st_size=65536` and **`read` returned 65536**. So a small
contiguous window is missing while the syscall reports complete success. The
extent is identical run to run for a given shape, which is the shape of an
alignment/length bug far more than of a race.

### Falsified — do not re-run these either

Each was a plausible mechanism, tested and killed:

| Hypothesis | Test | Result |
|---|---|---|
| **A page fault *during* the copy loses bytes** | Pre-touch all 16 destination pages, then punch exactly ONE back to a fresh `MAP_FIXED` page, so the copy has exactly one possible fault at a known page. Five hole positions | **0/12 corrupted at every position.** A guaranteed mid-copy fault corrupts nothing |
| **It scales with the number of lazy pages** | Same, punching 0, 1, 2, 4, 8, 12, 16 pages | **0/12 at every count**, including all 16 lazy |
| **One large lazy region vs many small ones** | Punch the whole 64 KB as a single `MAP_FIXED` region instead of 16 one-page regions | **0/12** |
| **Fresh-address-space state right after `execve`** | `whole-warm`: identical to `whole` but with a throwaway 1 MB mmap+memset+munmap first | **10/16 still wrong** — warming changes nothing |
| **`malloc` (brk/heap) vs `mmap` destination** | `whole-mmap` | **9/16 wrong** — mmap is *not* immune, so an early clean 0/20 reading on this arm was luck. What is robust is untouched-vs-pre-touched, not the allocator |
| **Installed signal handlers** (busybox issues `rt_sigaction` ×2; the probe issued none) | `SIGH=1` arm installing 7 handlers | 0/14 wrong, and `handler_runs=0` — no signal was ever delivered, so this is **untested rather than refuted** |

The one manipulation that reliably *fixes* it remains `memset`-ing the
destination before the read (`whole-touch`: **0/20**).

### It is a Heisenbug, precisely measured

A temporary trace in the file-read arm fingerprinting every chunk on both sides
of `copy_to_user` (`k=` kernel buffer, `u=` re-read from the user buffer) made
the corruption **disappear: 8/8 correct**. The per-byte readback is slow enough
to shift the timing.

Two consequences. First, timing matters, so the deterministic-extent reading
above and this are in tension — the mechanism is probably deterministic *given* a
timing window that opens ~50 % of the time. Second, and more practically:
**there is still no kernel-side fingerprint from a failing run**, so "the kernel
buffer `temp` was already correct before `copy_to_user`" is **unverified**. The
next attempt must not print: accumulate the fingerprint into a `static` and read
it back out of band.

### The strongest remaining lead

`__arch_copy_user_memory` (`crates/akuma-exec/src/mmu/user_access.rs`) — the
user-copy asm — **was widened from a byte loop to 64/16/8-byte chunks on
2026-08-27**, which is the same day this symptom was first observed
(`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md` § Resolution), and there is a later
`8cf0747c fix copy byte loop` commit. Every clean arm of every probe reads in
4096-byte chunks; every corrupting arm issues **one 64 KB read**, which is the
only shape that drives the 64-byte tier hard.

The loop inspects correct by eye (tier arithmetic and post-index updates are
consistent), so the way to settle it is not more reading:

**Sweep it deterministically as a boot test.** Drive
`__arch_copy_user_memory` directly over the cross-product of source alignment,
destination alignment, and length (especially lengths that straddle 64/16/8
boundaries and page boundaries), comparing against a byte-wise reference. That
converts a 50 %-of-the-time guest symptom into a deterministic unit test, and it
is the kind of test `docs/archive/USER_COPY_BYTE_LOOP.md` should have shipped
with the widening.

## The 2026-08-27 user-copy widening (the trigger, not the cause)

**Superseded by the section at the top of this file — read that first.** The A/B
below is still the measurement that narrowed the search from "userspace
miscomputes" to "the user copy loses bytes", and it is kept for that. Its
conclusion ("the defect is in the multi-register `stp` stores") is wrong: the
defect is that the EL1 exception vector did not preserve the registers those
stores read. The tier harness the section describes (`USER_COPY_TIER`, the
`no64`/`x8`/page-split/IRQ-masked arms) was removed once the cause was known;
`__arch_copy_user_memory_bytes` stays as the differential sweep's oracle.

### Original section: ROOT CAUSE: the 2026-08-27 user-copy widening

Confirmed by single-variable A/B, not by reading code. `USER_COPY_TIER` in
`crates/akuma-exec/src/mmu/user_access.rs` selects the copy loop; nothing else
changes between arms:

| `USER_COPY_TIER` | copy shape | `md5probe whole` | `busybox md5sum` | bad words per failure |
|---|---|---:|---:|---:|
| `0` | shipped: 64/16/8/byte tiers | **9-10 / 16 wrong** | **6-8 / 12 wrong** | **90** (360 B) |
| `2` | same minus the 64-byte tier | **7 / 16 wrong** | **3 / 12 wrong** | **30** (120 B) |
| `1` | pre-widening byte loop | **0 / 16** | **0 / 12** | 0 |

Two things follow, and the second is the useful one:

1. **The widened user-copy loop causes the corruption.** The byte loop is clean
   on both the probe and busybox, in the same boot, on the same file.
2. **The damage scales with store width** — 360 bytes lost per failure with
   64-byte groups, 120 with 16-byte pairs, zero with single-byte stores. So the
   defect is in the **multi-register `stp` stores to user memory**, not in the
   loop's length arithmetic (which inspects correct, and which a length bug would
   break deterministically rather than ~50 % of the time).

Supporting detail: in the `malloc` arm the first bad word sits at offset **4072**
— 24 bytes before the page boundary — which is exactly where a 16-byte `stp`
starts straddling into the next page. No `EFAULT` is ever returned and `read`
reports the full byte count, so this is not the copy aborting on a fault.

### Mitigation formerly in the tree (removed 2026-08-28)

`USER_COPY_TIER = 1` (the byte loop) — the only arm measured clean on **both**
the probe and busybox (0/16 and 0/12 at SMP=4, re-verified 0/20 and 0/20 with the
boot suite at 99 `[PASS]`, empty failure set). Correctness over speed: it reverts
the 2026-08-27 speedup (warm 4 KB `pread` was 2110 -> 1100 ns, and the byte loop
costs ~16x an in-kernel memcpy per byte), so it is a placeholder, not the fix.

Flip to `0` to reproduce, `2` to bisect tiers, `3`/`4` to see the failed
candidate fixes.

### How it was actually cracked (method, not luck)

Worth recording because the two decisive moves were both *stepping outside the
probe-writing loop*, and both came from the reviewer rather than from the
investigation's own momentum:

1. **Gate the suspect change and A/B it.** After ~15 userspace probes had each
   come back clean, the question "can you roll the copy change back, or gate it,
   and see if it still reproduces?" settled in two builds what no amount of
   further probe-writing would have. A suspect with a date on it
   (`git log`: the widening landed 2026-08-27, the symptom was first seen
   2026-08-27) is a one-variable experiment, not a hypothesis.
2. **Read the failing program from the kernel side.** The push to disassemble
   busybox led instead to `[PSTATS]`, which reports a per-process syscall
   histogram — and it showed busybox issuing `fstatat` plus exactly **one 64 KB
   `read`**, no `mmap`. That is the shape every probe had missed: a single large
   read into an *untouched* destination. Reproducing it took one more probe mode
   (`md5probe whole`) and moved the reproducer from "busybox does something
   strange" to 300 lines under our control.

The counter-lesson, stated plainly because it cost the most time: writing more
probes that *fail to reproduce* feels like progress and is not. Nine clean arms
in a row was the signal to stop and go after the diff, not to write a tenth.

Two claims made during this session were also **wrong and retracted here**, both
worth knowing about:

- *"`malloc` vs `mmap` destination is the differentiator"* — no: the `mmap` arm
  fails too (9/16). An early 0/20 reading on it was luck, believed because it fit
  the story being told at the time.
- *"A page fault during the copy loses the bytes"* — falsified by a controlled
  test (pre-touch every destination page, punch exactly one back to a fresh
  `MAP_FIXED` page): **0/12 corrupted at five different hole positions**. A
  guaranteed mid-copy fault corrupts nothing. That test only got written because
  the claim was challenged rather than accepted.

### Two candidate fixes tried, both failed

Recorded so nobody re-tries them. Both keep the wide stores and target a
specific mechanism; both still corrupt.

| `USER_COPY_TIER` | idea | result |
|---|---|---:|
| `3` | **Page-split**: bound every segment by the nearer next page boundary of src and dst, so no `ldp`/`stp` ever straddles a page, while the 64-byte tier still does the bulk inside each page | **12/20 wrong** (`whole`), 11/20 (`whole-mmap`) |
| `4` | **IRQs masked** across the whole copy (`with_irqs_disabled`), so no tick, no scheduler SGI, no preemption can land between the stores | **9/20 wrong**, 5/20, busybox 11/20 |

Tier 4 is the one that matters: **"an interrupt tears the copy" is falsified.**
Masking interrupts for the entire 64 KB changes nothing. Combined with the
earlier hole test (a guaranteed mid-copy *fault* corrupts nothing) and the fact
that no `EFAULT` is ever returned, the tear is neither a fault nor an interrupt.

### One more clue: SMP matters, but not for both programs

Shipped widened loop (`tier 0`) at **SMP=1**:

| | wrong / 20 |
|---|---:|
| `md5probe whole` | **0** — clean, vs 9-12/20 at SMP=4 |
| `busybox md5sum` | **6** — still fails |

So the probe's corruption looks **cross-core**, while busybox still miscomputes on
a single core. Either busybox reaches a second path (it does more startup I/O
than the probe), or the probe's rate at SMP=1 is low rather than zero and 20 runs
missed it. **Do not read this as "two bugs" without more samples** — but do note
that the byte loop (`tier 1`) makes *both* clean at SMP=4, so the wide stores are
implicated in both.

### Where that leaves the mechanism

Everything ruled out so far — fault, interrupt, straddle, page count, laziness,
region granularity, barriers at the PTE install — leaves a race between the
copy's stores and **something else touching those frames**: frame
zeroing/recycling, a concurrent PTE edit, or a reclaim path. The width scaling
is the strongest hint: a faster copy loses *more* bytes, which is the signature
of racing against work that lands at a fixed wall-clock offset rather than
against anything the copy itself does.

### Session 3: a 100 % reproducer, and the source buffer cleared

**Under TCG it fails 100 % of the time.** `HVF=0` (software emulation) instead of
HVF, same kernel, same probe: `md5probe whole` **24/24 wrong**, busybox `md5sum`
**24/24 wrong (14 distinct digests)**, always exactly 90 bad words. Under HVF the
same case is ~20 %. So this is not an HVF quirk — TCG simply holds the window
open — and **anyone continuing this should work under TCG**, where the signal is
deterministic instead of a coin flip.

**The kernel's own buffer is correct; `copy_to_user` loses the bytes.** The
probe file is self-identifying, so the read path can verify just the 90-word
window that comes back wrong — 90 comparisons, printing *only* on mismatch, so
the common path stays silent. Result: **0 occurrences of `[KBUF-BAD]` across a
run where 8/40 invocations were corrupted.** `temp` held the right bytes every
time. That retires ext2, the file-page cache and the read-into-`temp` path for
good; the loss is on the user side of the copy.

### Every in-kernel instrumentation of this path suppresses it

This is the defining practical property of the bug, and it has now bitten three
times:

| instrumentation | effect |
|---|---|
| Per-chunk fingerprint on both sides of `copy_to_user` (per-byte readback) | **8/8 clean** — bug gone |
| Read-only page-table walk before/after the copy (`translate_current_user_va`) plus one frame read through the kernel alias | **0/6 clean under TCG**, where the uninstrumented kernel is 24/24 |
| The 90-word check on the *kernel* buffer only (no user access, no PTE walk, silent unless wrong) | bug still present (8/40) — **the only instrumentation that survived** |

So the budget for anything added to that path is tiny: no printing, no user
access, no page-table walk. Prefer capturing into `static`s and reading them out
of band, and re-confirm the corruption is still occurring in the same run before
believing a negative result.

### A misstep worth not repeating

Filling freshly allocated pages with a `0xDEADF00D` marker instead of zeroes — to
tell "never written" from "written then zeroed" — **wedged the kernel** in a
`[BKL] stuck` storm. `alloc_page_zeroed` also backs **page tables and kernel
structures**, not just user anon pages, so a non-zero fill poisons them. If the
marker idea is worth retrying, restrict it to the user demand-paging allocator
(`alloc_page_zeroed_user`) and expect programs that rely on zeroed `.bss` to
misbehave.

### What to do next *(as written before the cause was found — answered above)*

The first two items below were done and are what cracked it: the in-page offset
histogram is the "instrument the frame lifecycle" item's real payoff (the frames
were never swapped — the *registers* were), and the boot-test sweep proved the asm
innocent, which is what forced the search out of the copy loop and into the
vector. The third item, "does a straddling `stp` behave correctly", has the answer
"yes — straddling was never involved".

#### Original list

- **Instrument the frame lifecycle, without printing.** The remaining hypothesis
  is that the destination frames are written by someone else after the copy.
  Record `(va, frame_pa)` at prefault, then after the copy re-read the PTE and
  confirm it still points at the same frame and that the frame holds what was
  copied. Accumulate the result into `static` counters and read them out of band
  — a `safe_print!` on this path suppresses the bug (measured: the per-byte
  readback trace made it 8/8 clean).
- **Sweep `__arch_copy_user_memory` as a boot test** over (src alignment, dst
  alignment, length), especially lengths and destinations that make an `stp`
  straddle a page boundary, comparing against a byte-wise reference. That is the
  test the widening should have shipped with
  (`docs/archive/USER_COPY_BYTE_LOOP.md`).
- **Check the straddling-store hypothesis directly**: does a `stp` whose two
  halves land in different pages behave correctly when both pages are freshly
  prefaulted? `map_user_page` does issue `dsb ishst; tlbi vaae1is; dsb ish; isb`
  on install (checked — it passes `flush = true`), so a missing barrier is ruled
  out at that site; the remaining suspects are the emulator's handling of a
  page-straddling multi-register store, and anything that re-maps the second page
  between the prefault and the store.
- Re-check whether `main` (or any commit before the widening) is clean — expected
  to be, given arm `1` above, but worth one confirmation.

## Leading hypotheses, none confirmed *(all superseded — see the top of this file)*

Both surviving hypotheses below are **dead**. The failing set being md5/sha and
not `cksum`/`base64` has nothing to do with libbb's hash driver: it is that the
hash applets issue one large `read(2)` into a fresh untouched buffer, which is
the only shape that faults a destination page in mid-copy. `cksum` and `base64`
read in small chunks into a buffer they have already touched.

### Original hypotheses

1. **libbb's shared hash driver.** md5/sha1/sha512 are the failing set and they
   share `libbb`'s hash plumbing (a common read buffer — busybox has a global
   `bb_common_bufsiz1` — plus a context struct carried across chunk boundaries).
   `cksum`/`base64` do not use it. The next measurement is to read busybox's
   source for that driver and instrument it, or disassemble the md5 applet, and
   find what it touches that a stateless chunk loop does not.
2. ~~**PIE + RELR.**~~ **Dead** — see "Three more eliminations": a static
   non-PIE busybox and a dynamic PIE busybox both miscompute.

3. **busybox's `bb_common_bufsiz1`, or wherever the hash context lives.** The
   failing set (md5/sha1/sha512) shares libbb's hash driver; the passing set in
   the same binary (`cksum`, `base64`) does not, and that split survives across
   two independent builds. busybox keeps a global scratch buffer
   (`bb_common_bufsiz1`) that applets share, and the hash driver carries a
   multi-word context plus a partial-block buffer **across** every `read(2)`.
   The probe arms above put a *read destination* in `.bss`, on the stack, and in
   anonymous mmap and found nothing — but none of them placed **live state that
   must survive a syscall** at busybox's particular addresses.

   Concrete next measurement: disassemble the md5 applet (the binary is
   stripped, so work from the `.rodata` md5 constant table backwards) to find
   where the context lives, then check whether the kernel writes anywhere near
   it on a syscall return — signal-frame setup, TLS, or the initial
   stack/auxv region are the candidates with prior form in this tree.

### A corroborating symptom, noticed in passing

`apk update` in the same guest reports, for a **cached local** file:

```
WARNING: opening from cache …/APKINDEX.tar.gz: file format is invalid or inconsistent
```

That is a gzip/tar integrity failure while reading a local file larger than a
page — the same shape as this bug, in an unrelated program that is not busybox
and does not compute md5. It is circumstantial (the network was also down, so
the index may genuinely be truncated) but it is the first hint that the defect
is not confined to busybox's hash applets, and it is cheap to re-check once
`apk` works: `apk update` twice and see whether the warning is intermittent.
An intermittent warning on unchanged bytes would be conclusive.

The two signatures any real explanation must satisfy: **first pass wrong, second
right in the same process**, and **clean at or below one page**.

## Next steps

1. Read/disassemble busybox's md5 applet and libbb hash driver; instrument a
   locally built md5 applet. This is the highest-value step and follows directly
   from the algorithm split above.
2. Build a digest probe as `-static-pie` with a PIC-capable musl to settle
   hypothesis 2.
3. Land the probes. `readback` (with its `verify` / `stackverify` / `freshbuf` /
   `bssverify` arms), `computecheck`, `neonstate` and `rodatacheck` are the
   elimination table in executable form, and every one of them is calibrated to
   run unchanged on real Linux arm64. They belong in
   `userspace/forktest/c_stress/` next to `madvshared`/`cowstale`, and
   `mmapsum`'s digest-agreement idea is the closest existing precedent.
4. **Beware the pre-faulting trap.** The first version of `readback` `memset` its
   destination buffer before reading into it, which faults the whole buffer in
   and made every arm pass. Any probe written for this bug must leave the
   destination untouched, and must hash the *first* file it sees in a fresh
   process, or it will report health that isn't there.

## Background

- [`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md)
  § "Resolution" — where this was first seen, and the (wrong) page-cache guess
- [`MADVISE_WILLNEED_FILE_CORRUPTION`-class work](CARGO_HEAP_NULL_RC.md) — the
  file-page corruption family this superficially resembles and is not
- [`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
  — the premature-free / shared-page classes, and the RELR bug whose
  accumulation prediction is falsified above
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
  — `mmapsum`'s digest-agreement check, the existing probe closest in spirit

## Verify

The fix is in `sync_el1_handler` (`src/exceptions.rs`) and the guard is a boot
test, so the cheap check is the boot log:

```
  [PASS] test_user_copy_loop_differential_sweep (114048 cases, wide == byte everywhere)
  [test] el1_sync_exception_preserves_gprs: the EL1 abort dump below is deliberate
  [PASS] test_el1_sync_exception_preserves_gprs (1 abort(s), x4-x18 intact)
```

Two things to insist on in that output. **`1 abort(s)` is load-bearing**: the
probe stores to `0x0000_6000_0000_0000` and reports how far `SYNC_EC_EL1[0x25]`
moved, because the first version of the test stored to `0x1000` — which turned out
to be mapped during the boot suite — took no exception at all, and passed
vacuously on a kernel with the bug wide open. A `[FAIL] ... vacuous` line means
the chosen VA became mappable, not that the vector regressed. And the EL1
exception dump that follows the marker line **is** the test; a clean boot has 5 of
those, not 4.

One unrelated thing you will hit while doing repeat boots: `retired_reclaim_ab`
fails intermittently at `SMP=1` — measured 1 boot in 6 on an **unmodified** tree,
with the identical `OFF recovered 0p (retired 1), ON recovered 0p (retired 0)`
message. It needs two retired processes to exist by the time it runs and
sometimes finds one. It is not this fix, and it is not the boot test above.

End-to-end, on HVF (**not** TCG — TCG does not reproduce this class):

```bash
SMP=4 MEMORY=2048 cargo run --release &
python3 scripts/vm_ready.py 2222 600
# a self-identifying file: the 4-byte word at offset o holds o
python3 -c "import sys;sys.stdout.buffer.write(b''.join(o.to_bytes(4,'little') for o in range(0,65536,4)))" > ident.bin
# push it, verify st_size is 65536, then ONE iteration per FRESH exec:
#   /tmp/md5probe whole /tmp/ident.bin 1     -> expect badwords=0 every time
#   busybox md5sum /tmp/ident.bin            -> expect f67ea8aaa3735fcf05215a86495be8f7
```

`userspace/forktest/c_stress/md5probe.c` prints, on any failure, the histogram of
**in-page** offsets of the bad words. That histogram is the diagnostic worth
keeping: a first-bad offset made this look like one lost window for two sessions,
and the histogram named the registers in one run.

## What this cost, and the transferable lesson

Three sessions, ~15 userspace probes, and a dozen retired mechanisms — and the
cause was a register save list in the exception vector, which no probe could see.
Two habits would have shortened it:

- **When a symptom dies as soon as the code uses fewer registers, suspect
  registers.** The tier A/B already contained that signal: byte loop clean (x3
  only), widened loop broken (x3-x10). It was read as "wide stores are the
  problem" instead of "the extra registers are the problem", and the next
  experiment (a single-register 8-byte loop) was designed but never run.
- **Ask where the damage is, not where it starts.** Every report until the third
  session gave a first-bad offset and a count. One histogram of in-page offsets
  turned "90 lost bytes somewhere" into "x8, x9, x10, once per page" — and from
  there the vector is two greps away.

The other lesson is about this document: it carried "CAUSE FOUND" for a cause
that was one level too shallow, and a reproducer platform (TCG) that was exactly
backwards. Both cost real time in the session that inherited them. A confident
status line is worth re-measuring before you build on it.
