# The user-copy byte loop, and what it costs (2026-08-27)

Status: **history — implemented 2026-08-27.** Written up as an investigation
first, then landed; the plan below is what shipped, and § "Result" records the
measurement against it. Kept separate from the ext2 docs because the finding was
never an ext2 finding — it taxed every syscall that moves bytes across the
user/kernel boundary.

Companion: [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md) (the validation/trampoline
machinery this must not break), [`EXT2_PER_FD_INODE_READ_PATH.md`](EXT2_PER_FD_INODE_READ_PATH.md)
(the read-path work that led here).

---

## How this was found

Chasing "why is `seq_read` still 79x Linux after the read path stopped walking
directories". The answer turned out not to be I/O and not to be syscall overhead.

`ext2probe-host` reports **0 device reads** for `seq_read` — the write-back cache
serves it entirely from RAM. So the 11 ms for 2 MB is pure CPU. Fitting the
guest's `dd` timings across four block sizes (`scripts/benchmarks/read_path_ab.py
--sweep`, quiet host, HVF) separates fixed cost from streaming cost:

```
fixed  = 19.6 us per read(2)
stream = 5.26 ns/byte  ->  190 MB/s

  bs=  1024  measured    25.0 us   model    25.0 us
  bs=  4096  measured    37.8 us   model    41.2 us
  bs= 16384  measured   112.9 us   model   105.7 us
  bs= 65536  measured   364.1 us   model   364.1 us

2 MB at that stream rate = 11.0 ms   (measured seq_read = 11 ms)
```

The streaming term alone accounts for the whole operation. So `seq_read` is
bandwidth-bound at **190 MB/s**, on hardware-accelerated (HVF) aarch64 where a
memcpy should run in the GB/s.

## The read path copies every byte three times

1. `alloc::vec![0u8; to_read]` — allocate the staging buffer **and zero it**,
   then immediately overwrite every byte of it;
2. ext2 copies block-cache → staging buffer;
3. `copy_to_user` copies staging buffer → user.

Isolating step 2 by reading `/dev/zero` (same syscall arm, no filesystem):

| 8 MB, `bs=65536`, median of 5 | ns/byte | throughput |
|---|---:|---:|
| `/dev/zero` — zero-fill + `copy_to_user` | 4.73 | 202 MB/s |
| ext2 file — the above **+ block-cache → staging** | 5.02 | 190 MB/s |

**ext2's own copy costs 0.29 ns/byte — 3.4 GB/s.** That is a compiler-generated
`memcpy` on this exact machine, and it is the control that makes the rest of this
document a measurement rather than an opinion: the hardware is fine, and one of
the three passes proves it.

The other two passes cost **4.73 ns/byte between them — 16x more per byte.**

## The cause

`crates/akuma-exec/src/mmu/user_access.rs`, `global_asm!`:

```asm
__arch_copy_user_memory:            // x0 = dst, x1 = src, x2 = len
    cbz x2, 2f
1:  ldrb w3, [x1], #1               // ONE byte
    strb w3, [x0], #1               // ONE byte
    subs x2, x2, #1
    b.ne 1b
2:  mov x0, #0
    ret
```

Every byte crossing the boundary — `read`, `write`, sockets, `getdents`,
`statx`, all of it — moves one byte per four instructions, through a
loop-carried dependency.

**The arithmetic closes exactly.** A compiler `memcpy` moves 16 bytes per
iteration with about the same instruction count; this moves 1. So it should cost
~16x per byte, and 0.29 x 16 = **4.6 ns/byte predicted** against **4.73 ns/byte
measured**. Nothing exotic is going on — it is the width, and only the width.

That agreement is why this document predicts a number at all. D-4 in this same
subsystem (`EXT2_WRITEBACK_DESIGN.md`) predicted a read win from an *assumed*
cost breakdown and delivered nothing measurable; the difference here is that the
predicted rate is a rate this machine was measured doing, in the same syscall,
microseconds apart.

## Why is it byte-at-a-time?

**No documented reason, and no real one.** [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md)
§1 quotes the loop and describes the recovery mechanism, but the only property
anything depends on is:

> on an unmapped page the loop takes an EL1 data abort, and `src/exceptions.rs`
> (`EC=0x25` with `ELR` inside kernel code and a non-zero registered handler)
> **rewrites `ELR_EL1` to the trampoline** before returning — so the faulting
> instruction is never retried and the function returns `Err(EFAULT)`.

That is width-independent: the handler keys on "a fault handler is registered",
not on the faulting instruction's shape. It is the simplest thing that works,
written once and never revisited.

## What "copy a page at a time" would actually mean

AArch64 has no page-copy instruction, so the unit is not a page. The change is to
widen the *iteration*:

- `ldp x3, x4, [x1], #16` / `stp x3, x4, [x0], #16` — 16 bytes in two
  instructions;
- unrolled four ways — 64 bytes per iteration, which is Linux-grade;
- head/tail for the sub-16-byte remainder (lengths are not 16-aligned).

## Four constraints the replacement must respect

1. **It must stay a leaf, stackless function.** `__arch_copy_user_fault` is
   `mov x0, #14 ; ret` — it returns through `x30`, and that only works because
   the current loop never writes LR or `sp`. A wide version written with a normal
   prologue would, on a mid-copy fault, `ret` to whatever the frame happened to
   hold. **This is the sharp edge, and it is undocumented anywhere else.** Either
   keep the replacement leaf and stackless, or teach the trampoline to unwind.
2. **Unaligned access is safe here, but for a reason worth re-checking.**
   SCTLR_EL1.A (bit 1, data alignment check) is **0**: `src/boot.rs` forces SA
   and SA0 off and leaves A at its reset value, and both recorded reset words
   have bit 1 clear (QEMU virt `0x3490d185`, Firecracker/KVM `0x34c5d1dd`). So
   unaligned `ldp`/`stp` to **Normal** memory is architecturally fine. Device
   memory faults on multi-register access regardless of A — confirm no user VA
   can be Device-mapped before relying on this.
3. **Registers.** The current loop clobbers only `w3`. A wide version needs
   several; stay inside the AAPCS64 caller-saved set (`x3`–`x17`).
4. **Partial-write semantics are unchanged.** A fault mid-copy already leaves the
   destination partly written, and the contract is a bare `EFAULT`, not Linux's
   bytes-not-copied. A 16-byte store faulting halfway is the same class of
   partial write the byte loop already permits. No regression, and no caller
   depends on the granularity.

## Result

Landed as 64/16/8-byte chunks with a byte tail. Measured the same way the problem
was found — 8 MB at `bs=65536`, median of 5, quiet host:

| | byte loop | widened | |
|---|---:|---:|---:|
| `/dev/zero` (zero-fill + `copy_to_user`) | 4.73 ns/byte, 202 MB/s | **0.56 ns/byte, 1707 MB/s** | **8.4x** |
| ext2 file (+ block-cache → temp) | 5.02 ns/byte, 190 MB/s | **1.28 ns/byte, 746 MB/s** | **3.9x** |

The prediction was "0.6 ns/byte if the user copy reaches what the in-kernel
`memcpy` already does". Measured **0.56**. That is the payoff for anchoring a
prediction on a rate the same machine was measured doing in the same syscall,
rather than on theory — see the note under "The cause".

`ext2probe`, 3 runs, against the pre-change numbers:

| op | before | after | |
|---|---:|---:|---:|
| **`seq_read` 2 MB** | 11 ms | **5 ms** | **-51%** |
| create 300 | 1370 ms | 1375 ms | — |
| seq_write 2 MB | 656 ms | 654 ms | — |
| delete 300 | 697 ms | 740 ms | noise |
| build 3200-file tree | 14.6 s | 14.8 s | noise |
| mass-delete 3200 | 5.48 s | 5.42 s | — |

Only `seq_read` moves, which is the expected shape: every other op is bound by
synchronous device writes, not by bytes crossing the boundary. Against Linux ext2
`-o sync` (0.14 ms) `seq_read` goes from **79x to 36x**.

### Where the prediction was wrong, and what it means for what comes next

The table above predicted `seq_read` would reach ~2 ms. It reached 5 ms, because
that estimate treated the whole operation as per-byte: 2 MB x 0.9 ns/byte. But
`seq_read` is 256 reads of 8 KB, and at ~17 us of **fixed cost per `read(2)`**
that is 4.4 ms before a single byte moves. 4.4 ms fixed + ~1.2 ms of bytes ≈ the
5 ms measured.

So this change **flipped `seq_read` from byte-bound to syscall-fixed-cost-bound.**
That reorders the remaining work: with the per-byte term now 9x smaller, the
~17 us per-syscall fixed cost is the dominant term in a warm read, and finding
where it goes (syscall entry/exit, `validate_user_range`'s per-page table walks,
the BKL guard, the staging allocation) matters more than removing further
allocations — measured at 2026-08-27 to be invisible individually
(`EXT2_PER_FD_INODE_READ_PATH.md`).

## The other byte-at-a-time path, which this did **not** fix

`copy_from_user_str` (`src/syscall/mod.rs`) — the fetch every `*at` syscall uses
for its path argument — does not reach the widened loop in any useful way. It
calls `copy_from_user_byte` **per character**, and each of those is a full
`copy_from_user_safe`: two `BYPASS_VALIDATION` atomic loads, a fault-handler
store, a compiler fence, the asm call for *one byte*, another fence, a handler
clear. Plus `bytes.push(c)` onto a `Vec::new()` with no reserved capacity.

Unlike the main loop, this one has a **real reason** to be careful: you cannot
read past the NUL. A blind 512-byte read for a 10-byte path could fault on the
next page and turn a valid call into `EFAULT`. Linux's `strncpy_from_user`
handles it by copying up to the **page boundary** and scanning for the NUL
in-kernel; that is the fix here too, and it would replace ~20 handler setups with
one for a typical path.

**Sized honestly before anyone prioritises it:** roughly 0.3-0.6 us per
path-taking syscall, against an `openat` that also runs several real directory
walks. About 1% of that syscall. The metadata operations that are 19x-129x Linux
are bound by synchronous device writes, not by string copying, so this is a
genuine defect and a cheap one, but it is not where those gaps live. Folded into
the per-syscall fixed-cost work rather than treated as its own win.

## The zero-fill, while we are here

`alloc::vec![0u8; to_read]` in `sys_read`/`sys_pread64` memsets a buffer that the
filesystem overwrites in full on the very next line. It is one of the three
passes and it buys nothing. Three ways out, in increasing order of value and
difficulty: drop the zero-fill but keep the allocation; keep a reusable
per-thread staging buffer; or read straight into user memory and delete the
staging pass entirely (hardest — the filesystem would be writing into user pages
while holding its state lock, so the fault discipline needs real thought).

## Verification performed

`test_user_copy_wide_and_faults` (`src/tests.rs`, boot self-test — none of this
is host-testable, it is `global_asm!` running at EL1):

1. **81 lengths x 8 x 8 alignments** copy exactly, with the bytes on either side
   of the destination range left untouched so an over-copying chunk loop is
   caught. Covers every path through the 64/16/8/tail ladder and every relative
   alignment.
2. A fully unmapped source returns `EFAULT`.
3. **The one that matters: a fault part way through an 8 KB copy.** Observed
   `x2=0x1c00` — 1024 bytes already copied — returning `EFAULT` *and returning at
   all*, which is what proves `x30` survived (invariant 1). A version with a
   prologue passes cases 1 and 2 and then jumps to garbage here. The exception
   trace confirms the fault landed on `0xa9401023` = `ldp x3, x4, [x1]`, i.e.
   inside the new 64-byte loop rather than the byte tail.

Plus: `cargo clippy --release` clean, `cargo build --release` and
`scripts/build_extreme_size.sh` clean, full boot with every self-test passing,
and the guest measurements above on a quiet host — this session measured the same
arms **20x apart** under load (`EXT2_PER_FD_INODE_READ_PATH.md` § Background).
