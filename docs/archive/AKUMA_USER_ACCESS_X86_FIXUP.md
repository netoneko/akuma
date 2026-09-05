# A real x86_64 fault-recovery user copy for `akuma-user-access` — 2026-09-05

The second half of `proposals/AKUMA_USER_ACCESS_ARCH_PORTABILITY.md`, after
`docs/archive/AKUMA_USER_ACCESS_GATE_FIX.md`'s gate fix. Where that pass made
the crate *compile* for `x86_64-unknown-none` with `copy_from_user_safe` left
as a deliberate `unimplemented!()`, and ran one experiment that came back
negative, this pass gives it a real, working, boot-verified x86_64 arm —
proven by three page faults actually taken inside the copy loop and recovered,
not by a clean build.

Unlike `akuma-mmu` (`amd64/src/paging.rs`) and `akuma-threading`
(`amd64/src/sched.rs`), there was **no proven port target to copy from**: amd64
dereferenced user pointers raw and its `#PF` handler was purely diagnostic. The
mechanism had to be built here.

## What was built

### The copy loop (`crates/akuma-user-access/src/lib.rs`)

A `#[cfg(target_arch = "x86_64")]` `global_asm!` block with the same three
symbols the AArch64 block exports, plus two range labels:

```
__arch_copy_user_region_start:
__arch_copy_user_memory:         mov rcx, rdx; cld; rep movsb; xor eax, eax; ret
__arch_copy_user_memory_bytes:   a byte loop (the differential-sweep oracle)
__arch_copy_user_region_end:
__arch_copy_user_fault:          mov eax, 14; ret
```

`rep movsb`, not a hand-tiered loop: one restartable instruction whose whole
state is `rcx`/`rsi`/`rdi`, updated by the CPU as it goes. That is what makes
the recovery argument short — a serviced fault re-executes it and it resumes at
the right byte; an unserviceable one leaves a copied prefix, the same
partial-write contract the AArch64 side documents. With ERMS it is also the
fastest copy the ISA has at these lengths, though nothing has been measured
yet; `docs/archive/USER_COPY_BYTE_LOOP.md` is what measuring it would look like.

The AArch64 invariants carry over with x86 names and are stated on the asm:
leaf and stackless (so `iretq` restores an `rsp` the trampoline can `ret`
through), caller-saved registers only, and the exception path must not eat
them (below). Plus one new one: `cld`, because `rep movsb` honours `DF`.

### No armed handler — an exception-table check instead

The AArch64 arm arms a per-thread trampoline address in
`akuma_primitives::preempt` around each copy and the EL1 handler consults it.
x86_64 does **not** do that, and this is a deliberate divergence, not an
omission: `current_tid()` is a constant `0` on this target (the amd64 kernel
has no `TPIDRRO_EL0` analogue wired up), so the "per-thread" slot would be one
kernel-wide word, and a task preempted mid-copy would leave it armed for
whatever faulted next.

Instead the page-fault handler asks `akuma_user_access::user_copy_fixup(rip)`
whether the faulting `rip` lies in
`[__arch_copy_user_region_start, __arch_copy_user_region_end)`, and only then
rewrites the saved `rip` to `__arch_copy_user_fault`. That is Linux's
`fixup_exception` shape with a one-entry table: the *instruction* decides, not
a flag. Nothing to arm, nothing to disarm, no window, and a fault anywhere else
is exactly as fatal as before.

### The hand-assembled `#PF` entry (`amd64/src/idt.rs`)

This is the piece the gate-fix experiment showed was needed. Vector 14 no
longer goes through rustc's `extern "x86-interrupt"`; it enters at
`page_fault_entry`, a `global_asm!` stub in the same style as `sched.rs`'s
`switch_context` and `usermode.rs`'s `syscall_entry`:

```
push rbp; push rax; push rcx; push rdx; push rsi; push rdi
push r8; push r9; push r10; push r11        ; 10 pushes = 80 bytes
lea rdi, [rsp + 80]                         ; &PageFaultFrame (error code first)
call page_fault_dispatch                    ; plain extern "C" Rust
pop r11 … pop rbp                           ; exactly what was pushed
add rsp, 8                                  ; drop the error code
iretq
```

Two facts make the offsets and alignment right, both architectural: the CPU
pushes `err, rip, cs, rflags, rsp, ss` for vector 14 in long mode whether or
not the privilege level changed, and it aligns `rsp` to 16 before pushing. So
`[rsp + 80]` after ten pushes is the error code, and `rsp` is 16-aligned at
the `call` as System V requires. `PageFaultFrame` is `#[repr(C)] { error_code,
frame: InterruptStackFrame }` — the existing struct, one field in front.

`page_fault_dispatch` is `#[unsafe(no_mangle)] extern "C" fn(*mut
PageFaultFrame)` and decides, **in this order**:

1. demand paging (unchanged behaviour — the armed lazy region, not-present
   only), return with the frame untouched so the instruction re-executes;
2. user-copy fixup — `user_copy_fixup(frame.rip)` says yes, so write the
   trampoline address into `frame.rip` and return;
3. fatal, as before.

Demand paging before fixup is load-bearing: a copy into a lazy page must be
serviced, not failed, and the boot test pins it.

Saving the nine caller-saved registers around the Rust call is the x86 form of
invariant 3 from `docs/archive/BUSYBOX_HASH_MISCOMPUTE.md`: a `rep movsb` that
faults on a lazy page comes back to re-execute with the `rcx`/`rsi`/`rdi` it
faulted with. Rust preserves `rbx`/`rbp`/`r12`–`r15` on its own.

**The other vectors are untouched** and still `x86-interrupt` by value — they
never return anywhere but where they came from, or never return at all.

### Why the first attempt landed 5 bytes into an instruction

Not resolved by reading rustc's codegen — deliberately. The whole point of the
stub is that it *does not matter* how `abi_x86_interrupt` locates the frame
for a `&mut` parameter: the stub owns the layout, so there is nothing to guess
at and nothing that can silently change under a toolchain bump. The plausible
causes listed in the gate-fix doc stay plausible and unverified.

## Verification

```
cargo build --release                                                 # aarch64 kernel builds, unchanged
cargo clippy --release                                                # clean (akuma-kacho DEADCODE-PROBE
                                                                      #  deprecations are pre-existing)
cargo clippy -p akuma-user-access --target aarch64-unknown-none --release   # clean
cargo clippy -p akuma-user-access --target x86_64-unknown-none  --release   # clean
cargo clippy -p akuma-amd64       --target x86_64-unknown-none  --release   # clean
cargo test --target aarch64-apple-darwin                              # every crate green
```

Then `objdump` on the linked amd64 ELF, since that is where the previous
attempt's failure was visible: `page_fault_entry` is the 26 instructions
above with `leaq 0x50(%rsp), %rdi` and a real `callq page_fault_dispatch`;
the copy region is `[0x…518, 0x…538)` and `__arch_copy_user_fault` begins at
exactly `0x…538` — the end label, excluded from the range.

Then the real thing: `amd64/run.sh` under QEMU `microvm`/TCG, with a new
permanent boot test, `idt::user_copy_smoke_test`, run right after the
demand-paging smoke test:

```
  user copy: kernel-to-kernel copy returns Ok   [OK]
  user copy: kernel-to-kernel copy is byte-exact   [OK]
  user copy: unmapped source returns EFAULT   [OK]
  user copy: unmapped source wrote nothing   [OK]
  user copy: unmapped destination returns EFAULT   [OK]
  user copy: copy after a recovered fault still works   [OK]
  user copy: fault off the end of a mapped page copies the prefix, then EFAULT   [OK]
  user copy: a lazy-region fault inside the loop is demand-paged, not fixed up   [OK]
  user copy: exactly three faults were fixed up   [OK]
  user copy: differential sweep ran   [OK]
  user copy: rep movsb agrees with the byte loop   [OK]
  user copy: only the intermediate tables retained   [OK]

Akuma/amd64 self-test: 185 passed, 0 failed
```

173 baseline + 12. First boot, no iteration. The two checks that carry the
weight: "fault off the end of a mapped page" maps one page at `0x11_0000_0000`
with a known pattern, copies 8192 bytes out of it, and requires `Err(14)`
**and** a byte-exact first 4 KiB **and** an untouched second 4 KiB — that is a
fault taken mid-`rep movsb`, on the 4097th byte, with the CPU's progress
honoured. "demand-paged, not fixed up" copies out of an armed lazy region and
requires `Ok`, all zeroes, and the demand-fault counter up by one with the
fixup counter unchanged — the ordering in `page_fault_dispatch`. The
differential sweep (`copy_loop_differential_sweep`, now built for x86_64 too)
ran `rep movsb` against the byte loop over 72×72 alignments × 22 tier-boundary
lengths on kernel memory with zero mismatches.

The probe is **permanent**, not reverted: it is a `Suite` test like every
other stage's, and a mechanism whose failure mode is "resumes at garbage" is
exactly what should be re-proven on every boot. `DEMAND_FAULTS` gained a
sibling `COPY_FIXUPS` counter for it.

## Part 2, same day: the syscall bodies, and the `#GP` half

The first pass left two things stated and open; both closed within the hour,
for real hardware.

**Non-canonical addresses.** Bit 47 not sign-extended into 48..63 is not a
page fault — the CPU rejects it before translation, as `#GP` with error code
0 — and `#GP` was fatal. Two fixes, belt and braces: `USER_VA_LIMIT` is now
`0x0000_7FFF_FFFF_FFFF` on x86_64 (`#[cfg]`-gated in the crate; AArch64
unchanged), so no validated pointer reaches the loop with one; and vector 13
goes through the same stub as vector 14 — the `global_asm!` became a
`fixable_exception_entry!("<entry>", "<dispatch>")` macro emitting both —
with `general_protection_dispatch` running the fixup query and nothing else
(a `#GP` is never "not mapped yet", so no demand paging). The boot test takes
one on purpose (`copy_from_user_safe` from `0x0000_8000_0000_0000` →
`Err(14)`) and the fixup counter pins four recovered faults: three `#PF`, one
`#GP`.

**The raw dereferences.** `amd64/src/uaccess.rs` is new: `range_ok` (null
page, wrap, kernel half, non-canonical), `read_bytes`/`write_bytes` over
`copy_from_user_safe`/`copy_to_user_safe`, `read_val`/`write_val` for the
small fixed-layout ABI structs, and `read_cstr`, which reads to the end of the
current page at a time so a string ending just before an unmapped page is not
failed by a speculative over-read (Linux's `strncpy_from_user` stops at page
boundaries for the same reason). Every `read_volatile`/`write_volatile`
through a `ptr as *const u8` in `usermode.rs`, `fd.rs` and `sock.rs` — 31
sites — now goes through it: `fd::copy_in` returns `Option<Vec<u8>>`,
`fd::copy_to_user`/`copy_out` return the count **or `errno::EFAULT`** in
syscall-return form (so a caller whose result is the count returns it
directly, and every other caller checks `errno::is_err`), `path_from_user`/
`user_cstr` are one line over `read_cstr`, and the `timespec`/`timeval`/
`iovec`/`pollfd` field reads are single `read_val::<[i64; 2]>`-style copies.
`sys_write` to the console copies through a 256-byte stack chunk rather than
per byte. Three sites deliberately degrade instead of failing: `wait4`'s
status write (the child is already reaped; the pid is the useful half),
`sys_spawn`'s stdin seed (an empty stdin is a state the child handles), and
`sock::read_u64` for optional `msghdr` fields (a bad pointer reads as 0, which
is what a zeroed field is).

**The self-tests broke, correctly.** First boot after the conversion: 43
failures, every one `EFAULT` or `EBADF`, every one an `fd`/`sock`/spawn test
that drives a syscall body with a **kernel-stack buffer** where a program
would pass a user pointer — which `range_ok` refuses, as it must. That is the
exact case the AArch64 kernel's ~85 boot-test sites solved with
`akuma_user_access::BYPASS_VALIDATION`, which is portable; `uaccess` honours
it (waiving only the "is it in the user half" question — the copy stays
fault-safe and a wrapping range is refused regardless), and `main.rs` holds a
`BypassValidationGuard` from `Suite::new` to just before `t.report()`, so
`run_init`'s real program gets real `EFAULT`s. Second boot: **187 passed, 0
failed** (185 + the `#GP` case + a `range_ok` truth table), and `busybox sh`
came up through the converted paths.

Verification repeated in full: aarch64 kernel build + workspace clippy clean,
crate clippy clean on both bare-metal targets, workspace host tests green.

`CR4.SMAP` stays off, as `proposals/AKUMA_USER_ACCESS_ARCH_PORTABILITY.md`
scoped; `stac`/`clac` go inside the copy region when it turns on, and until
then a *mapped* kernel address handed by a program is refused by `range_ok`
alone, not by hardware.

Also found and not touched: `DISK=none amd64/run.sh` page-faults in
`net::init` before the self-tests on the untouched tree (`cr2=0x80_0002_0008`,
no NIC to probe). Pre-existing, unrelated, and why the baseline above was
measured with the default disk.

## Next

`CR4.SMAP`/`SMEP`, with `stac`/`clac` bracketing the copy region — the one
remaining way a program can make the kernel touch a mapped kernel address on
its behalf is a bug in `range_ok`, and SMAP is what makes that class of bug a
fault instead of a read. `akuma-el0-entry` (`invalid register 'x30'`) and
`akuma-elf`'s `UserAddressSpace` needs remain the next two named blockers on
the `akuma-exec` chain, unchanged from the gate-fix doc.
