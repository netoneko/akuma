# `md5sum`/`sha*sum` return wrong digests for correct bytes

**Date:** 2026-08-28
**Status:** **OPEN — reproduced and heavily localized, not root-caused.**
**Grade: B** — every measurement below was run end-to-end on 2026-08-28 at
`SMP=4`, `MEMORY=2048`, on the default `cargo build --release` kernel. The
elimination table is the durable part; the hypotheses at the end are not
confirmed.

## Summary

`busybox md5sum` returns a **wrong, non-deterministic digest** for an unmodified
file, roughly **40–50 % of invocations**, for files larger than one page. The
bytes are not wrong: the same file verifies byte-exact through `read(2)` and
through `mmap`, and `busybox cksum` and `busybox base64` — which read every byte
of the same file — are perfectly stable across runs.

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

## Leading hypotheses, none confirmed

1. **libbb's shared hash driver.** md5/sha1/sha512 are the failing set and they
   share `libbb`'s hash plumbing (a common read buffer — busybox has a global
   `bb_common_bufsiz1` — plus a context struct carried across chunk boundaries).
   `cksum`/`base64` do not use it. The next measurement is to read busybox's
   source for that driver and instrument it, or disassemble the md5 applet, and
   find what it touches that a stateless chunk loop does not.
2. **PIE + RELR.** busybox is a PIE with a `.relr.dyn` section; **every** probe
   above is `-static` non-PIE, which is a real confound. This is **untested**,
   and three routes to testing it were tried and are all blocked in this
   environment:
   - `-static-pie` with the local `musl-cross` 0.9.11 fails to link
     ("read-only segment has dynamic relocations" — its `libc.a` is not PIC).
   - `apk add coreutils` for a non-busybox `md5sum` fails: the guest could not
     reach the network, and `/bin/md5sum` is only a symlink to busybox.
   - A dynamically-linked PIE cannot run in the guest at all — there is no
     `/lib/ld-musl-aarch64.so.1`.

   Unblocking needs a PIC-capable musl on the host, or a working `apk` path.

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
