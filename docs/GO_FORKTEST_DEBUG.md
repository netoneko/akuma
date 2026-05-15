# Go Forktest Crash Analysis

This document details crash patterns seen when running `forktest_parent` with **stress flags** (especially **`-combined_stress`**, **`-mmap_test`**, or **`-file_io`**) on Akuma OS. The **child** often shows `addr=0x2` in Go's allocator; the **parent** can fault in **`read()`** on the epoll pipe with a **heap-range** fault address; **`-file_io`** can also contribute to **deadlocks** (guest or SSH) via temp-file traffic on ext2 (see [Isolation matrix](#isolation-matrix-2026-04-14)).

## Current status (2026-05-15 — all patterns fixed; full matrix green)

### Fixed: Pattern 4 — QEMU EC=0x15 misrouting for `stp xzr, xzr` in Go 1.26 GreenTeaGC (`crush`)

Same root-cause class as Pattern 1 (QEMU generates EC=0x15 instead of EC=0x25 for a non-SVC instruction), but for a **different instruction** in a **different binary**. The DC ZVA fix did not catch it.

**Binary:** `/bin/crush` — a Go 1.26.1 binary with GreenTeaGC (`toolchain@v0.0.1-go1.26.1`), CGo-enabled, statically linked via `aarch64-linux-musl-gcc`. Source: `userspace/crush/crush/` (Go), built by `userspace/crush/build.rs` (Rust build script).

**Crash sequence (crash36.log lines 61569–61589, T157.84):**

```
[TRAMP] tid=15 alt_sp=0x0
[EFAULT] nr=48 pid=142 tid=15 ELR=0x432e40 args=[0x1e1c4df80, 0x12, 0x1, 0x4d, ...]
[DA-MISS] pid=142 ppid=0 va=0xfffffffffffffff2 lr_count=31
[WILD-DA] *** FAR=0xfffffffffffffff2 is -14 (EFAULT) - syscall error used as pointer! ***
[WILD-DA] pid=142 FAR=0xfffffffffffffff2 ELR=0x432e40 last_sc=18446744073709551615
```

User-visible panic:
```
[signal SIGSEGV: segmentation violation code=0x1 addr=0xfffffffffffffff2 pc=0x432e40]
runtime.(*spanInlineMarkBits).init  mgcmark_greenteagc.go:139 +0x20 pc=0x432e40
runtime.(*mspan).moveInlineMarks    mgcmark_greenteagc.go:215
runtime.(*sweepLocked).sweep        mgcsweep.go:656
runtime.bgsweep
```

**Disassembly of crush at 0x432e40** (`bootstrap/bin/crush`, ET_EXEC non-PIE):

```
432e38: cbz  x0, 0x432e70
432e3c: tbz  w2, #0x0, 0x432e60     ← NOT an SVC
432e40: stp  xzr, xzr, [x0]         ← misrouted instruction (0xa9007c1f)
432e44: stp  xzr, xzr, [x0, #0x10]
432e48: stp  xzr, xzr, [x0, #0x20]
...
432e5c: stp  xzr, xzr, [x0, #0x70]  ← 8× 16-byte zero-stores = 128-byte zero block
```

**Why QEMU misroutes it:** `spanInlineMarkBits.init` zeroes a 128-byte inline mark bitmap using 8× `stp xzr, xzr`. When x0 (the bitmap base) is in a PROT_NONE lazy region (Go's `sysReserve` arena, not yet committed by `sysMap`), QEMU generates EC=0x15 instead of EC=0x25. ELR is set to the `stp` instruction itself (same QEMU behaviour as DC ZVA: ELR = faulting instruction, not +4).

**Why the DC ZVA fix missed it:** The check at `src/exceptions.rs` was `(instr & 0xFFFFFFE0) == 0xD50B7420` — DC ZVA only. `stp xzr, xzr, [x0]` encodes as `0xa9007c1f`, which doesn't match.

**Consequence (pre-fix):** `handle_syscall` was called with x8=48 (`faccessat`, leftover from a prior syscall). `faccessat` validated x1=0x12 as the path pointer → EFAULT. x0 was overwritten to `0xfffffffffffffff2`. Goroutine resumed at ELR=0x432e40, executed `stp xzr, xzr, [0xfffffffffffffff2]` → translation fault → `[DA-MISS]` → SIGSEGV.

### Fix (`src/exceptions.rs`, 2026-05-15)

Added `decode_stp_xzr_xzr` and `emulate_stp_xzr_xzr` helpers, plus a second misrouting detector block in the `EC_SVC64` handler immediately after the DC ZVA block.

**Decoder:** mask `(instr & 0xFFC0_7C1F) == 0xA900_7C1F` matches all signed-offset `stp xzr, xzr, [Xn, #N]` variants (clears imm7 bits [21:15] and Rn bits [9:5]; checks Rt=xzr and Rt2=xzr). Rn and byte offset are extracted as:

```rust
let rn = ((instr >> 5) & 0x1F) as usize;
let imm7 = ((instr >> 15) & 0x7F) as i32;
let imm7 = (imm7 << 25) >> 25; // sign-extend 7-bit
let offset = (imm7 as i64) * 8;
```

**Emulation sequence:**
1. Read instruction at ELR; check it matches `decode_stp_xzr_xzr`.
2. Read ELR-4; confirm it is not an actual SVC (`(i & 0xFFE0001F) == 0xD4000001`).
3. Compute `store_va = frame[Rn] + offset`.
4. If `store_va` is in a PROT_NONE lazy region, demand-page it as RW_NO_EXEC (same logic as the Pattern 3 DA-NONE handler).
5. Write 16 zero bytes to `store_va` via `copy_to_user_safe`.
6. Advance `frame.elr_el1 += 4`; return `frame.x0` unchanged (preserves caller's x0 across the fake syscall).

Each of the 8 subsequent `stp xzr, xzr, [x0, #N]` stores triggers its own EC=0x15 trap and is handled independently — the page is already mapped after the first one, so steps 1–4 become a fast path on the remaining seven.

### Verification (2026-05-15)

`stp_test_go` and `stp_test_c` binaries (`userspace/stp_test/`) exercise three offset variants each against freshly `mmap(PROT_NONE)`-mapped regions; the C binary additionally tests the exact `#0x70` offset from the crush crash. Both binaries print `ALL PASSED` and exit 0 on the patched kernel.

In-kernel unit tests (`src/process_tests.rs`):
- `test_stp_xzr_misroute_decode` — validates the decoder against 7 encodings and 5 non-matching instructions.
- `test_stp_xzr_emulation` — verifies the 16-byte zeroing lands at the correct offset in a kernel heap buffer.

---

### Root cause: QEMU TCG DC ZVA misrouting

`crash31.log` (lines 3054–3079) reveals the complete crash chain for the child at `pc=0x86840`:

```
[clock-diag] large clock_id=0x1e059e000 tp=0x21fc0 ELR=0x86840   ← QEMU EC=0x15 misroute
[EINVAL] nr=113 pid=94 tid=17 ELR=0x86840                          ← clock_gettime returns EINVAL
[signal] deliver sig=23 fault_pc=0x86840                           ← SIGURG saves x0=EINVAL, pc=0x86840
[sigreturn] pc=0x86840                                             ← goroutine resumes x0=EINVAL
[WILD-DA] pid=94 FAR=0xffffffffffffffea ELR=0x86840                ← dc zva x0=EINVAL → fault
```

**What QEMU is doing wrong:** `forktest_child` disassembles as:

```
0x8683c: sub  x1, x1, x5    // loop: remaining -= block_size
0x86840: dc   zva, x0        // zero cache line at x0  ← DC ZVA here
0x86844: add  x0, x0, x5    // x0 += block_size
0x86848: subs x1, x1, x5
0x8684c: b.hs 0x86840        // loop
```

QEMU TCG generates **EC=0x15 (SVC)** for the `dc zva, x0` at 0x86840, with ELR pointing at the DC ZVA instruction itself (not at the non-existent SVC+4). For real SVCs, ELR = SVC + 4; here ELR = the DC ZVA address, and the instruction at ELR-4 is `sub x1, x1, x5` — not an SVC.

**Why x8=113:** x8 holds the syscall number for `svc #0`. Go's `nanotime1` sets `mov x8, #0x71 (113)` then calls `svc #0`; after that SVC returns, x8=113 is still in the register file (the kernel saves/restores all GPRs across a syscall). The Go runtime does not clear x8 between nanotime and the subsequent `mallocgcLarge → memclrNoHeapPointers` call chain. When QEMU misroutes DC ZVA as EC=0x15, Akuma's SVC dispatcher reads x8=113 and calls `sys_clock_gettime` with x0 = the DC ZVA target address (a heap pointer ~`0x1e0…`) as the `clock_id`.

**Why EINVAL:** `sys_clock_gettime` has a guard — `clock_id > 0x10000000` → EINVAL — intended to catch heap pointers accidentally passed as clock IDs. This fires here, returning EINVAL (0xffffffffffffffea).

**Why the goroutine dies:** After `handle_syscall` returns EINVAL, line 2096 in `exceptions.rs` does `(*frame).x0 = ret` (stores EINVAL into the trap frame's x0 field). SIGURG (goroutine preemption) is pending and gets delivered at this exact syscall return boundary: `try_deliver_signal` builds a signal frame on the goroutine's altstack, saving `{pc = frame.elr_el1 = 0x86840, x0 = EINVAL}`. The Go signal handler (`handler=0x87160`) runs, then returns via `rt_sigreturn`. `do_rt_sigreturn` restores the saved `{x0=EINVAL, pc=0x86840}` to the goroutine. The goroutine resumes at 0x86840 — the DC ZVA instruction — but now x0=0xffffffffffffffea. `SCTLR_EL1.DZE=1` allows DC ZVA to execute without trapping (EC=0x18 no longer fires), so the CPU attempts to zero the cache line at 0xffffffffffffffea → translation fault → `[WILD-DA]`.

### Fixes (`src/exceptions.rs`, 2026-05-10)

**Fix 1 — EC=0x15 QEMU misrouting detector (primary):**

Inserted in `rust_sync_el0_handler` before `handle_syscall` (after the JIT-retry block, before rt_sigreturn). On every EC=0x15 entry:

1. Read 4 bytes at ELR — check if it is a DC ZVA instruction: `(instr & !0x1F) == 0xD50B7420`. The low 5 bits are the Xt register number; all other bits are fixed for any `dc zva, Xn`.
2. If yes, read 4 bytes at ELR-4 — check if it is **not** an SVC: `(instr & 0xFFE0001F) != 0xD4000001`. This distinguishes the QEMU misrouting (ELR points at DC ZVA; ELR-4 is a non-SVC instruction) from the legitimate case where a real SVC is immediately followed by DC ZVA (`svc #0; dc zva, x0`), which would have an SVC at ELR-4 and should be handled normally.
3. If both hold: decode Xt from `instr & 0x1F`, read `frame[Xt]` to get the zero-target address, call `emulate_dc_zva(addr)`, set `frame.elr_el1 = elr + 4` (advance past the DC ZVA), return `frame.x0` (original goroutine x0, unmodified). The assembly stub stores the return value into x0 on ERET, so the goroutine resumes at DC_ZVA+4 with x0 = the original heap address, as if the instruction had executed normally.

This means `handle_syscall` is never called, `frame.x0` is never overwritten with a syscall return value, and there is nothing bad for SIGURG to save in a signal frame.

**Fix 2 — EC=0x18 DC ZVA emulation via `emulate_dc_zva` (secondary):**

Added `pub(crate) fn emulate_dc_zva(addr: u64)` (`src/exceptions.rs` ~line 687). Reads `DCZID_EL0`: if bit 4 (DZP) is set, DC ZVA is prohibited — return silently. Otherwise, `block_size = (4 << BS).min(2048)` (QEMU uses BS=4 → 64 bytes), aligns `addr` to the block boundary, and zeroes `block_size` bytes via `copy_to_user_safe`. Also wired into the EC=0x18 handler's `CRm==4` branch so that if QEMU does generate EC=0x18 (e.g. with DZE=0), the handler actually zeroes memory instead of silently skipping.

**Fix 3 — `SCTLR_EL1.DZE=1` (`src/boot.rs`, prior session):**

Sets bit 14 in SCTLR_EL1 at boot so that DC ZVA from EL0 is architecturally permitted without a trap. Confirmed effective: after this fix, invalid DC ZVA addresses generate EC=0x25 (data abort) rather than EC=0x18 (system trap), which is correct CPU behavior. QEMU TCG nonetheless continues generating EC=0x15 for DC ZVA, so Fix 1 is still required.

### Kernel boot test

`test_dc_zva_emulation` in `src/process_tests.rs`: reads `DCZID_EL0.BS`, checks block size is a power-of-two ≥ 4, allocates a heap buffer, fills it with 0xAA, issues `dc zva` from EL1 (no trap needed in EL1 regardless of DZE), and verifies the block is zeroed. Boot log shows `[Test] dc_zva_emulation PASSED`.

### What remains

Parent-side crash at `fault_pc=0x13060` (SIGSEGV in pipe `read` / `runtime.read`) is a separate pattern unrelated to DC ZVA — see §Interim conclusions and §Pattern 2 below.

---

## Pattern 2 root cause analysis (crash32.log, 2026-05-10)

`crash32.log` provides clean evidence of the parent crash in a run where the DC ZVA child crash was already fixed. The child (`forktest_child` pid=97) exits `code=0` at T60.43 after 30.27s; the parent (`forktest_parent` pid=100) crashes at T178.70 after only 1.57s. The DC ZVA fix eliminated the child crash but exposed the parent crash in isolation.

### crash32.log crash sequence (T178.55–T178.70)

```
[T178.55] [pipe-read] enter pid=100 tid=8 fd=4 pipe=34 buf=0x1e0087708 cnt=1024
[T178.56] [epoll]    ev[0] data=0x4 IN
[signal] tkill(tid=17, sig=23)
[T178.58] [signal] deliver sig=23 slot=17 handler=0x87160 fault_pc=0x87064
[T178.58] [sigreturn] restoring: sp=0x1e0083c90 pc=0x87064 pstate=0x80000000
[T178.59] [SIGSEGV-HEAP] pid=100 far=0x1e0a47000 elr=0x402b38a0 iss=0x47
[T178.59] [signal] deliver sig=11 slot=8 handler=0x86ee0 fault_pc=0x13060
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x40435e28, FAR=0x1e0a47000, SPSR=0x80002345
  Thread=17, TTBR0=0xcc00005958a000, TTBR1=0x404b6000
  SP=0x583048d0, SP_EL0=0x1e0066c30
  Instruction at ELR: 0x38001403   (strb w3, [x0], #1)
[T178.70] [exit_group] pid=100 name=/bin/forktest_parent code=2 after 1.57s
```

Key observation: the `[SIGSEGV-HEAP]` log and the `[Exception] Sync from EL1` log are produced by the same page fault — but they fire under different contexts. `[SIGSEGV-HEAP]` reports `pid=100` (parent) because that is what `read_current_pid()` returns; the EL1 exception dump shows `Thread=17, TTBR0=0xcc00005958a000`. These are inconsistent.

### Root cause: process_info_page PID mismatch

Thread=17 is running the child's goroutine (`SP_EL0=0x1e0066c30` is in the child's goroutine stack range; the TTBR0 is the child's page table root) but `read_current_pid()` returns 100 (the parent's PID). This happens because `process_info_page` — the per-thread page that holds the PID readable from EL0 — is **not updated** when a newly-exec'd child is assigned to a kernel thread that previously ran the parent's goroutines.

The fault chain:

1. **Thread=17 triggers a demand-paging fault** on child VA `0x1e0a47000` — a lazily-mapped heap page in the child's address space.
2. **The kernel's demand-paging handler** reads `read_current_pid()` → 100 (parent). It looks up the parent's lazy region table (keyed by PID 100) for VA `0x1e0a47000`.
3. **That VA does not exist in the parent's lazy regions** — it's a child VA. The lookup fails.
4. **The kernel copy path** (`strb w3, [x0], #1` at EL1 ELR=0x40435e28) faults when trying to write to the unmapped page, generating a **Sync from EL1 exception** (EC=0x25 = data abort from EL1).
5. **The EL1 fault handler** sees this as an unexpected kernel address fault, delivers `SIGSEGV` to `pid=100` (still using the stale `read_current_pid()`), with `fault_pc=0x13060` (the `read` SVC+4 — the parent goroutine's suspended resume point).
6. **The parent goroutine** receives SIGSEGV at `pc=0x13060` (return from `read` syscall) with `addr=0x1e0a47000` (the child's unmapped heap page) — a heap-range address that makes Go panic.

### Evidence details

- `TTBR0=0xcc00005958a000`: This is the child's (pid=104, exec'd at T177.42) page table. It is **not** the parent's page table. Thread=17 was assigned to the child after `execve`.
- `SP_EL0=0x1e0066c30`: This is in the `~0x1e006...` range — the child's goroutine stack (child stacks span `0x1e006...`–`0x1e008...`). The parent's main goroutine has `user_sp≈0x1e0087xxx` throughout the log.
- `[SIGSEGV-HEAP] pid=100 far=0x1e0a47000 elr=0x402b38a0`: The SIGSEGV is attributed to pid=100 because the PID lookup is stale. `elr=0x402b38a0` is inside the kernel (`0x402b…`) — this is a kernel VA fault, not a user-land fault, confirming the EL1 copy path faulted.
- `[signal] deliver sig=11 slot=8 handler=0x86ee0 fault_pc=0x13060`: The signal is delivered at slot=8 (the parent's main goroutine thread). `fault_pc=0x13060` is the `read` syscall return trampoline — the parent goroutine is waiting blocked in `unix.Read` on the epoll pipe and gets a spurious SIGSEGV from the child's demand-paging failure.
- Earlier in the log (T29.55, T177.42): the child (`forktest_child`) was exec'd twice (`pid=94` and `pid=104`). Thread=17 evidences fault at `TTBR0=0xc1000059568000` (pid=94's tables) at T29 (lines 3006, 3278) and at `TTBR0=0xcc00005958a000` (pid=104's tables) at T178 — same pattern, same thread, two different children, same stale-PID failure mode.

### Why this is hard to hit without the DC ZVA fix

Before the DC ZVA fix, the child was crashing within seconds (crash31.log shows the child dying at ~T1). Thread=17 ran very little child-side code before the child died, so the window for the demand-paging race was tiny. With the DC ZVA fix, `forktest_child` runs for the full 30s duration with heavy mmap stress (70 MiB, `-mmap_test`), greatly expanding the window during which Thread=17 is executing child goroutines under the parent's process_info_page PID.

### Fix applied (2026-05-10)

**`address_space_owner_pid_for_fault()` — TTBR0-derived lookup**
(`crates/akuma-exec/src/process/children.rs`)

Added `owner_pid_for_l0_phys(l0_phys)` which scans the process table for the non-shared process (thread-group leader) whose address space L0 frame matches the given physical address.  Updated `address_space_owner_pid_for_fault()` to call this first, using the current TTBR0_EL1 (which unambiguously identifies the running address space) before falling back to `THREAD_PID_MAP → tgid` and `read_current_pid()`.

**Why this fixes the crash**: TTBR0_EL1 is always correct for the currently-running address space. Any staleness in `THREAD_PID_MAP` (e.g. a kernel thread slot reused for a different process before the old entry was cleaned up, or a goroutine that was assigned tgid=100 by a misdirected `clone_thread` call) does not affect the TTBR0 path. The demand-pager now finds the correct owner PID even if the process_info_page or thread map disagrees.

---

## Pattern 3 root cause analysis (crash34.log, 2026-05-15)

`crash34.log` showed `forktest_parent` (pid=128) crashing with:

```
[DA-NONE] pid=128 as_owner=128 far=0x1e2be9000 region=0x1e0400000+0x7c00000 flags=0x60000000000080
[signal] deliver sig=11 slot=8 handler=0x86ee0 fault_pc=0x13060
```

### Root cause: PROT_NONE lazy region accessed without prior sysMap

Go's memory manager reserves heap arenas with `sysReserve` → `mmap(hint, n, PROT_NONE, MAP_ANON)`, then commits subranges with `sysMap` → `mmap(v, n, PROT_RW, MAP_FIXED)` before first use. Both calls produce lazy regions (> 256 pages, so `use_lazy = true`).

`forktest_parent`'s Go runtime called `sysReserve` for `0x1e0400000+0x7c00000` (128 MB, PROT_NONE lazy entry) but never called `sysMap` for the subrange containing `0x1e2be9000`. The parent's heap workload is lighter than `forktest_child`'s, so its heap never grew into the fourth 72 MB subrange that the children commit during their own initialization.

When the parent eventually wrote to `0x1e2be9000` (a heap data write — ISS WnR=1, translation fault DFSC=0x07), the demand-pager found the PROT_NONE lazy region entry and previously fell through to SIGSEGV via the `[DA-NONE]` path.

**Why guard pages are unaffected:** Stack guard pages are plain unmapped VAs (no PTE, no lazy-region entry). On access they fault with `lazy_region_lookup = None` and fall through to SIGSEGV regardless of this change.

### Fix (`src/exceptions.rs`, 2026-05-15)

Changed the `[DA-NONE]` path in the translation-fault demand-pager to auto-commit anonymous PROT_NONE lazy regions on first access, mapping with `RW_NO_EXEC` instead of SIGSEGVing. Only the OOM path still falls through to SIGSEGV.

```rust
if akuma_exec::mmu::user_flags::is_none(flags) {
    // Auto-commit anonymous PROT_NONE reservation on first access (Go sysReserve pattern).
    let page_va = far_usize & !(0xFFF);
    if let Some(page_frame) = crate::pmm::alloc_page_zeroed() {
        // ... map RW_NO_EXEC, track frames, return success
        return unsafe { (*frame).x0 };
    }
    // OOM: fall through to SIGSEGV
}
```

### Verification (crash35.log + crash36.log, 2026-05-15)

**crash35.log:** Three `forktest_parent -mmap_test -mmap_alloc_mb=70 -duration=30s -num_children=2` runs show no `[DA-NONE]` lines. All completed runs exited `code=0` (pid=112 at T116, pid=127 at T160, pid=143 confirmed clean).

**crash36.log:** Three `forktest_parent --combined_stress --mmap_alloc_mb=100 --duration=30s --num_children=2` runs all exited `code=0` (pid=90 at T47, pid=108 at T85, pid=125 at T121). All children `code=0`. No `[DA-NONE]`, `[SIGSEGV]`, or `[WILD-DA]` lines for forktest anywhere in the log. Combined stress with 100 MB mmap is fully passing.

**crash36.log also contains the first `crush` crash** (pid=142/149, T157–T158): Pattern 4 QEMU EC=0x15 misrouting on `stp xzr, xzr, [x0]` in Go 1.26 GreenTeaGC — see §Pattern 4 at top of this document.

**Forktest matrix status:**

| Test | Status |
|------|--------|
| `-mmap_test -mmap_alloc_mb=70 -duration=30s -num_children=2` | ✅ 3 clean runs (crash35.log) |
| `--combined_stress --mmap_alloc_mb=100 -duration=30s --num_children=2` | ✅ 3 clean runs (crash36.log) |
| `-file_io` | ❓ not yet tested; ext2 deadlock risk |
| `crush` binary (`stp_test_go`, `stp_test_c`) | ✅ Pattern 4 fixed (2026-05-15) |

---

## Prior status (2026-05-07, updated 2026-05-08 — interim conclusions below)

**Quadrant experiments (`crash21.log`, **`crash23.log`**, 2026-05-08):** Serial captures across **Go/C parent × Go/C child** combinations — see [§ Quadrant matrix + crash21](#quadrant-matrix--crash21log-2026-05-08) and [§ crash23.log quadrant timeline](#crash23log-quadrant-timeline-2026-05-08). **`pattern2_parent -child=forktest`** drives **`forktest_child`** from a **C** epoll parent; **one** Go child tends to complete cleanly; **two** Go children reliably stress **`forktest_child`** (**`memclr` / errno-shaped FAR**) while **`pattern2_parent` stays `exit_group code=0`**. **`crash23.log`** adds a **dedicated Q4** window (Go parent + Go children): **both** child **`WILD-DA` / `nr=113`** and parent **`sig=11` @ `0x13060`** / **`SIGSEGV-HEAP`** in one session.

**Forktest mmap stress still reproduces intermittent allocator crashes** (`pc≈0x86768`, `runMmapStress` / `memclr`), with fault addresses varying over time (`0x2`, **`0x10`** / **`0x12`** (low canonical VAs), negative “errno-like” FARs, etc.). A **lazy-region / `tgid` owner fix** (2026-05-07) addresses a real **`EFAULT` / wrong-owner** class of bugs but **does not close** the **SIGURG + syscall / JIT replay** failure mode seen in serial evidence (see **`crash5.log`** below).

**2026-05-08 (session `crash19.log`):** One serial capture ties together **SSH + kernel**: with **`GODEBUG=asyncpreemptoff=1`**, the Go parent dies in **`EpollCtl`** (**syscall nr `0x15` = `epoll_ctl`**); with **`GODEBUG` unset**, stacks show **`unix.Read`** (**`0x3f` = `read`**). **`pattern2_parent`** ([`userspace/forktest/c_stress/pattern2_parent.c`](../userspace/forktest/c_stress/pattern2_parent.c)) — minimal **C** parent with the same epoll/pipe/**`mmap_stress`** shape — **`exit_group code=0`** for **1** and **3** children @ **70 MiB**, while **`forktest_parent`** in the same log exits **`code=2`** four times with **`deliver sig=11`**, **`fault_pc=0x13060`**. That **isolates the crash to the Go parent path** (runtime / signal / trampoline interaction), not “epoll or pipe broken for every userspace caller.” Full tables: [§ crash19.log](#crash19log--pattern2_parent-session-2026-05-08).

**2026-05-08:** Pure-C **`mmap_stress`** ([`userspace/forktest/c_stress/`](../userspace/forktest/c_stress/)) can run cleanly **standalone**, but **`forktest_parent --use_c_child`** still kills the **Go parent** in **Pattern 2** (`read` / epoll pipe). Serial **`crash14.log`** shows **`mmap`** (**`nr=222`**) from a C child with **`x0 = −22`** (errno-shaped **before** kernel handling)—see [§E](#e-pure-c-mmap_stress--crash14-serial-2026-05-08) below. That points to **syscall-entry / trap-frame corruption**, not Go’s allocator alone.

**Kernel instrumentation (2026-05-08):** When **`[EINVAL]` / `[EFAULT]` / `[ENOSYS]`** fire, serial can include **`tid=`**, **`ELR=`**, full **`x0`–`x5`**, and for **`nr=222`** a follow-on **`[mmap-einval]`** line with **`reason=`** and decoded **`flags=`**—see [Serial errno diagnostics](#serial-errno-diagnostics-2026-05-08). Toggle via **`SYSCALL_ERRNO_DIAG_EXTRA`** in [`src/config.rs`](../src/config.rs). Regression tests: **`mmap_fixed_addr_unaligned_einval_helper`**, **`mmap_einval_through_handle_syscall`** in [`src/tests.rs`](../src/tests.rs).

**Pipe `read` tracing (2026-05-08):** With **`SYSCALL_DEBUG_PIPE_READ`** (and optional **`SYSCALL_DEBUG_PIPE_READ_SAMPLE`**) in [`src/config.rs`](../src/config.rs), serial emits **`[pipe-read] enter`** / **`EFAULT`** lines from [`src/syscall/fs.rs`](../src/syscall/fs.rs) for **`PipeRead`** FDs—correlate with **`fault_pc≈0x13060`** via **`tprint`** timestamps. [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh) greps **`[pipe-read]`** alongside mmap / fault markers.

Synthesis of evidence so far: [§ Interim conclusions](#interim-conclusions-2026-05-08). **Continuing work:** [§ Agent handoff](#agent-handoff-2026-05-08) (includes **`crash23.log`** findings).

This failure mode is **orthogonal** to ext2 fixes that removed spurious **`input/output error`** on `/tmp` under load (blocking `read_state()` and a single `write_state()` for `write_at` in [`crates/akuma-ext2/src/ext2.rs`](../crates/akuma-ext2/src/ext2.rs)). If you see **EIO** on temp files, that was filesystem contention; if you see **`addr=0x2`** / **`0x10`** in the Go allocator, treat it as the **heap + demand paging + signal / syscall-frame** investigation described below.

| What you see | Likely bucket | Where to read |
|--------------|----------------|---------------|
| `write /tmp/...: input/output error` | ext2 read path starved / `IoError` | ext2 history in `GO_FORK_EXEC_FIXES.md` |
| `addr=0x2`, `0x10`, `0x12`, panic in `mallocgc` / `memclr` | Bad pointer / span base in Go after kernel user-mode fault path | This file, §Crash Pattern 1–2, **`crash5.log`** |
| **`unexpected fault address 0xfffffffffffffffa`** (`-6`), **`fatal error: fault`** | Prior **`clock_gettime` → EINVAL (-22)**; FAR **`-6`** = **`-22+16`** (errno misused as base + `sizeof(timespec)`) | **`crash4.log`** / § Serial captures |
| **`unexpected fault address 0xffffffffffffffb0`**, `fatal error: fault` in `memclr` | Same **`pc≈0x86768`** family; often at **50–70 MiB** `-mmap_alloc_mb` | [Empirical threshold](#empirical-allocation-threshold-2026-04-14-session) |
| `[JIT] IC flush + replay … bogus nr=…` near fault | Stale / corrupted syscall dispatch state around SVC replay | [`src/exceptions.rs`](../src/exceptions.rs), `FIX_MEMORY_MAPPING.md`, `EPOLL_PERFORMANCE.md` |
| **`[EINVAL] nr=222`** (`mmap`), **`args[0]`** = **`0xffffffffffffffea`** (−22) | Same **errno-as-GPR** family as xattr/`clock_gettime`; extended lines show **`reason=`** (`len==0`, **`fixed+unaligned`**, **`kernel_va`**, **`other`**)—see [Serial errno diagnostics](#serial-errno-diagnostics-2026-05-08) | **`crash14.log`**, §E; [`src/syscall/mod.rs`](../src/syscall/mod.rs); **`mmap_fixed_addr_unaligned_einval`** in [`src/syscall/mem.rs`](../src/syscall/mem.rs) |
| **`pattern2_parent`** exits **0**, **`forktest_parent`** exits **2** (same **`mmap_stress`** stress) | C parent control completes; Go parent still Pattern 2 — focus **Go runtime + signals**, not pipe/epoll alone | [crash19 session](#crash19log--pattern2_parent-session-2026-05-08), [`pattern2_parent.c`](../userspace/forktest/c_stress/pattern2_parent.c) |
| SSH **disconnect** after commands finish | Often **client / router idle timeout**; confirm with serial still running | Not proven guest deadlock from **`crash5.log`** alone |

**Mitigations while debugging:** ample RAM (`MEMORY=2048M` or higher), `GODEBUG=asyncpreemptoff=1`, or avoid **`-mmap_test`**, **`-combined_stress`**, and **`-file_io`** until fixed. **`GOMAXPROCS=1` does not prevent** the **parent** `read()` SIGSEGV when **`-mmap_test`** is enabled ([Isolation matrix](#isolation-matrix-2026-04-14)). **`asyncpreemptoff=1` alone does not reliably prevent Pattern 2**—see [§ Interim conclusions](#interim-conclusions-2026-05-08).

## Interim conclusions (2026-05-08)

These are **working hypotheses** from serial + SSH captures (**`crash16.log`**, **`crash14`**, same-session repros), not a closed root cause.

### 1. `GODEBUG=asyncpreemptoff=1` is necessary context, not a fix

In one session, **`forktest_parent --use_c_child …`** completed cleanly (**deadline SIGTERM**, children graceful, parent exited **0**), then a **second run** in the **same shell** (**`GODEBUG` still set**) **SIGSEGV**’d again (**`PC≈0x13060`**, **`read`** on a pipe FD). So **async goroutine preemption via `SIGURG` is not the only prerequisite** for Pattern 2; disabling it **reduces** urgency-signal traffic but **does not prove** the bug away. This aligns with the older finding that **`GOMAXPROCS=1` does not save the parent**.

### 2. Parent SIGSEGV: fault address vs `read` buffer

User registers at crash often show **`read(fd, buf, 0x400)`** with **`buf ≈ 0x1e0087708`**, while Go reports **`addr`** / **`fault`** around **`0x129a1000`**, **`0x1343f000`**, **`0x13d96000`**—**not** inside **`[buf, buf+0x400)`**. That **does not match** a naïve story “kernel **`copy_to_user`** faulted on the first byte of the pipe-read buffer.” It fits better **wrong effective address for some access while `PC` is still in the syscall/read trampoline** (e.g. **corrupted GPRs**) or **fault metadata** that does not map one-to-one to the **`copy_to_user`** range—still consistent with **syscall-entry / return / signal-frame** bugs rather than only “pipe implementation wrong.”

### 3. `SIGURG` at the same PC as `SIGSEGV` (serial **`crash16`**)

**`crash16.log`** shows **`SIGURG` (`sig=23`)** delivered to the parent thread with **`fault_pc=0x13060`** **before** the fatal **`SIGSEGV` (`sig=11`)** at the **same `fault_pc`**. That strengthened suspicion of **signal + syscall interaction**, but **§1** shows **`asyncpreemptoff`** does **not** eliminate the crash—so treat **`SIGURG`** as **correlated**, not **sufficiently causal** by itself.

### 4. Children: `mmap` **`[EINVAL]`** lines and **`[mmap-einval]`**

Extended logging shows **`nr=222`** with **`x1=0`** (**`len==0`**), **`x0`** sometimes a **prior mmap base** (e.g. **`0x10420000`**) or errno-shaped (**`0xffffffffffffffea`**), and **`prot` / `flags`** fields that are **not plausible Linux `mmap` arguments**. **`reason=len==0`** is accurate for the **`EINVAL`** return path; the **`flags=(FIXED|…)`** **decode can be meaningless** when **`x2`/`x3` are garbage**—do not read **`FIXED`** from the string alone. This supports **GPR corruption or reordering at SVC** under mmap load, independent of Go in the child.

### 5. Kernel implementation notes (landed in tree)

- **`sys_mmap`:** **`MAP_FIXED` / `MAP_FIXED_NOREPLACE`** **unaligned `addr` → `EINVAL`** is evaluated **before** **`lookup_process`** so **`handle_syscall`** from kernel-only tests and early rejects match **`EINVAL`**, not **`ESRCH`** (memory-test **`mmap_einval_through_handle_syscall`**).
- **Diagnostics:** **`SYSCALL_ERRNO_DIAG_EXTRA`**, **`SYSCALL_DEBUG_PIPE_READ`**, [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh).

### 6. Next narrowing steps

| Step | Purpose |
|------|---------|
| Serial **`grep`** **`[pipe-read]`** + **`fault_pc=0x13060`** + **`sig=11`** | See last **`buf=`** before crash; check for **`EFAULT copy_to_user`** vs fault without EFAULT. |
| **`--num_children=1`** | Reduce cross-process contention; if parent stabilizes, suspect **scheduler / concurrent mmap / FD** pressure. |
| Fresh process **vs** second run in same shell | Check for **accumulated kernel or libc state** (informal; serial **`pid=`** / **`pipe=`** ids still help). |
| **`[pipe-read]`** + **`[EINVAL] nr=222`** ordering | If syscall-arg garbage **clusters before** parent **`sig=11`**, prioritize **global trap / per-thread syscall state** audits over pipe-only fixes. |

## crash19.log + pattern2_parent session (2026-05-08)

Single boot, serial **`crash19.log`** (~7853 lines) + SSH transcript: **`forktest_parent --use_c_child`** (**`-mmap_test`**, **70 MiB**) and **`pattern2_parent`** (**same child stress**). Grep volume for the bundled regex ([`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh)): **~1588** matching lines.

### SSH: syscall number (`x8`) vs `GODEBUG`

Go prints the syscall number as the **first** argument to **`Syscall` / `Syscall6`**.

| `GODEBUG` | First arg (hex) | nr | Userspace stack |
|-----------|-------------------|-----|-----------------|
| **`asyncpreemptoff=1`** | **`0x15`** | **21** (`epoll_ctl`) | **`EpollCtl`** `main.go:243` |
| **unset** | **`0x3f`** | **63** (`read`) | **`unix.Read`** `main.go:209` |

So **`PC≈0x13060`** stays the **shared trampoline**; **`asyncpreemptoff`** changes **which syscall is live** when the fault surfaces—not proof of two unrelated bugs.

### Kernel / serial outcomes

| Binary | **`exit_group`** | Code |
|--------|------------------|------|
| **`forktest_parent`** | **4×** (pid **90, 97, 102, 109**) | **`2`** |
| **`pattern2_parent`** | **2×** (pid **116, 118**) | **`0`** |

**`deliver sig=11`** with **`fault_pc=0x13060`**: **4** lines (**`T22`**, **`T49`**, **`T84`**, **`T111`**) — all **`forktest_parent`** pids. **None** in the **`pattern2_parent`** windows (**~`T396–407`**, **`~T433–444`**).

### `[pipe-read]` churn

Approximate **`[pipe-read] enter`** counts: **`forktest_parent`** pid **109** **~1263** vs **`pattern2_parent`** pid **116** **~36** / pid **118** **~23**. Buffer pointers: Go **`buf≈0x1e0087708`**; musl **`buf≈0x201ffffa00`**. No **`[pipe-read] EFAULT`** on runtime pids immediately before **`sig=11`** in this capture.

### Children **`mmap` `[EINVAL]`**

**`[EINVAL] nr=222`** with **`x1=0`**, **`reason=len==0`**, garbage **`prot`/`flags`** decode appears for **`mmap_stress`** during **both** Go-parent and C-parent runs — **does not** predict parent **`sig=11`**.

### What did *not* show up in **`crash19.log`**

- **`[DA-MISS]` / `[WILD-DA]` / `[WILD-IA]`**: **no** lines with those tags (this failure mode is **not** exposing as logged demand-page wild faults here).
- **`[JIT]` bogus nr`**: **none**.
- **`[sigsegv-syscall]`**: **none** (instrumentation may be absent in the kernel binary used for this capture).

### Proposed next steps (after this session)

1. **Confirm `x8` on serial:** Run a kernel build that prints **`[sigsegv-syscall]`** ([`src/config.rs`](../src/config.rs) **`DEBUG_SIGSEGV_SYSCALL_STUB`**) and verify it matches SSH (**21** vs **63**) on crash.
2. **Prioritize Go + trap path:** With **`pattern2_parent`** stable, focus on **`SIGURG`** delivery / **`rt_sigreturn`** / nested signals while **`ELR≈0x13060`**, and on **why** Go’s heap-range **`fault`** addresses appear during syscalls (**[`src/exceptions.rs`](../src/exceptions.rs)**, Go signal handler **`0x86ee0`**).
3. **Shrink the repro in Go:** Minimal static binary: **epoll + pipe `read` + `epoll_ctl` MOD** only (no **`forktest_parent`** extras) to bisect runtime surface area.
4. **Keep child `mmap` `[EINVAL]` on the parallel track:** Still audit **SVC / GPR** under load ([§E](#e-pure-c-mmap_stress--crash14-serial-2026-05-08)), but treat it as **orthogonal noise** until it correlates with parent death in time/pid order.
5. **Scripts:** [`scripts/forktest_pattern2_bisect.sh`](../scripts/forktest_pattern2_bisect.sh) for bisection commands; rebuild **`pattern2_parent`** via **`userspace/build.sh --with-forktest`**.

## Quadrant matrix + crash21.log (2026-05-08)

Single boot, serial **`crash21.log`** (~32k lines): mixed **`forktest_parent`**, **`pattern2_parent`** (C parent + **`mmap_stress`** or **`forktest_child`** via **`-child=forktest`**), and **`exit_group`** outcomes. Mine with [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh) / [`scripts/correlate_forktest_mmap_sig11.sh`](../scripts/correlate_forktest_mmap_sig11.sh).

### Parent × child stress (four quadrants)

| Quadrant | Parent | Children | Serial assessment (`crash21.log`) |
|----------|--------|----------|---------------------------|
| **Q1** | Go **`forktest_parent`** | C **`mmap_stress`** (`--use_c_child`) | **Mixed:** **`forktest_parent`** **`exit_group code=2`** (e.g. pid **90**, **111**) and **`code=0`** (e.g. **97**, **104**). **`mmap_stress`** exits logged **`code=0`**. Matches **Pattern 2** in the **Go parent** — intermittent. |
| **Q2** | C **`pattern2_parent`** | C **`mmap_stress`** (default) | **Stable:** **`pattern2_parent`** **`code=0`**; **`mmap_stress`** **`code=0`** (several 3-child runs @ **70 MiB**). Baseline: epoll/pipe + mmap storm **without Go**. |
| **Q3** | C **`pattern2_parent`** | Go **`forktest_child`** (**`-child=forktest`**) | **Parent always `code=0`.** **`forktest_child`** sometimes **`code=2`** under multi-child + **70 MiB** (failed **`runMmapStress`** / **`memclr`**); **`code=0`** on other runs. Failure is **child-side**, not C epoll. **One** Go child often completes; **two** children reliably reproduce **`forktest_child`** faults in interactive testing — aligns with **`exit_group`** **`forktest_child`** **`code=2`** while parent **`pattern2_parent`** still **`code=0`**. |
| **Q4** | Go **`forktest_parent`** | Go **`forktest_child`** (default) | **`crash23.log`** (~**T224–T226**): explicit **`forktest_parent`** + **`forktest_child`** window — parent **`sig=11`**, **`fault_pc=0x13060`**, **`[SIGSEGV-HEAP]`** on parent **`far≈0x1e4372000`**; children **`[EINVAL] nr=113`** @ **`ELR≈0x86768`** → **`[WILD-DA] FAR=0xfffffffa`**. **`crash21.log`** alone did not isolate Q4 cleanly; use **`crash23`** as the serial baseline for **Go+Go**. |

### Kernel sequence (Q3 / errno-shaped FAR)

On a failing **`forktest_child`** (e.g. **pid 172**), serial shows **`[EINVAL] nr=113`** (**`clock_gettime`**) with **implausible `args=`** and **`ELR≈0x86768`** (**`memclr`**), then **`[DA-MISS]`** / **`[WILD-DA]`** with **`FAR=0xfffffffffffffffa`** (**signed −6**), **`x0=0xffffffffffffffea`** (**−22 `EINVAL`** sign-extended) — same **errno-as-pointer / timespec** family as [crash4 / table](#what-you-see) (**`unexpected fault address 0xfffffffffffffffa`**). **`pattern2_parent`** continues (**`[pipe-read]`** on parent **pid 171**) — the crash is **not** the C parent’s syscall trampoline.

### Practical commands (Q3)

```bash
# C parent, one Go child (often clean @ 70 MiB / 10 s)
/bin/pattern2_parent -child=forktest -num_children=1 -duration=10s -mmap_alloc_mb=70

# C parent, two Go children (stress — child faults common)
/bin/pattern2_parent -child=forktest -num_children=2 -duration=10s -mmap_alloc_mb=70
```

Rebuild **`pattern2_parent`** / disk after changing [`pattern2_parent.c`](../userspace/forktest/c_stress/pattern2_parent.c): **`userspace/build.sh --with-forktest`**.

### Next steps (from this matrix)

1. **Capture Q4** explicitly: **`forktest_parent`** without **`--use_c_child`**, same **`-mmap_test`** / **`-mmap_alloc_mb`**, **`tee`** serial — compare **`[sigsegv-syscall]`** / parent **`sig=11`** vs child **`WILD-DA`** ordering.
2. **Q3 bisection:** **`pattern2_parent -child=forktest -num_children=2`** with **`-mmap_alloc_mb=10`** vs **70** to locate a pressure threshold.
3. **Q3 control:** **`pattern2_parent`** (default **`mmap_stress`**) **`-num_children=2`** @ **70 MiB** — if stable, isolates **Go runtime + syscalls** vs raw mmap traffic.
4. **Kernel audit:** **`nr=113`** path + trap-frame **`args=`** when **`ELR`** points at **allocator** PC — [`src/syscall/mod.rs`](../src/syscall/mod.rs), [`src/exceptions.rs`](../src/exceptions.rs).

## crash23.log quadrant timeline (2026-05-08)

Serial **`crash23.log`** (~7185 lines): one QEMU boot with SSH traffic + mixed quadrant runs. Mine with [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh) (**`[WILD-DA]`**, **`[EINVAL] nr=113`**, **`deliver sig=11`**, **`exit_group`**).

### Boot noise (ignore for forktest RCA)

- **`[Test] exit_group_kills_siblings_before_close_all FAILED`** during kernel self-tests — **orthogonal** to mmap/forktest; track as a separate CI/kernel bug.

### Q1 — Go parent + C **`mmap_stress`** (~**T39**)

- **`forktest_parent` pid 90** (`/bin/forktest_parent`): **`tkill(tid=8, sig=23)`** then **`deliver sig=11`**, **`fault_pc=0x13060`**, **`slot=8`**; nested **`sig=23`** at **`fault_pc=0x80800`** after **`sigreturn`**.
- **`[exit_group] pid=90 … forktest_parent code=2`** after **~0.63 s**. No adjacent **`[pipe-read] EFAULT`** — same shape as **`crash19`**: parent dies **without** a logged pipe **`copy_to_user`** failure.
- Children **94–96** continue **`mmap_stress`**; **`[EINVAL] nr=222`** (**`len==0`**, garbage decode) appears **after** the parent is already gone — **parallel child noise**, not causal ordering.

### Q2 — C **`pattern2_parent`** + C **`mmap_stress`** (~**T85–T95**)

- **`pattern2_parent` pid 97** **`exit_group code=0`**; **`mmap_stress`** pids **98–100** **`code=0`**. **`buf≈0x201ffffa00`** on **`[pipe-read]`** (musl stack) vs Go **`~0x1e0087708`**.

### Q3 — C **`pattern2_parent`** + Go **`forktest_child`**

- **One child (~T127–T138):** **`forktest_child` pid 108 `code=0`**, **`pattern2_parent` pid 101 `code=0`** — clean.
- **Two children (~T155–T156):** **`forktest_child` pid 110**: **`[EINVAL] nr=113`** (**`clock_gettime`**) with **`ELR≈0x86768`** and implausible **`args=`** → **`[DA-MISS] va=0xfffffffffffffffa`** → **`[WILD-DA]`** (**`FAR=-6`**, **`x0=-22`**). **`forktest_child` exits `code=2`**; **`pattern2_parent` pid 109 `code=0`**. Strong kernel-visible proof of the **child-only** errno / **`memclr`** lane while the **C parent stays healthy**.

### Q4 — Go **`forktest_parent`** + Go **`forktest_child`** (~**T224–T226**)

- **`SIGURG`** bursts (**`tkill`**) on parent thread **tid 8** with **`fault_pc=0x13060`**; **`sigreturn`** restores **`pc=0x13060`** (syscall trampoline under preempt signals).
- **`forktest_child`** pids **127 / 128**: repeat **`[EINVAL] nr=113` @ `ELR≈0x86768`** → **`[WILD-DA]`** (**`FAR=0xfffffffa`**).
- **`forktest_parent` pid 123**: **`[SIGSEGV-HEAP] far=0x1e4372000 elr=0x402b30a0`** then **`deliver sig=11`**, **`fault_pc=0x13060`** — **two PCs** in sequence (**runtime / heap-tagged fault** vs **stub-line `sig=11`**); treat both as parent-side evidence, not a single ELR.
- **`forktest_parent` `exit_group code=2`**; some **`forktest_child`** instances **`code=2`**, others **`code=0`** depending on timing.

### Instrumentation gap in this capture

- **No `[sigsegv-syscall]`** lines — either **`DEBUG_SIGSEGV_SYSCALL_STUB`** was off or ELR at the diagnostic site did not match the stub window. **Use serial **`deliver sig=11` … `fault_pc=0x13060`**** as the anchor for the parent crash in this log.

### Relation to **`crash21.log`**

- **`crash21`** established the quadrant matrix and Q3 **`nr=113` → `WILD-DA`** sequence; **`crash23`** **narrows Q4** (Go+Go) and records **Q1/Q2/Q3** in one shorter file with **timestamps** (**`T39`**, **`T95`**, **`T138`**, **`T226`**) for ordering **`SIGURG`** vs child faults.

## Isolation matrix (2026-04-14)

Shell: `export GOMAXPROCS=1` for all runs below. Command line: `forktest_parent --duration 10s` plus flags.

| Child mode | Outcome |
|------------|---------|
| **(none)** — children run default main (no stress) | **Stable.** Parent sends SIGTERM at deadline; `Wait()` reports `signal: killed`; empty child stdout. |
| **`-mmap_test`** | **Parent SIGSEGV** in `unix.Read` on pipe (**`main.go:199`**): `PC≈0x13060`, fault `addr≈0x1e39df000`, `syscall` read `fd=4`. Same shape as [Pattern 2](#crash-pattern-2-parent-process-heap-corruption). **Does not require** `-combined_stress` or multiple Go M-threads in the parent. |
| **`--use_c_child`** + **`-mmap_test`** | Same **parent** failure as above (Go parent + epoll **`read`**); children run **`/bin/mmap_stress`** instead of Go. **C children can still `exit_group code=0`** after the parent dies — not proof the parent path is sound. **`crash14.log`**: **`nr=222`** (`mmap`) with errno-shaped **`x0`** from a child. See §E. |
| **`-file_io`** | **Not a safe mode.** One short run showed children printing `Received terminated, exiting gracefully.` before kill, but **`-file_io` has also reproduced a full deadlock** (no forward progress in SSH / shell). That lines up with **concurrent temp-file writes** on ext2 and earlier **`ps`** / shell hangs under I/O stress—count **`file_io`** as a **deadlock risk**, not “stable”. |
| **`-send_signal`** | **Stable** (benign race): `Failed to send SIGINT … process already finished` if the child exits before 500 ms; then deadline kill as usual. |

**Conclusion:** The **mmap heap stress in children** (`runMmapStress`, large `make([]byte, …)`, or pure-C **`mmap_stress`**) is enough to trigger the **parent `read()` SIGSEGV**; **`GOMAXPROCS=1` does not rule out “multi-M in parent”** as the sole cause—it rules out **parent** multi-threading as required for that crash. Replacing Go children with **C `mmap_stress`** does **not** eliminate the parent crash (§E). Separately, **`file_io`** stress can **deadlock** the system even when mmap does not SIGSEGV the parent—likely **filesystem / lock / scheduler** interaction, not only Go’s allocator.

## Test Command

```bash
MEMORY=2048M cargo run --release
# Then via SSH:
cd /bin && forktest_parent --duration 10s --combined_stress
```

## Crash Pattern 1: Child Process Heap Corruption

### Symptoms

```
panic: runtime error: invalid memory address or nil pointer dereference
[signal SIGSEGV: segmentation violation code=0x1 addr=0x2 pc=0x86768]

goroutine 10 [running]:
main.runMmapStress(...)
runtime.memclrNoHeapPointers()
  .../memclr_arm64.s:91 +0xb8
runtime.mallocgcLarge(...)
  .../malloc.go:1612 +0x1a8
```

### Kernel Log Evidence

```
[DA-MISS] pid=96 ppid=90 va=0x2 lr_count=14 parent_lr=13 parent_has_va=false
[WILD-DA] pid=96 FAR=0x2 ELR=0x86768 last_sc=18446744073709551615
```

### Analysis

- **Fault address**: `0x2` is NOT a valid memory address - it's a corrupted pointer value
- **PC=0x86768**: Crash occurs in `memclrNoHeapPointers` (Go's memory zeroing routine)
- **Call chain**: `make([]byte, N)` → `mallocgc` → `mallocgcLarge` → `memclrNoHeapPointers`
- **`last_sc=!0u64`**: No syscall was active - crash is purely in userspace
- **Implication**: Go's `mallocgc` returned `0x2` instead of a valid heap pointer

## Crash Pattern 2: Parent Process Heap Corruption

### Symptoms

```
SIGSEGV: segmentation violation
PC=0x13060 m=0 sigcode=1 addr=0x2

goroutine 1 [syscall]:
syscall.Syscall(0x3f, 0x4, 0x1e0087718, 0x400)  // read() syscall
```

### Kernel Log Evidence

```
[DA-MISS] pid=90 ppid=0 va=0x2 lr_count=13 parent_lr=0 parent_has_va=false
[WILD-DA] pid=90 FAR=0x2 ELR=0x13060 last_sc=18446744073709551615
```

### Analysis

- **Fault address**: Older kernel captures reported **`FAR=0x2`** for the parent as well as the child. A **2026-04-14 SSH capture** (see below) shows the parent fault at **`addr=0x1e251f000`** (heap-range VA) while the child still shows **`addr=0x2`**. So the parent failure is **not always** the same bit pattern as the allocator bug in the child; it may be a **follow-on SIGSEGV** during `read()` (pipe drain), **kernel copy_to_user**, or a **distinct** runtime bug.
- **PC≈0x13060**: In Go's syscall path (e.g. return trampoline around `read`)
- **Context**: Parent was in **`unix.Read`** on the epoll-monitored pipe (**`fd=4`** in registers: `r0=4`, `r1=buf`, `r2=0x400`); corresponds to [`userspace/forktest/parent/main.go`](../../userspace/forktest/parent/main.go) pipe-read logic (line numbers shift with Go version; stack may show `main.go:176` in older builds vs current sources).
- **Timing**: Often **after** a child process has already panicked with **`addr=0x2`** in `runMmapStress`, but not always independently observed.

## Captured SSH log (2026-04-14)

Full command: `forktest_parent --duration 10s --combined_stress` from `/bin` over SSH.

**1. Child (`forktest_child`) — Pattern 1**

```
panic: runtime error: invalid memory address or nil pointer dereference
[signal SIGSEGV: segmentation violation code=0x1 addr=0x2 pc=0x86768]

goroutine 10 [running]:
main.runMmapStress(...{childID}...)
    .../forktest/child/main.go:88 +0x228
main.runCombinedStress.func1()
    .../forktest/child/main.go:225 +0x50
```

Line 88 is the large `make([]byte, …)` allocation in `runMmapStress` (see [`userspace/forktest/child/main.go`](../../userspace/forktest/child/main.go)).

**2. Parent (`forktest_parent`) — Pattern 2 (same session, second fault)**

```
SIGSEGV: segmentation violation
PC=0x13060 m=0 sigcode=1 addr=0x1e251f000

goroutine 1 gp=0x1e00021c0 m=0 mp=0x1edc40 [syscall]:
syscall.Syscall(0x3f, 0x4, 0x1e0087718, 0x400)   // read(fd=4, buf, 1024)
golang.org/x/sys/unix.Read(...)
main.main()
    .../forktest/parent/main.go:176 +0xd40
```

`0x3f` is **63** decimal = **`read`** on Linux arm64. The buffer pointer `0x1e0087718` is a normal-looking stack/local slot; the reported fault **`addr=0x1e251f000`** is in the **~0x1e0…** Go heap range (unlike **`0x2`** in the child). Register dump included `r0=0x4` (fd), consistent with draining the child's stdout pipe in the epoll loop.

**3. `ps` after the crash**

The first `ps` may list **many rows** with the same `/bin/forktest_child … -combined_stress` cmdline and odd **PPID chains** (e.g. threads under one child). That matches **goroutine / runtime threads** (`CLONE_VM`) each appearing as a schedulable entity in Akuma’s process listing. A **second** `ps` in the same session showed **no processes** — everything had exited after the faults.

**4. Build paths in the traceback**

Paths such as `/opt/homebrew/Cellar/go/1.25.5/...` are from the **host** used to build the `GOOS=linux GOARCH=arm64` binary; the panic still occurred on the **Akuma** target.

**5. Delayed full kernel freeze (anecdotal, same session)**

Sometime **after** the user-level panic/`SIGSEGV` sequence above, the **whole guest** appeared to **lock up** (e.g. SSH stopped responding). That was **not** in the same snippet as the forktest traceback, so it is **not** proven causal—only **temporally** related.

If this repeats, capture **serial / QEMU log** from the freeze window and look for: a thread stuck **forever** in a spinlock (ext2, process table, lazy-region, or fault path), **interrupts masked** too long, or **memory corruption** from an earlier fault that only manifests when another subsystem runs. Until there is a trace, treat “freeze after forktest” as an open **follow-on** symptom tied to the same stress scenario, not a separately root-caused bug.

## The `0x2` Value

The value `0x2` is suspicious because:

1. It's too small to be a valid heap pointer (Go heap starts at ~0x1e000_0000)
2. It's not NULL (0x0) which would indicate a clear nil pointer
3. It appears in **child** processes in these traces; the **parent** sometimes faults at a **heap-like** address (e.g. `0x1e251f000`) instead — see [Captured SSH log](#captured-ssh-log-2026-04-14)
4. It suggests corruption of Go's span/mheap data structures

Possible sources of `0x2`:
- A corrupted `mspan.base` pointer
- A freed/recycled span that still contains stale metadata
- A race condition leaving partial pointer values

## Verified Non-Issues

### clock_gettime Syscall

**Static tests:** `clock_gettime` / `clock_getres` behave plausibly Linux-compat in isolation (see tests below).

**Runtime (2026-05-07):** Serial logs show **`nr=113`** with **pointer-sized “clock_id”** in **`x0`** and small garbage in **`x1`**, often **`tkill(…, SIGURG)`** (async preempt) nearby. That is **not** explained by the syscall implementation alone; it implicates **trap-frame / syscall-restart / signal** interaction. Returning **strict `EINVAL`** for oversized `clock_id` avoids EL1 `copy_to_user` on a bogus “second arg” pointer, but leaves **`-22` in `x0`**, which Go can still misuse (see **`crash4.log`**: FAR **`0xfffffffffffffffa`** = **`-22+16`**).

**Rejected mitigation:** Treating oversized **`x0`** as the timespec pointer and writing 16 bytes (**“recover x0 as tp”**) was **abandoned**: **`crash5.log`** shows **`SIGURG`** → that path → **`[WILD-DA] FAR=0x`10` ELR=0x86768`** with **`x0=0x0`** at fault time (nil-adjacent **`memclr`**), i.e. no durable fix and possible heap clobber.

The older note that some **`[EFAULT] nr=113`** lines come from **kernel self-tests at boot** remains true; **forktest** sessions additionally show **runtime** **`nr=113`** adjacent to allocator faults.

Tests added:
- `test_clock_gettime_struct_layout` - Verifies `struct timespec` matches Linux ABI
- `test_clock_gettime_realtime` - CLOCK_REALTIME returns valid time
- `test_clock_gettime_monotonic` - CLOCK_MONOTONIC never goes backwards
- `test_clock_gettime_all_clock_ids` - All clock IDs accepted
- `test_clock_gettime_efault_null_ptr` - NULL pointer returns EFAULT
- `test_clock_gettime_efault_invalid_ptr` - Invalid pointer returns EFAULT
- `test_clock_getres_basic` - Resolution query works
- `test_clock_getres_null_ptr` - NULL res pointer allowed (Linux compat)

### Sigaltstack Handling

Sigaltstack handling was verified:
- `clone_thread` creates new M-threads with `alt_sp=0x0` (correct)
- Forked processes inherit sigaltstack from parent (correct for fork semantics)
- SIGURG guard in `entry_point_trampoline` clears pending signals for uninitialized threads

## Theories to Investigate

### Theory 1: CoW Page Fault Race Condition

**Hypothesis**: When multiple Go M-threads fault on the same CoW page simultaneously, the page fault handler may corrupt allocator metadata.

**Evidence**:
- Crashes occur in multi-threaded Go processes
- `CLONE_VM` threads share address space
- Go's heap spans cross page boundaries

**Investigation steps**:
1. Add logging to `handle_cow_fault()` when Go heap pages are duplicated
2. Check for lock contention in CoW fault handling
3. Verify TLB invalidation is correct for all CPUs/threads

### Theory 2: Demand Paging Race in Lazy Regions

**Hypothesis**: The `LAZY_REGION_TABLE` operations have a race condition when multiple threads fault on the same lazy region.

**Evidence**:
- Go allocates large lazy regions (e.g., `mmap 0x6400000` = 100MB)
- Multiple M-threads can fault on different pages within the same region
- The region lookup and physical page allocation may not be fully atomic

**Investigation steps**:
1. Add per-region locks for demand paging
2. Log when two threads fault on the same region simultaneously
3. Verify physical page is correctly mapped for all faulting threads

### Theory 3: Process/Thread Address Space Confusion

**Hypothesis**: With `CLONE_VM` threads, the address-space owner PID tracking has edge cases that cause wrong page tables to be used.

**Evidence**:
- Lazy regions are keyed by "address-space owner PID"
- Thread groups share address space via `CLONE_VM`
- The `find_process_info_page_owner` function may return wrong PID in some cases

**Investigation steps**:
1. Log PID used for lazy region lookups vs actual thread's PID
2. Verify TTBR0 (page table base) is consistent across all threads in a group
3. Check if terminated threads' PIDs are incorrectly reused

### Theory 4: OOM Handling Corrupts Allocator State

**Hypothesis**: When physical memory runs low (OOM), the kernel's error handling corrupts Go's heap state.

**Evidence**:
- With 256MB RAM, `[DA-DP] ... anon alloc failed, 0 free pages` appears
- OOM handling may return error codes that Go misinterprets as pointers
- Even with 2GB RAM, memory pressure from multiple children could trigger edge cases

**Investigation steps**:
1. Run with `MEMORY=4096M` to eliminate OOM entirely
2. Add logging when demand paging fails due to OOM
3. Verify mmap failure returns correct -ENOMEM to userspace

### Theory 5: Signal Delivery During Allocation

**Hypothesis**: SIGURG for goroutine preemption arrives during `mallocgc` critical section, corrupting allocator state.

**Evidence**:
- Go sends SIGURG to M-threads for preemption
- `mallocgc` is complex with multiple internal data structures
- Go's allocator should be signal-safe since Go 1.14, but kernel-level signal delivery differs

**Investigation steps**:
1. Log all SIGURG deliveries with PC at delivery time
2. Check if any SIGURG arrives while PC is in `mallocgc` range
3. Test with `GODEBUG=asyncpreemptoff=1` to disable preemption signals

## Diagnostic Commands

### Check kernel logs for crashes:
```bash
grep -E "DA-MISS|WILD-DA|SIGSEGV-HEAP" /tmp/akuma_output.txt
```

### Check thread creation:
```bash
grep -E "clone_thread|TRAMP.*alt_sp" /tmp/akuma_output.txt
```

### Check memory state:
```bash
grep -E "DA-DP|anon alloc failed|free pages" /tmp/akuma_output.txt
```

### Check signal delivery:
```bash
grep -E "signal.*deliver|tkill.*sig=23" /tmp/akuma_output.txt
```

## Files of Interest

| File | Purpose |
|------|---------|
| `crates/akuma-exec/src/process/mod.rs` | `fork_process`, `clone_thread`, `entry_point_trampoline` |
| `crates/akuma-exec/src/mmu/mod.rs` | Address space management, CoW handling |
| `src/exceptions.rs` | Page fault handling, signal delivery |
| `src/pmm.rs` | Physical memory manager, CoW reference counting |
| `crates/akuma-exec/src/threading/mod.rs` | Thread state, sigaltstack, pending signals |

## Test Isolation Ideas

1. **Single M-thread (`GOMAXPROCS=1`)**: **Tried (2026-04-14)** — parent still **SIGSEGV** in `read()` with **`-mmap_test`** only. This **does not** point to parent goroutine count alone; keep it for narrowing **child** threading vs allocator.

2. **No forking**: Run child directly without fork - isolates fork/CoW from thread creation

3. **Smaller allocations**: **`forktest_parent`** / **`forktest_child`** accept **`-mmap_alloc_mb=N`** (default **100**) to scale lazy region size without editing Go source (e.g. **`-mmap_alloc_mb=4`** with **`-num_children=1`**).

4. **Disable preemption**: `GODEBUG=asyncpreemptoff=1` - eliminates SIGURG as a factor (still worth testing; not yet logged as definitive)

## Appendix: mmap / demand-paging investigation (implementation notes)

### Serial capture and grep

- **Script:** [`scripts/capture_serial_forktest_mmap.sh`](../scripts/capture_serial_forktest_mmap.sh) — runs [`scripts/run.sh`](../scripts/run.sh) with **`tee`** to **`full.log`** (or the path you pass). Set **`MEMORY=2048M`** (default in script) as needed.
- **Manual:** `MEMORY=2048M ./scripts/run.sh 2>&1 | tee full.log`
- **After a forktest repro over SSH**, search the log for demand paging and faults:

```bash
rg '\[mmap\]|\[DA-MISS\]|\[DA-DP\]|\[WILD-DA\]|\[Fault\]|\[JIT\]|nr=113|exit_group|forktest|mmap_alloc_mb|clock_gettime' full.log
```

For **`[EINVAL]`** / **`mmap`** correlation after the 2026-05-08 logging change, also search:

```bash
rg '\[EINVAL\]|\[EFAULT\]|\[ENOSYS\]|\[mmap-einval\]|mmap_fixed_addr|nr=222|tid=' full.log
```

Correlate **`pid=`** / **`ppid=`** in **`[DA-MISS]`** lines with **`[exit_group]`** PIDs to tie faults to parent vs child address spaces.

### Serial errno diagnostics (2026-05-08)

When a syscall returns **`EFAULT`**, **`ENOSYS`**, or **`EINVAL`**, the kernel may print an extended line (gated by **`SYSCALL_ERRNO_DIAG_EXTRA`** in [`src/config.rs`](../src/config.rs); default **`true`**, set **`false`** for the legacy short format).

**First line** (all dangerous errnos): **`[EINVAL]`** / **`[EFAULT]`** / **`[ENOSYS]`** with **`nr=<n> pid=<p> tid=<t> ELR=<pc-or-?> args=[x0…x5]`** — six AArch64 argument registers at SVC entry, plus the userspace program counter of the system call when the live trap frame is available (**`ELR=?`** if not). Thread id comes from the scheduler slot (**`current_thread_id`**).

**Second line** (only **`nr=222`** **`mmap`** returning **`EINVAL`**): **`[mmap-einval] reason=<token> addr=… len=… prot=… flags=0x…(FLAG|FLAG|…)`**. The **`reason`** token is derived from the syscall inputs using the same predicates as [`sys_mmap`](../src/syscall/mem.rs) (via **`mmap_fixed_addr_unaligned_einval`** and **`mmap_fixed_overlaps_kernel_va`**):

| **`reason=`** | Meaning |
|---------------|---------|
| **`len==0`** | **`x1`** was zero — **`EINVAL`** before address interpretation. |
| **`fixed+unaligned`** | **`MAP_FIXED`** or **`MAP_FIXED_NOREPLACE`** set and **`addr`** not page-aligned — **errno-shaped `addr`** (e.g. **`0xffffffffffffffea`**) produces **`EINVAL`** here without implying **`NULL` was corrupted** if **`MAP_FIXED`** is actually set in **`flags`**. |
| **`kernel_va`** | Fixed mapping would overlap the kernel identity-map VA window. |
| **`other`** | No token matched — inspect **`sys_mmap`** body for that combination of **`prot` / flags / fd**. |

**Reading crash14:** If **`flags`** decodes to normal musl anonymous mmap (**`PRIVATE|ANON`**, no **`FIXED`**) but **`x0`** still looks errno-shaped and **`reason=other`** (or **`EINVAL`** for an unexpected branch), that supports **GPR corruption** rather than the fixed-alignment path alone. If **`FIXED`** appears in the decode and **`reason=fixed+unaligned`**, the kernel is behaving consistently with **unaligned fixed `addr`** even though user source passed **`NULL`** — then prioritize **trap-frame / wrong-register** auditing.

**Trap ELR source:** [`current_trap_frame_elr()`](../crates/akuma-exec/src/threading/mod.rs) reads the pointer saved by **`set_current_trap_frame`** in the EL0 sync handler ([`src/exceptions.rs`](../src/exceptions.rs)).

**Pipe `read` (Pattern 2 parent):** unrelated tag **`[pipe-read]`** — gated by **`SYSCALL_DEBUG_PIPE_READ`** in [`src/config.rs`](../src/config.rs); see [§ Interim conclusions](#interim-conclusions-2026-05-08) for how **`buf=`** vs Go **`fault`** compares.

### Kernel audit: owner PID and lazy regions (read-path checklist)

Code review (no behavioral change required for this appendix):

| Mechanism | Location | Notes |
|-----------|----------|--------|
| Thread-group owner for faults | [`crates/akuma-exec/src/process/children.rs`](../crates/akuma-exec/src/process/children.rs) | **`address_space_owner_pid_for_fault()`** uses **`current_process().tgid`**. |
| Lazy region lookup for faults | Same file | **`lazy_region_lookup_for_page_fault(pid, va)`** tries **`pid`**, then the owner PID if different — aligns **`LAZY_REGION_TABLE`** with **`CLONE_VM`**. |
| EL0 demand paging / CoW | [`src/exceptions.rs`](../src/exceptions.rs) | **`as_owner`** from **`address_space_owner_pid_for_fault()`**; **`fault_mutex`** / **`DaFaultGuard`** on **`as_owner`**; **`lazy_region_lookup_for_page_fault(pid, far)`** for translation faults. |
| **`sys_mmap` lazy policy** | [`src/syscall/mem.rs`](../src/syscall/mem.rs) | Large anonymous maps use **lazy** (`pages > 256`, **`MAP_NORESERVE`**, etc.). |

Regression coverage includes [`src/process_tests.rs`](../src/process_tests.rs) (**`test_lazy_region_lookup_for_page_fault_clone`**, **`test_kill_thread_group_preserves_lazy_regions`**, etc.).

### Branch: parent **`read()` SIGSEGV** vs child allocator

If serial shows **no** suspicious **`[DA-MISS]`** / **`[WILD-DA]`** on children but the **parent** still faults in **`unix.Read`** (**[`userspace/forktest/parent/main.go`](../userspace/forktest/parent/main.go)** pipe drain), treat **pipe + syscall return** separately:

| Layer | Files |
|-------|--------|
| **`read` syscall** | [`src/syscall/fs.rs`](../src/syscall/fs.rs) — **`sys_read`** |
| **Pipe buffers** | [`src/syscall/pipe.rs`](../src/syscall/pipe.rs) — **`pipe_read`**, **`pipe_write`**, waiters |

### `mmap_alloc_mb` flag

- **Parent:** **`forktest_parent -mmap_alloc_mb=N`** (forwarded when **`-mmap_test`** or **`-combined_stress`**).
- **Child:** **`forktest_child -mmap_alloc_mb=N`** (used by **`runMmapStress`**). Lower values reduce lazy region size and fault volume for bisection.

### Empirical allocation threshold (2026-04-14 session)

Conditions: **`export GOMAXPROCS=1`**, **`forktest_parent --duration 10s -mmap_test -mmap_alloc_mb=N`** (default **3 children**). Outcomes below are from **SSH transcripts**; for kernel-side correlation, capture **[`full.log`](../../full.log)** (or another serial path) and use the [grep patterns above](#serial-capture-and-grep).

| `-mmap_alloc_mb` | Typical outcome |
|------------------|-----------------|
| **10** | **Stable** — children **`Received terminated, exiting gracefully`**, parent completes. Reproduced on repeated runs. |
| **50** | **Mixed.** Often stable like 10 MB, but the **same** command also produced **`fatal error: fault`** / **`unexpected fault address 0xffffffffffffffb0`** in **`memclrNoHeapPointers`** → **`mallocgcLarge`** (request size **`0x3200000`** = 50 MiB). Treat as **non-deterministic** at this size. |
| **70** | **Fails** — panics with **`addr=0x2`** or **`addr=0x12`**, and **`0xffffffffffffffb0`** in **`memclrNoHeapPointers`** (same **`pc≈0x86768`** family). |
| **100** | **Fails** — **`addr=0x2`** at **`pc=0x86768`**, **`runMmapStress`** / goroutine 1 (duplicate panics when multiple children hit the bug). |

**Interpretation:** Failures cluster **above ~50–70 MiB per slice** (not exact; **50 MiB can still pass or fail**). Guest **`free`** during the session still showed **~1.6 GB RAM free**, so these are **not** simple OOM from the shell’s view—favor **kernel demand-paging / lazy region / allocator visibility** bugs over “out of memory” until serial **`[DA-DP]`** / **`[DA-MISS]`** proves otherwise.

**Fault address `0xffffffffffffffb0`:** Appears in Go’s **`fatal error: fault`** path while **`memclr`** runs on a large allocation. It is a distinct pattern from **`0x2`** but the **same code site** (`memclr_arm64.s` / **`mallocgcLarge`**). Log both when filing kernel issues.

### `full.log` correlation (serial grep, 2026-04-14)

Captured **[`full.log`](../../full.log)** (~48k lines) from the same session as the table above. Useful command:

```bash
rg -n 'forktest|DA-MISS|WILD-DA|mmap_alloc_mb' full.log
```

**Stable runs** ( **`exit_group` … `forktest_child` `code=0`** , parent `code=0` ):

- **`mmap_alloc_mb=10`**: e.g. execve lines ~2147–2296; exits ~8348–8412.
- **`mmap_alloc_mb=50`** (first batch): ~8587–8718 execve; ~14813–14874 clean exit.

**Failing `mmap_alloc_mb=100`** (children **`exit_group` `code=2`**):

- **`[DA-MISS] pid=137 … va=0x2`** (and pid **138** similarly) → **`[DP] no lazy region for FAR=0x2`** → **`[WILD-DA] pid=137 FAR=0x2 ELR=0x86768`** , **`last_sc=18446744073709551615`** (no syscall active).
- Immediately **before** the first **`[DA-MISS]`** on pid **137**, the log shows **`[EFAULT] nr=113`** (**`clock_gettime`**) with odd args, an **EL1 sync** (**`EC=0x25`**) with **“Kernel accessing user-space address”**, and **`WARNING: … stale TTBR0`**. That lines up with the older “garbage **`clock_gettime`**” note in [Verified Non-Issues](#clock_gettime-syscall) but here it is **adjacent to** the **`FAR=0x2`** wild fault—worth treating as **noise vs cause** only after more runs.

**Failing `mmap_alloc_mb=70`**:

- **`FAR=0x12`** and **`FAR=0xffffffffffffffb0`** on different PIDs; **`[WILD-DA]`** still reports **`ELR=0x86768`**.
- For **`FAR=0xffffffffffffffb0`**, the kernel prints **`*** FAR=0xffffffffffffffb0 is -80 (???) - syscall error used as pointer! ***`** (signed **`FAR == -80`**). Register dumps show **`x0`** values like **`0xffffffffffffffa0`** (another errno-like pattern). Demand-paging path still logs **`[DP] no lazy region for FAR=…`** before **`[WILD-DA]`**.

**Second `mmap_alloc_mb=50`** run (one child faults):

- Same **`FAR=0xffffffffffffffb0`** / **`ELR=0x86768`** chain for pid **205** (~43914–43932).

**Takeaway:** Serial proves the “bad address” is delivered to the fault handler as **`FAR`** on a **demand-paging / lazy miss** path (**`[DA-MISS]`** → **`no lazy region`** → **`[WILD-DA]`**), not only as a Go-side panic string. The PC **`0x86768`** matches userspace **`memclr`**. Next kernel step: determine **why `ELR`/`FAR` pair** ends up as **`0x2`** / **`-80` sign-extended** (bad **`mmap` return**, bad **`brk`**, or **corrupted register state** across syscall) while **`[DP]`** correctly reports **no** lazy mapping for that bogus VA.

## Summary & status (2026-05-07)

### A. `EFAULT` / lazy-region owner (`tgid`) — fixed class

Many `addr=0x2`-style panics were consistent with the kernel returning **`EFAULT` (`-14`)** from syscalls that touched **lazy-mapped Go heap** while **`lazy_region_lookup` used `read_current_pid()`** instead of the **thread-group owner (`tgid`)**. Under **`CLONE_VM`**, lookup failed, demand paging was skipped, and **`copy_to_user` / validation** failed → **`EFAULT`** → Go mis-handled the negative value as an address (including **`FAR = -14 + 16`** patterns).

**Fix (landed):** resolve the address-space owner for lazy regions and mmap metadata (`address_space_owner_pid_for_fault` / **`tgid`**) in **`lazy_region_lookup`**, **`sys_mmap`**, **`sys_mremap`**, **`sys_munmap`**, **`sys_mprotect`**, **`sys_madvise`**, etc., so worker threads don’t strand lazy regions or physical pages on the wrong `Process`.

### B. OOM / `user_frames` leak on `CLONE_VM` workers — fixed class

Heavy **`forktest`** also produced **`[DA-DP] … anon alloc failed, 0 free pages`** when **worker threads** mapped memory into per-thread `UserAddressSpace` slices that **skipped freeing `user_frames` on `drop` for `shared` address spaces**. Routing mappings through the owner **`tgid`** aligns teardown with Linux-like sharing semantics.

### C. `clock_gettime` / SIGURG / syscall frame — **open**

**`crash4.log` (serial):** After rejecting pointer-like **`x0`** with **`EINVAL`**, user-visible **`unexpected fault address 0xfffffffffffffffa`**: kernel register dump shows **`x0 = -22` (`EINVAL`)** and **`FAR = -6`** (**`-22 + sizeof(timespec)`** on AArch64), i.e. **errno in `x0` treated as a base pointer** inside Go’s runtime.

**`crash5.log` (serial, `MEMORY=2048M`, `-mmap_alloc_mb=70`):**

- First failing child sequence (**kernel with H4 recovery; reverted after this capture**): **`tkill(…, sig=23)`** (**`SIGURG`**) → NDJSON **`clock_gettime_recover_x0_as_tp`** (**hypothesis `H4`**, `x0≈0x1e…` heap) → **`[DA-MISS] va=0x10`**, **`[WILD-DA] pid=95 FAR=0x10 ELR=0x86768`**, fault **`pc` matches `memclr`**, register dump **`x0=0x0`**.
- Second run (another child): **`[JIT] IC flush + replay #1 bogus nr=8053579784 ELR=0x86768`** then **`tkill(…, sig=23)`** → **`FAR=0x12`**, same **`ELR`**, **`exit_group` `code=2`**. No **`H4`** line required for that path.

**Conclusion:** **`clock_gettime`-only heuristics do not resolve forktest**; **`recover x0 as tp` was removed** from [`src/syscall/time.rs`](../src/syscall/time.rs) after **`crash5`**. Remaining work aligns with **async preemption (`SIGURG`)**, **SVC / JIT IC flush replay**, and **signal return** correctness (see [`src/exceptions.rs`](../src/exceptions.rs), [`FIX_MEMORY_MAPPING.md`](FIX_MEMORY_MAPPING.md), [`EPOLL_PERFORMANCE.md`](EPOLL_PERFORMANCE.md)).

### D. SSH “hang” / disconnect

**`crash5.log`** ends with **`forktest_parent` / children `exit_group` `code=0`**. An SSH **“Connection closed”** afterward is **not** demonstrated as a guest deadlock from this capture alone—check **host timeouts** and **parallel serial tee**.

### E. Pure-C `mmap_stress` + `crash14` serial (2026-05-08)

**Motivation:** Swap **`forktest_child`** (Go) for **`/bin/mmap_stress`** (static musl: **`mmap` → `memset` → `munmap`**, see [`userspace/forktest/c_stress/README.md`](../userspace/forktest/c_stress/README.md)) via **`forktest_parent --use_c_child`**, to see whether crashes are **only** Go’s heap/runtime.

**Results:**

1. **`mmap_stress` alone** (no parent farm): can finish many iterations and **`exit_group code=0`** — the **anonymous mmap demand-paging path is not unconditionally broken** for a single process.

2. **`forktest_parent --use_c_child --duration 10s --mmap_test=true --mmap_alloc_mb=70`:** the **parent still crashes** in **[Pattern 2](#crash-pattern-2-parent-process-heap-corruption)** — **`unix.Read`** on the epoll pipe (**`PC≈0x13060`**), fault address in the **heap VA range** (e.g. **`0x13d96000`**). Only children were swapped to C; **parent remains Go + epoll + pipe `read`**.

3. **`crash14.log`** (serial, same style of run):

   - **`forktest_parent`** exits **`exit_group … code=2`** shortly after **`[signal] deliver sig=11`** with **`fault_pc=0x13060`** (**SIGSEGV** on the parent thread draining pipes — matches userspace **`read`** trampoline).

   - **Immediately before** that, kernel prints **`[EINVAL] nr=222 pid=<child>`** with **`args=[0xffffffffffffffea, …]`**. **`nr::MMAP` is 222** in [`src/syscall/mod.rs`](../src/syscall/mod.rs). **`0xffffffffffffffea`** is **`−22` (`EINVAL`)** in **`x0`** — the **first argument to `mmap`** (address hint). The C source only ever passes **`NULL`**; this implies **GPR corruption at syscall entry** (or a logging/TID mix-up—either way, investigate trap path). With **extended errno logging** enabled, compare **`tid`** / **`ELR`** on that line to the faulting thread; read **`[mmap-einval] reason=`** and the **`flags=(…)`** decode—see [Serial errno diagnostics](#serial-errno-diagnostics-2026-05-08).

   - **After** the parent dies, **`mmap_stress`** children often **`exit_group code=0`** — they can **outlive** the parent until **`SIGTERM`** / duration; **child `code=0` does not prove** the parent **`read`** path is safe.

**Interpretation:** Parallel **mmap storm + parent pipe I/O** correlates with **errno-shaped words appearing in syscall argument registers** — seen on **`crash13`** (**xattr / `clock_gettime`**) and now on **`mmap`** in **`crash14`**. That strengthens the hypothesis that **kernel-side syscall / trap-frame / `clone` / `rt_sigreturn` plumbing** must be audited—not only Go’s **`mallocgc`**.

**Regression coverage:** In-kernel tests **`mmap_fixed_addr_unaligned_einval_helper`** and **`mmap_einval_through_handle_syscall`** ([`src/tests.rs`](../src/tests.rs)) pin the **`MAP_FIXED` + unaligned `addr` → EINVAL** path and the **`len==0`** path so **`mmap_fixed_addr_unaligned_einval`** in [`src/syscall/mem.rs`](../src/syscall/mem.rs) cannot drift from **`sys_mmap`** without CI catching it.

**Mitigations while debugging:** `GODEBUG=asyncpreemptoff=1`, ample RAM, smaller `-mmap_alloc_mb` for bisection; reduce contention with **`--num_children=1`** when isolating parent vs child behavior.

## Agent handoff (2026-05-08, updated with crash23)

Use this section to pick up **`forktest_parent`** + **`--use_c_child`** + **`-mmap_test`** investigation without re-reading the whole thread. **Root cause is not closed.**

### Handoff notes from **`crash23.log`**

- **Q4 is now serial-documented:** Go parent (**pid 123**) + Go children in **one boot** — **`SIGURG`/`tkill`** overlapping **`fault_pc=0x13060`**, **`[SIGSEGV-HEAP]`** (**`elr=0x402b30a0`**) immediately before **`deliver sig=11`** (**`fault_pc=0x13060`**), plus **`forktest_child`** **`nr=113` → `WILD-DA`** (**pids 127/128**). Confirms **messy Q4** from interactive SSH is reproducible on serial with **PID/timestamp order**.
- **Q3 two-child** block (**~T155**): **`pattern2_parent` `code=0`** while **`forktest_child` pid 110** fails — **best short kernel trace** for **`EINVAL nr=113`** + **`ELR≈memclr`** without Go parent in the loop.
- **Q1** (~**T39**): **`forktest_parent` pid 90** dies **`code=2`** at **`fault_pc=0x13060`**; **`mmap_stress`** survives — parent failure **does not require** child **`WILD-DA`** first.
- **Still missing on this kernel binary:** **`[sigsegv-syscall]`** — next agent should verify **`DEBUG_SIGSEGV_SYSCALL_STUB`** at build time when correlating **`x8`** with **`fault_pc=0x13060`**.
- **Ignore boot **`exit_group_kills_siblings`** test failure** when grep-mining **`crash23`** for forktest (listed in [§ crash23.log quadrant timeline](#crash23log-quadrant-timeline-2026-05-08)).

### Scope

- **Parent:** Go binary **`forktest_parent`** ([`userspace/forktest/parent/main.go`](../userspace/forktest/parent/main.go)) — **epoll loop**, **`read`** on pipe FDs, **`EpollCtl`** `EPOLL_CTL_MOD` re-arm ([`main.go`](../userspace/forktest/parent/main.go) ~238–245).
- **Children:** **`/bin/mmap_stress`** (pure C, musl) — heavy **`mmap` / memset / munmap** ([`userspace/forktest/c_stress/`](../userspace/forktest/c_stress/)).
- **Failure:** intermittent **Pattern 2**-style death: **`SIGSEGV`**, userspace reports **`PC≈0x13060`**, fault addresses in **`~0x13……`** / **`~0x129……`** range (not obviously the pipe **`read`** buffer pointer **`~0x1e008……`**).

### Established conclusions (do not re-litigate without new evidence)

1. **`PC=0x13060` is not specific to `read`.** In static **`forktest_parent`**, it is the shared **AArch64 syscall trampoline** (`internal/runtime/syscall/asm_linux_arm64.s` → **`Syscall6`**). Crashes have been attributed in Go stacks to **`unix.Read`**, **`unix.EpollCtl`**, etc.—**the same `PC`** appears for **different syscalls**. **Inferring “pipe read bug only” from `PC` is wrong.**

2. **`crash17.log` (serial + same-session SSH)** — two **`forktest_parent` runs in one boot log:
   - **Run A (`pid=90`, ~`T22`):** Last **`[pipe-read] enter`** with **`fd=4`**, **`buf=0x1e0087708`**, **`cnt=1024`**; **immediately after**, **`tkill(tid=8, sig=23)`** (`SIGURG`); then **`deliver sig=11`**, **`fault_pc=0x13060`**. No **`[pipe-read] EFAULT`** (validate or **`copy_to_user`**) before death.
   - **Run B (`pid=97`, ~`T92`):** Dense **`[pipe-read] enter`** storm; **no** adjacent **`tkill(sig=23)`** line before **`sig=11`** in the captured window; **same** **`fault_pc=0x13060`**, **`slot=8`**. SSH traceback for run B showed **`EpollCtl`** at **`main.go:243`** (re-arm), not **`Read`**—consistent with **§1**.

3. **Fault address vs buffer.** Go **`fault` / `addr`** (e.g. **`0x1343f000`**, **`0x13a53000`**) has **not** been shown to lie in **`[buf, buf+count)`** for the **`read`** shown in registers (**`buf≈0x1e0087708`**, **`count=0x400`**). Do **not** assume a trivial **`copy_to_user`**-to-pipe-buffer failure without kernel **`FAR`** / **`copy_to_user`** path proof.

4. **`GODEBUG=asyncpreemptoff=1`** — In at least one session, **first** forktest run **succeeded**, **second** run in the **same shell** still **crashed**. **`asyncpreemptoff`** is **not** a reliable fix. Treat **`SIGURG`** as **sometimes correlated** (see run A), **not** always present (run B) and **not** proven sole cause.

5. **Children / `mmap`:** Serial **`[EINVAL] nr=222`** with **`x1=0`**, errno-shaped or mmap-base-shaped **`x0`**, garbage **`prot`/`flags`** → **`reason=len==0`** is real; **decoded `flags=(FIXED|…)` text may be meaningless** when registers are garbage. Supports **SVC entry / GPR corruption** under load, **not** “musl called `mmap` wrong” in source.

### Already in tree (landed before next agent)

| Item | Location |
|------|----------|
| Extended errno lines (`tid`, `ELR`, full args; **`[mmap-einval]`**) | [`src/syscall/mod.rs`](../src/syscall/mod.rs), **`SYSCALL_ERRNO_DIAG_EXTRA`** in [`src/config.rs`](../src/config.rs) |
| **`[pipe-read]`** tracing | [`src/syscall/fs.rs`](../src/syscall/fs.rs), **`SYSCALL_DEBUG_PIPE_READ`**, **`SYSCALL_DEBUG_PIPE_READ_SAMPLE`** in [`src/config.rs`](../src/config.rs) |
| **`mmap`** unaligned fixed **`EINVAL`** before **`lookup_process`** | [`src/syscall/mem.rs`](../src/syscall/mem.rs) |
| Regression tests | [`src/tests.rs`](../src/tests.rs): **`mmap_fixed_addr_unaligned_einval_helper`**, **`mmap_einval_through_handle_syscall`** |
| **`current_trap_frame_elr()`** | [`crates/akuma-exec/src/threading/mod.rs`](../crates/akuma-exec/src/threading/mod.rs) |
| Log grep helper | [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh) |

### Open hypotheses (priority order)

1. **Trap frame / syscall return / `rt_sigreturn`** loses or swaps GPRs across **nested signals** (`SIGURG`, **`SIGSEGV`**) or across **scheduler / syscall** boundaries.
2. **`copy_from_user`** / **`copy_to_user`** interaction with **lazy mappings** or **wrong ASID / TTBR / owner PID** for **`CLONE_VM`**-adjacent bugs — less ruled out than once thought after **`tgid`** fixes, still worth verifying on **parent** path.
3. **Epoll + pipe** interaction bugs — **secondary** until **`x8`** (syscall nr) at fault is proven; **`read`** and **`epoll_ctl`** share **`PC`**.

### Recommended next steps (for the next agent)

1. ~~**Log syscall number at fault:**~~ **Done in-tree** (`[sigsegv-syscall]`, [`src/exceptions.rs`](../src/exceptions.rs); **`DEBUG_SIGSEGV_SYSCALL_STUB`** in [`src/config.rs`](../src/config.rs)). **`crash23.log`** had **no** **`[sigsegv-syscall]`** lines — **confirm toggles + rebuild** before relying on serial for **`x8`**. **Also observable from SSH:** first arg to **`Syscall6`** / **`Syscall`** (**`crash19`** session: **`asyncpreemptoff`** → **`epoll_ctl`** (**`0x15`**); default → **`read`** (**`0x3f`**)).
2. **If `x8=21`:** Audit **[`sys_epoll_ctl`](../src/syscall/poll.rs)** (`copy_from_user` for **`epoll_event`**, **`validate_user_ptr`**).
3. **If `x8=63`:** Trace **[`sys_read`](../src/syscall/fs.rs)** **`PipeRead`** path end-to-end; confirm whether **`copy_to_user`** can return **`EFAULT`** without logging (already logs when **`SYSCALL_DEBUG_PIPE_READ`** on).
4. ~~**A/B:** **`GODEBUG=asyncpreemptoff=1`** vs off~~ — **done** in **`crash19`** SSH + serial (**`SIGURG`** / **`sig=23`** clustering before last **`forktest_parent`** death **`T108–111`**).
5. ~~**Load bisection:** **`--num_children=1`**, **`-mmap_alloc_mb=10`** vs **70**.~~ Partially covered in session (multiple **`forktest_parent`** runs); keep for flaky repros.
6. ~~**Control experiment:** minimal **C parent**~~ — **`pattern2_parent`** ([`userspace/forktest/c_stress/pattern2_parent.c`](../userspace/forktest/c_stress/pattern2_parent.c)) **stable** @ **1** and **3** children in **`crash19`** → **shift focus to Go parent runtime + kernel signal/trap interaction**, not pipe/epoll correctness alone.
7. **Next:** Confirm **`[sigsegv-syscall]`** on serial; minimal **Go** repro (epoll loop only); deep dive **`try_deliver_signal`** / **`do_rt_sigreturn`** when **`SIGURG`** overlaps trampoline (**[`src/exceptions.rs`](../src/exceptions.rs)**); optional audit **TTBR / ASID** for parent thread vs **`mmap_stress`** children under pressure.

See [crash19.log + pattern2_parent session](#crash19log--pattern2_parent-session-2026-05-08) for evidence tables.

### Key kernel files

[`src/exceptions.rs`](../src/exceptions.rs) (`rust_sync_el0_handler`, **`try_deliver_signal`**, **`do_rt_sigreturn`**) · [`src/syscall/fs.rs`](../src/syscall/fs.rs) · [`src/syscall/poll.rs`](../src/syscall/poll.rs) · [`src/syscall/pipe.rs`](../src/syscall/pipe.rs) · [`src/syscall/mod.rs`](../src/syscall/mod.rs)

### Reference captures

**`crash16.log`**, **`crash17.log`**, **`crash19.log`** (SSH + **`pattern2_parent`** control + **`x8`** evidence), **`crash21.log`** (full quadrant matrix), **`crash23.log`** (**Q1–Q4** ordering + **Q4 Go+Go**), **`crash14`** / **`crash5`** — grep patterns in [Serial capture and grep](#serial-capture-and-grep) and **`analyze_kernel_log.sh`**.
