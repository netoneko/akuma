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

## Leading hypotheses, none confirmed

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
