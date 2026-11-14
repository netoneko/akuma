# Taoxichuan's loader replaced a page two segments share

2026-11-04. `akuma-tokio-kernel-core::loader::place_segment`, both machines.

## Symptom

On aarch64 Taoxichuan, `init=/bin/sshd` printed `[SSHD] Starting userspace SSH
server...` and died at once:

```
[Fault] EL0 ec=0x20 elr=0x416060 far=0x416060 esr=0x8200000f pte=0x60000041605fcf -- task killed
```

An instruction abort with a level-3 *permission* fault, on a page of `.text`.
The leaf (now printed in the EL0 fault report) is user, read-only, `PXN|UXN`.

## Cause

The disk's `sshd` (a static libakuma binary, linked by `lld`) has

| `PT_LOAD` | vaddr | filesz | flags |
|---|---|---|---|
| 0 | `0x400000` | `0x16D00` | R X |
| 1 | `0x416D00` | `0x3238` | R |

so page `0x416000` holds the end of `.text` and the start of the read-only
segment. `place_segment` builds a page that a segment only partly covers
eagerly: a zeroed frame, that segment's bytes copied in, mapped with that
segment's permissions. A trace of every leaf write to `0x416000` showed it
mapped twice during the load: once by segment 0 (`...4fcf`, executable), then
by segment 1 with a **new** frame (`...5fcf`, read-only). The second mapping
dropped segment 0's exec bit, and also its bytes (`0x416000..0x416D00` read as
zeros), so the bytes on that page were lost even before the permission fault.

Busybox and every musl binary tried before have page-aligned segments, which is
why A5.6b to A5.8 never hit it. The code is shared, so amd64 would
fault the same way on such a binary; the amd64 battery passes, but the amd64
binaries were not checked for shared pages.

## Fix

`place_segment` takes the image's whole segment list. When it builds an eager
page it copies in the file bytes of **every** segment overlapping that page,
and it skips a page that is already present (a valid leaf, not a lazy record),
so the first segment's permissions stand. That is the main kernel's rule
(`akuma-elf`'s `map_segment_eager`: the first mapping wins, later bytes are
written into it). The copy also bounds-checks the image slice instead of
panicking on a malformed `p_offset`.

## Verification

- aarch64 TCG: `init=/bin/sshd` runs; `[SSHD] Listening on 0.0.0.0:22`; keys
  load, publickey auth verifies.
- With the linux-abi sshd (`--features libakuma/linux-abi`, clone+execve,
  what Taoxichuan runs on amd64): ssh sessions run `uname`, pipes, `wget`,
  and the probe battery (`tokioprobe` PASS, `killtree 8` PASS, `pidfdprobe`
  PASS, `wait` ok, 0 faults).
- amd64 probe battery at smp=1 and smp=4: all PASS, 0 faults.

## Still open

- The permissions of a shared page are the first segment's. If an `R` segment
  and a following `RW` one shared a page, writes to the start of the data would
  fault. The main kernel does the same; no binary in the tree is laid out that
  way.
