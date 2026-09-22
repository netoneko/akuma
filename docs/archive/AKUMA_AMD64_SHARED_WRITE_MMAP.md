# amd64: writable `MAP_SHARED` — the refusal that hid three bugs

**Date:** 2026-09-22. **Found by:** akuma-miot's `dist/storeprobe` finally
being run on the bare-metal box, after months of being shipped and never
executed. **Fixed:** one designed-in refusal replaced with a real
implementation, plus two silent-wrong-answer bugs it had been shielding.

Companion reference (current state, no narrative):
[`../reference/subsystems/amd64-shared-write-mmap.md`](../reference/subsystems/amd64-shared-write-mmap.md).
The caller's side of the story — how the probe was written, what it costs to
ship a diagnostic and never run it — lives in akuma-miot's `docs/TOPOLOGY.md`
(`node5` section).

---

## The symptom: success at everything except the one thing that matters

`storeprobe` is a seven-stage ParityDB probe: fresh open, append, read back,
compact, rewind, **reopen across a process boundary**, one large value. On
the bare-metal box it printed:

```
[db] 1 open (creates files, maps them)
[db] 2 append 256 blocks
[db] 3 read back
[db] 4 compact (a state blob, and the blocks beneath it dropped)
[db] 5 rewind to the compaction, discarding our own tail
[db] 6 reopen — does it survive a process boundary?
thread 'main' panicked at storeprobe.rs:63: reopen: Db(Io(Os { code: 38,
kind: Unsupported, message: "Function not implemented" }))
```

Stages 1–5 green. The failure was *exactly* at the stage whose whole job is
`mmap` an existing file read-write — the shape `sys_mmap` had refused by
design since file mappings landed:

```rust
// Still refused: a **writable `MAP_SHARED`** file mapping. Writes
// through it must become visible in the file and to every other mapper,
// which needs a write-back path and one shared frame per file page —
// that is the page cache, and it is genuinely not here.
if plan.is_shared_writable {
    return errno::ENOSYS;
}
```

The refusal was honest, documented, and correct about the danger. It was
also shielding three other bugs, which is the actual story of this page.

> **The rule this earns:** a deliberate `ENOSYS` is a claim about *your*
> kernel, but it also silences every caller that would have found the next
> bug. `storeprobe` was green through five stages precisely because the
> refusal made the sixth one unreachable. A refusal is a wall you built on
> top of ground you have never surveyed.

## Pinning it: one probe, four mmap shapes

`mmapprobe` (written on the spot, `akuma-miot/crates/miot-store/src/bin/`)
maps one page each of: anonymous RW, file `MAP_PRIVATE` RO, file
`MAP_SHARED` RO, file `MAP_SHARED` RW. On the metal:

```
[mm] 1 anonymous RW:   ok ptr=0x100006000
[mm] 2 file PRIV RO:  ok ptr=0x100008000
[mm] 3 file SHARED RO:ok ptr=0x100009000
Segmentation fault              ← shape 4 never printed
```

**Segfault, not errno.** The refusal's *reason* lived in the doc comment and
was returned as a clean `ENOSYS` from the syscall path — but a raw
`syscall`-instruction caller died. (The refusal sat inside `sys_mmap`'s
`plan`-gated arm; the probe's fourth shape reached a different, unhappier
end. Do not assume a documented errno is what every caller sees.)

## The implementation, and the reserve that killed the first draft

The obvious implementation — eager-fill the mapping from the file, keep
frames in the region, write back on teardown — was written first and worked
for every hand probe. It is also wrong, and the thing that proved it was
parity-db's source, read *after* the new kernel still wedged at reopen:

```rust
// parity-db file.rs
const RESERVE_ADDRESS_SPACE: usize = 1024 * 1024 * 1024; // 1 Gb
let map_len = len + RESERVE_ADDRESS_SPACE;
memmap2::MmapOptions::new().len(map_len).map_mut(file)
```

**Every parity-db file mapping is `len + 1 GiB`.** Eager fill of that is a
gigabyte of zero frames for address space nobody has touched. The old
`plan()` doc had said shared-writable mappings must "stay resident so their
pages can be written back" — residency is exactly what a reserve-style
caller cannot afford. The fix inverts the rule: shared-writable mappings
take the **lazy** path like every other file mapping (per-fault fills read
the file's *current* bytes, so growth under a long-lived mapping just
works), and coherence is delivered by the write-back instead:

- `munmap` snapshots the overlapping shared-writable regions *before*
  detaching, then flushes;
- `msync` (x86_64 nr **26**, routed before the shared syscall table —
  asm-generic has no msync and the abi table's invariant forbids faking a
  generic twin) flushes the range;
- `madvise(MADV_DONTNEED)` flushes a page before zeroing it, because here
  the frame is the only copy — on Linux the page cache already has the
  writes, which is why Linux's drop-and-refault is free and ours is not.

The flush walks the region's **present leaves**, not the region's frame
list (an eager fill does not populate that list, and a CoW break does not
update it), and consults the file's size **at flush time** — a record taken
at `mmap` would exclude everything written into space the file grew into,
which is the entire point of the reserve.

Lock hygiene mattered more than usual: the first draft did `write_at` under
the region lock. Ext2 I/O under the mmap lock is a new
regions → disk edge in a module whose lock order
(regions → address space → PMM) is load-bearing everywhere else. Every
version after the first collects jobs under the locks and writes after they
drop.

## What the mmap work flushed out: two bugs behind the wall

### Bug 1 — ext2 `truncate` answered `Ok(())` for extend

With `MAP_SHARED` writable, parity-db got past reopen and started losing
data: `set_len(8192)` → map → write header → `msync` → remap → header
reads as **zeros**, and `metadata` said the file was 0 bytes. A probe
pinned it:

```
[t9] ftruncate(8192) rc=0          ← success!
[t9] pread after ftruncate = 0     ← nothing extended
```

The code said it out loud:

```rust
if length > current_size {
    // Extending would require allocating blocks - not implemented
    // For bun's use case, this is fine (it truncates to shrink)
    return Ok(());          // ← the silent wrong answer
}
```

Success plus no effect — the failure mode this tree's own docs call the
worst answer a syscall can give, and it was load-bearing: every
`ftruncate`-extend (parity-db's `set_len` before every mapping write, any
`fallocate`-shaped caller, any journal) got `0` and a zero-byte file.
Extension now allocates zero blocks through the write path's own
`ensure_block(_, zero_leaf = true)`, zeroes the tail of the old EOF's
partial block, and reports `ENOSPC` honestly if allocation fails. Host
test: `akuma-ext2::tests::ftruncate_extends_with_zero_blocks`.

> **The rule this earns, restated from
> [`AKUMA_AMD64_MEMORY_CLOSEOUT.md`](AKUMA_AMD64_MEMORY_CLOSEOUT.md):** an
> unimplemented case must return the error that *says so*. `Ok(())` for
> "didn't do it" converts every downstream diagnostic into a hunt for a
> different bug — here it cost a full probe cycle before anyone looked at
> `truncate`, because the kernel had *promised* the extension happened.

### Bug 2 — `posix_fadvise` had no table row

Next reopen failure: `Db(Io(Os { code: 38 ... }))` again — but this time
the console said why:

```
[syscall] no row for x86_64 nr=221 — returning ENOSYS
```

`nr 221` is `fadvise64`. parity-db calls `posix_fadvise(fd, 0, 0,
POSIX_FADV_RANDOM)` after opening *every* file and `try_io!`s the result —
one ENOSYS aborts the entire open. Added as a real row (x86_64 221,
asm-generic 223 — the two ABIs genuinely differ here, 221 being `execve` on
arm64, which is exactly the confusion the abi table's
`collisions_that_would_corrupt` test exists to catch; the first attempt
used 233, which is `madvise`, and the decode tests named it immediately).
The handler returns 0: advisory by Linux's own contract, and there is no
readahead state to tune. The miss had survived because nothing in the
guest's usual workload — busybox, cargo, git, sshd — ever issued it.

> **The rule this earns, restated from
> [`RUST_TOOLCHAIN_AMD64.md`](RUST_TOOLCHAIN_AMD64.md):** the console line
> `[syscall] no row for x86_64 nr=…` is the fastest diagnosis in this
> kernel. Read it before forming any theory about "Function not
> implemented".

## The dead ends, because they cost real time

The middle of the day was spent chasing a reopen fault that **neither** fix
explained — a ring-3 `#PF` at an address "outside every region", at
parity-db's `rip`, reproducibly after `mmap(len=1)` + `madvise` + `close` +
two signal calls. It motivated a series of single-shape probes, all of
which **passed**, which is how each suspect was eliminated:

| probe | shape | result |
|---|---|---|
| t2 | malloc churn + `pthread_create` | my own probe had a `1 << (i%12)*100` shift overflow — the crash was mine, not the kernel's |
| t3 | plain malloc | ok |
| t4 | `pthread_create` + join | ok (threads work) |
| t5 | `mmap(len=1, SHARED, RW)` → `madvise(DONTNEED)` → store → `msync` → `munmap` | ok |
| t6 | brk growth, 8 rounds, every page touched | ok |
| t7 | 1 GiB reserve mmap, header write, `msync`, `munmap`, heap churn | ok |
| t8 | four simultaneous reserves, growth + deep writes, remap-and-verify | **caught bug 1** (headers read as zeros) |

t2's lesson is worth keeping: **the probe that crashes may be crashing
itself.** `1 << 1164` is UB, `malloc`'s failure mode here is a null that
`memset` happily dereferences, and a static musl binary faults exactly like
a kernel bug would. Verify the probe against the stock kernel (or against
arithmetic) before promoting its crash to a finding.

The kernel-side instrument that actually localized things was added to
`user_fault` and kept: on a ring-3 kill, print whether `cr2` is inside a
recorded mmap region (and which kind), "outside every region", or
unknowable — capped at 8 regions so the 512-byte `StackWriter` does not
overflow silently. "Inside a region" means the kernel lied (a page it
recorded was not there); "outside every region" means the program wrote
past its mappings, or something unmapped them. That one line is the
difference between debugging the kernel and debugging the caller.

## Verification

- `storeprobe`: **all 7 stages**, on the bare-metal box and under the amd64
  QEMU loop, exit status 7.
- Boot suite (`amd64_trials.py --local-only --smp 1`): 731 passed, 1 failed
  — the pre-existing `the lazy path was actually taken` flake, identical on
  the stock kernel (it counts demand-faults from whichever programs the
  suite happened to run; it is a composition-dependent counter wearing a
  pass/fail coat, and it failed on stock the same day).
- `akuma-mmap` host tests: clip/split propagation of the new record, CoW
  child drops it, offset arithmetic. `akuma-ext2`: the new truncate test
  plus the full 107.
- akuma-miot's `miot node` on the metal: kill/reboot over a grown database,
  herd-supervised, replaying 1000+ blocks, taking signed extrinsics from
  the laptop.

## Deployed-state note

The kernel installed on the trashcan (uname reports the build stamp `22ac34d3`)
is this tree's `akuma-amd64`, `/boot/akuma-amd64`, md5- and
multiboot2-header-verified before the reboot, `.prev`/`.good` fallbacks
intact. `/bin/herd` on that box was also replaced the same day: its build
predated config reload, so services added to `/etc/herd/enabled/` after boot
never started and the failure read as a spawn bug. If a herd service "won't
start", first check *when* its conf appeared relative to the herd binary's
build date.
