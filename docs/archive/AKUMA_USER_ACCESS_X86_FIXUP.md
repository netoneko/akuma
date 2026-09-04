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

## Known gap, stated

A **non-canonical** address (bit 47 not sign-extended into 48..63) raises
`#GP`, not `#PF`, and `#GP` is fatal on this target. `USER_VA_LIMIT` in the
crate is the 48-bit AArch64 bound, so `validate_user_range` admits
`0x0000_8000_0000_0000..=0x0000_FFFF_FFFF_FFFF`, which x86_64 cannot address
at all. Until that limit is per-arch, a caller must not reach the loop with
such a pointer. Out of scope here (the ask was vector 14 only, and the fix
belongs in the range check, not the copy); it is stated on the asm block.

`CR4.SMAP` stays off, as `proposals/AKUMA_USER_ACCESS_ARCH_PORTABILITY.md`
already scoped; `stac`/`clac` would go inside the copy region when it turns on.

Also found and not touched: `DISK=none amd64/run.sh` page-faults in
`net::init` before the self-tests on the untouched tree (`cr2=0x80_0002_0008`,
no NIC to probe). Pre-existing, unrelated, and why the baseline above was
measured with the default disk.

## Next

`amd64/src/usermode.rs` and `fd.rs` still dereference user pointers raw
(`sys_write`'s per-byte `read_volatile`, `fd.rs`'s "own bounded copy
helpers"). Every one of those is now a `copy_from_user_safe`/`copy_to_user_safe`
call away from being fault-safe, and that conversion is the point of this
work. `akuma-el0-entry` (`invalid register 'x30'`) and `akuma-elf`'s
`UserAddressSpace` needs remain the next two named blockers on the
`akuma-exec` chain, unchanged from the gate-fix doc.
