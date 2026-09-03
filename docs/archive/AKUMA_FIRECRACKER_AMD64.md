# amd64: a second architecture, and the gate that was hiding it

**2026-09-03.** Akuma boots on x86_64. The target is Firecracker; the kernel is a
PVH-noted ELF64 in `amd64/`, and it reaches long mode with a working console.

```
Akuma/amd64 — long mode reached
  hvm_start_info @ 0x0000000000001580
```

The interesting result is not the boot. It is what the boot **found**: one `cfg`
gate in `akuma-cpu` was silently emitting AArch64 instructions into x86 codegen,
and it was taking three quarters of the tree down with it. That gate was listed in
`docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` under *"what is already right, and must
not be regressed."*

## The result

| | before | after |
|---|---:|---:|
| crates building for `x86_64-unknown-none` | 13 | **34** |
| crates failing | 39 | 18 |
| `akuma-cpu` cfg arms rewritten | — | 33 aarch64 + 18 stub |
| aarch64 boot suite (TCG) | 306 PASSED / 0 FAILED | **306 PASSED / 0 FAILED** |

No aarch64 codegen changed. The gate edit is `cfg`-only, and on
`aarch64-unknown-none` both the old and new expressions evaluate identically.

## 1. The gate

`akuma-cpu` gated every instruction on `target_os = "none"`, and its header
documented that choice at length and for a good reason: the crate had first been
written as `cfg(target_arch = "aarch64")`, which is a trap on this project because
the *development host* is `aarch64-apple-darwin` — the gate was true under
`cargo test`, the wrappers really executed, and `tlbi`/`dc cvau`/`mrs esr_el1` are
EL1 instructions, so the first host test died with `SIGILL`.

That reasoning is still correct. What made it insufficient is that
`x86_64-unknown-none` is **also** `target_os = "none"`:

```
error: invalid instruction mnemonic 'mrs'
   --> crates/akuma-cpu/src/lib.rs:552:29
    |   mrs rax, tpidrro_el0
error: invalid instruction mnemonic 'wfi'
   --> crates/akuma-cpu/src/lib.rs:303:31
```

The gate stopped discriminating the moment a second bare-metal target existed. The
correct gate is the **conjunction** — neither half works alone:

```rust
#[cfg(all(target_os = "none", target_arch = "aarch64"))]   // real instruction
#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]  // stub
```

### 1.1 Why one crate cost 39

`akuma-cpu` has an empty `[dependencies]` table and sits at the bottom of the tree.
`akuma-primitives` calls it, and almost everything calls `akuma-primitives`. A leaf
crate that fails to codegen fails every crate above it.

There is a second-order lesson in *how* it hid. `cargo build -p akuma-cpu --target
x86_64-unknown-none` **passed**. Every function is `#[inline(always)]`, so with no
caller nothing was instantiated and no `asm!` reached the assembler. The crate only
failed once something called it. A per-crate build sweep that treats "the leaf
compiles" as evidence is measuring nothing.

### 1.2 What the remaining 18 are

Seven root failures holding 29 raw `asm!` sites; the other eleven are cascades.

| Crate | `asm!` sites | Neutral-able? |
|---|---:|---|
| `akuma-entry` | 8 | No — AArch64 exception-vector and boot entry |
| `akuma-gic` | 5 | No — it *is* the ARM interrupt controller |
| `akuma-threading` | 5 | Partly — context switch is arch, the slot table is not |
| `akuma-el0-entry` | 4 | No — `eret` and the EL0 trap frame |
| `akuma-mmu` | 3 | Partly — the walker is arch, the PTE vocabulary is not |
| `akuma-psci` | 2 | No — `smc`/`hvc` are ARM firmware calls |
| `akuma-user-access` | 2 | Partly |

This is a **better** result than the 18.3% figure in the proposal's §8 suggests, and
it sharpens what that number means: 18.3% of production code lives in crates that
touch hardware, but only 29 `asm!` sites are the part that genuinely cannot cross.
The rest of those crates is neutral code sharing a compilation unit with arch code —
which is exactly the seam the proposal's items 1-4 are about moving.

### 1.3 The stub is a placeholder, not a port

x86_64 currently takes the *host* arm: `dsb_ish` is a no-op, `park::wfi` does not
park, `reg::sp` returns 0. That is survivable **only** because `amd64/` calls none of
these yet, and it must not be read as x86 support.

The split runs along a real fault line. `barrier`, `park` and `cache` can take honest
x86 bodies (`mfence`, `hlt`/`pause`, and no-ops that are *correct* because x86 caches
are coherent with instruction fetch) and that is a small job. `daif`, `tlb`, `vtimer`
and `sysreg` cannot, because they return raw AArch64 encodings: `daif::read()` yields
a register whose bit 7 **set** means *masked*, where the x86 counterpart is
`RFLAGS.IF` whose **set** bit means *enabled* — inverted polarity inside a `u64` that
callers bit-test against AArch64 positions. Giving those an x86 arm under an AArch64
mnemonic would reproduce, one level down, the lossy-encoding-at-a-neutral-seam defect
the proposal exists to fix.

The other `target_os`-only gates in the tree (`akuma_primitives::preempt::current_tid`
among them) are latent duplicates of this bug: harmless while nothing on x86 calls
them, wrong the moment something does.

## 2. Boot protocol: PVH, and why not the other two

Firecracker chooses the boot protocol **from the kernel ELF itself**.
`configure_system_for_boot` matches on `entry_point.protocol`, and an ELF declaring
the PVH note gets `BootProtocol::PvhBoot` instead of `BootProtocol::LinuxBoot`. There
is no Firecracker-side switch — declaring the note is the entire mechanism.

Three candidates were considered:

| Protocol | Entry state | Verdict |
|---|---|---|
| multiboot1 | 32-bit protected mode | **Rejected.** QEMU's multiboot loader requires `EM_386`, so a 64-bit kernel needs an objcopy to `elf32-i386`. Firecracker does not speak it at all. |
| Linux 64-bit boot | already in long mode, paging on, `boot_params` in `%rsi` | **Rejected.** Least code in `boot.s`, but nothing local reproduces that entry state, so every entry-path bug would only appear on the Firecracker host. |
| **PVH** | 32-bit protected mode, paging off, `hvm_start_info` in `%ebx` | **Chosen.** QEMU implements it too, so a local run and a Firecracker run take the *identical* entry path. |

The deciding factor was reproducibility, not code size. The 64-bit path would have
deleted the whole trampoline below; it would also have made the trampoline the one
piece of the kernel that could never be tested on the dev machine.

A useful side effect: `linux-loader` requires ELFCLASS64, so the PVH path needs **no
objcopy and no flat binary** — unlike the aarch64 target, Firecracker consumes the
linked ELF directly. `scripts/link_kernel.sh` has no amd64 counterpart and needs none.

### 2.1 The note

```asm
.section .note.Xen, "a", @note
    .long 4                          /* namesz: "Xen\0" */
    .long 4                          /* descsz: one u32 */
    .long 18                         /* XEN_ELFNOTE_PHYS32_ENTRY */
    .asciz "Xen"
    .long _start                     /* 32-bit entry point */
```

Verified in the linked image:

```
Displaying notes found in: .note.Xen
  Xen   0x00000004   Unknown note type: (0x00000012)
   description data: 00 10 20 00          # 0x00201000 == _start
```

`linker.ld` names the `PT_NOTE` phdr **explicitly** rather than trusting lld to
synthesise one. A note present as a section but covered by no program header is
invisible to the loader, and the failure is silent: Firecracker falls back to
`LinuxBoot` and enters in long mode at `e_entry` — i.e. straight into 32-bit
trampoline code. `amd64/run.sh` greps for the note before launching for the same
reason.

### 2.2 The cheapest possible check

`kmain` prints the `hvm_start_info` pointer, and the value identifies the protocol
that was actually used: **QEMU PVH reports `0x1580`; the multiboot prototype reported
`0x9500`.** That one line is how the switch from multiboot to PVH was confirmed to
have taken effect rather than been ignored.

### 2.3 2 MiB pages, not 1 GiB

`boot.s` identity-maps the first 1 GiB with 512 2 MiB pages rather than a single 1 GiB
PDPT entry. A 1 GiB entry requires CPUID `PDPE1GB`, which the default `qemu64` CPU
does not advertise, and the failure mode is a triple-fault at `mov %cr0` with nothing
on the serial line.

This also happens to be what makes the intended host viable. The target machine is an
**Intel Core i5-4460** (Haswell, 2014). Haswell *does* have `PDPE1GB`, so the choice
was not forced by it — but Firecracker officially supports "CPUs released starting
with 2015" and continuously tests only Skylake, Cascade Lake, Zen2 and Neoverse N1, so
that host is deliberately outside the supported window. Asking it for as few CPU
features as possible is the right posture. Beyond long mode and PAE, this kernel
requires nothing.

## 3. Verification

- **aarch64 boot suite under TCG: 306 PASSED, 0 FAILED.**
- Host test suite: green, no failures.
- `sh .git/hooks/pre-commit`: exit 0 (clippy `-D warnings` across every crate, release
  and extreme-size profiles, host tests).
- The `amd64/` package is absent from `default-members` and the hook loops `crates/*/`,
  so neither reaches it.

### 3.1 Two verification methods that did not work

**Binary hash comparison is useless here.** The first attempt at proving the
`akuma-cpu` edit was a no-op on aarch64 compared `shasum` of the linked kernel before
and after. They differ — but only because `akuma-cpu`'s SVH feeds symbol names, so
editing a doc comment perturbs the image. The hash says nothing about semantics.

**Under HVF the boot suite cannot complete.** `cargo run --release` aborts with
`Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 2437`. This
is pre-existing and unrelated: rebuilding with the *unmodified* `akuma-cpu` aborts at
the identical point, immediately after `OK: fully unmapped source returns EFAULT`.
`scripts/cargo_runner.sh:226` already documents the assert and offers `HVF=0`, which
is what produced the 306-PASSED run above. Any future A/B on this tree needs `HVF=0`.

## 3.5 Stage A: the memory subsystem runs on amd64

Added the same day. The heap and the physical frame allocator now come up on
x86_64 using **unmodified** `akuma-alloc` and `akuma-pmm`:

```
  ram:  0x0000000000100000 .. 0x000000001ffe0000
  kernel ends 0x000000000024d0b9
  heap: 0x000000000024e000 + 16 MiB ... ok
  pmm:  init(base=0x0000000000100000, size=510 MiB, reserved_to=0x000000000124e000)
  pmm:  126354 free frames (493 MiB)
  test: heap vec[4096] sum=22898104320
  test: pmm alloc 8 frames, free 126354 -> 126346 -> 126354   [OK]
```

This needed **none of the proposal's six items**. It needed a memory map, which
PVH supplies, and two crates that were already neutral. That is the thesis of
`REDUCING_PLATFORM_DEPENDENCY.md` demonstrated rather than argued.

The smoke test is deliberately not a "did init return" check. `22898104320` is
Σi² for i < 4096 = 4095·4096·8191/6 — an exact match proves the heap stores and
reads back 32 KiB rather than merely handing out a pointer, and the free count
moving 126354 → 126346 → 126354 proves frames are actually reserved and returned.

### 3.5.1 The ordering is backwards from intuition

**The heap must be up before the PMM.** `akuma_pmm`'s `init` allocates its own
free-page bitmap with `alloc::vec![0u64; n]`, so a PMM initialised first faults
inside the allocator. A frame allocator feels like the more primitive thing and
here it is not.

That forces the layout: the heap is carved statically (16 MiB) out of the region
above the kernel image, and the PMM is then told its reservation runs to
`heap_end`, not `kernel_end`. Passing `kernel_end` would hand out frames the
allocator is already using — and it would not fault immediately, which is the
worst kind of wrong.

### 3.5.2 Two choices that are load-bearing

**The RAM region is picked by containment, not by size.** `pick_region` selects
the region that *contains the kernel image*. Largest-usable would have given the
same answer here and is right almost always; containment is right by
construction.

**Everything is clamped to 1 GiB**, because that is exactly what `boot.s`
identity-maps. The VMM will happily report more RAM than the kernel can address,
and handing those frames to the PMM would produce a page fault with no IDT
installed — i.e. a triple-fault and a guest that vanishes silently.

### 3.5.3 `akuma-cpu` gained honest x86 arms

`barrier` and `park` are no longer stubs; both carry a mapping table in their
module docs. Two entries are lossy and documented as such:

- **`isb` → `lfence` is an approximation.** `lfence` is a dispatch barrier, not
  an architecturally serialising instruction (only `cpuid`, `iret` and CR writes
  are). It is redundant rather than insufficient in practice, because the x86
  operations one would put an `isb` after (`mov %cr3`, `wrmsr`) serialise
  themselves. Chosen over `cpuid`, which clobbers four registers.
- **`wfe` → `pause` and `sev` → `nop` are only correct as a pair.** x86 has
  neither, so `wfe` becomes a spin hint that does not sleep, and `sev` therefore
  has nothing to wake. Changing one without the other breaks it.

`cache` keeps the shared no-op arm, and its module note now records that on x86
this is **correct rather than a stub**: x86 caches are coherent by architecture
and the instruction cache snoops stores, so no maintenance is required. The one
thing to watch there is `line_size`, which returns a hardcoded 64 rather than
reading `CPUID`.

## 3.6 It boots under Firecracker, on real hardware

**2026-09-03, Firecracker v1.16.1, AMD Ryzen 7 8845HS (Zen 4), Pop!_OS 22.04,
kernel 6.17.9, native KVM.** Not emulation — the same ELF QEMU boots locally, run
by Firecracker on a real x86 machine. `amd64/run-firecracker.sh` does it in one
command (`FC_HOST=user@host`).

```
  hvm_start_info @ 0x0000000000006000
  version=1 modules=0 rsdp=0x0000000000000000 cmdline=0x0000000000020000
  memmap: 4 entries
    0x0000000000000000 + 0x000000000009fc00  RAM
    0x000000000009fc00 + 0x0000000000040400  reserved
    0x00000000eec00000 + 0x0000000010000000  reserved
    0x0000000000100000 + 0x000000001ff00000  RAM
  usable RAM: 511 MiB
  pmm:  126385 free frames (493 MiB)
  test: heap vec[4096] sum=22898104320
  test: pmm alloc 8 frames, free 126385 -> 126377 -> 126385   [OK]
  test: paging map/write/verify/unmap @0x0000000040000000   [OK]
  test: W^X encoding   [OK]
```

Four things only the real machine could establish.

**The PVH gamble paid off exactly as designed.** `hvm_start_info` is at
**`0x6000`** here against QEMU's **`0x1580`** — different address, identical code
path, no `#[cfg]` anywhere. That is the whole reason §2 chose PVH over the 64-bit
LinuxBoot protocol, and printing the pointer (§2.2) is what makes it checkable at
a glance. `cmdline=0x20000` is Firecracker's `CMDLINE_START`, matching its source.

**`rsdp_paddr` is 0 on Firecracker too — correcting §4.** The earlier note said
PVH hands over the ACPI root pointer so the "scan the BIOS area for `RSD PTR `"
step never has to exist, and flagged that QEMU reported 0 and Firecracker needed
checking. Now checked: **Firecracker v1.16.1 also reports 0.** The field is in the
ABI and neither VMM populates it, so when ACPI is eventually needed the RSDP will
have to be found some other way. Do not build on that field.

**Picking the RAM region by containment, not by size or order, was load-bearing.**
Firecracker reports **4** entries where QEMU reports 7, and — the part that
matters — it lists the main RAM region **last**, after a reserved region at
`0xeec00000`. Any implementation that took the first RAM entry would have chosen
the 640 KiB block at 0, and one that scanned in order and stopped early would
differ between the two VMMs. §3.5.2's "right by construction" was not a
hypothetical.

**`drives` and `network-interfaces` are mandatory in the single-JSON config.**
Even when empty. Firecracker rejects the file outright:

```
RunWithoutApiError error: Failed to build MicroVM from Json:
  Invalid JSON: missing field `drives` at line 10 column 1
```

They are not defaulted, and this differs from the API path. The machine has no
disk and no NIC, so both stay `[]`.

### 3.6.1 `EFER.NXE` — found before it could bite

Stage B's first blocker was in `boot.s`, not in the page-table code: it set
`EFER.LME` and not `EFER.NXE`. Without NXE, **bit 63 of a PTE is a reserved bit,
not the no-execute flag** — setting it does not mark a page non-executable, it
makes every access to that page fault with the reserved-bit error. A kernel that
omits it and then tries to enforce W^X gets the exact opposite of what it asked
for. It is now set in the same `wrmsr` as LME, so no page-table code can run
before it is in force.

## 3.7 Stage C: an IDT, and demand paging

```
  test: demand paging 4 faults serviced, frames 126380 -> 126378   [OK]
```

`amd64/src/idt.rs`. Before it, *every* fault was fatal and invisible: with no IDT
loaded a page fault escalates to a double fault and then a triple fault, and the
VMM resets the guest with nothing on the serial line. Every bug in the earlier
stages had that same symptom — silence — which is why those modules bounds-check
so aggressively. This replaces that discipline with a diagnostic.

The test arms a lazily-backed range at 2 GiB (outside the identity map *and*
clear of the 1 GiB address `paging::smoke_test` uses, so it cannot pass by
accidentally hitting an existing mapping), stores to four pages, and lets `#PF`
service each one: allocate a frame, zero it, map it, `iretq` re-executes the
faulting store. It then reads all four back — a handler that mapped a page and
then lost it would still let the store retire — and unmaps them, checking the
frame count returns to where it started so a leak cannot hide.

**No hand-written entry stubs.** rustc's `x86-interrupt` calling convention
synthesises the uniform frame and the `iretq`, so the handlers are ordinary
`fn`s. That is a compiler feature, not a dependency.

**A protection fault inside the lazy range is deliberately still fatal.** Only
error-code bit 0 clear (*not present*) is demand paging; bit 0 set means a write
to something deliberately read-only, and servicing that would silently defeat the
protection. This is the same grant-vs-deny distinction as
`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`.

**No TSS and no IST**, so a double fault runs on the faulting stack. Fine while
nothing can overflow it, wrong the moment a guard page exists — a stack-overflow
double fault would fault again pushing its own frame and triple-fault. When a
guard page appears, vector 8 needs an IST entry *before* it.

## 3.8 `timeout` silently breaks Firecracker on a terminal

Worth its own section because the symptom is indistinguishable from a kernel that
does not boot, and it only reproduces interactively.

```
$ ./run.sh
2026-09-03T23:31:55 [anonymous-instance:main] Running Firecracker v1.16.1
$                          # ...and nothing else, ever
```

`timeout` runs its child in **its own process group**, so the child is no longer
the foreground process group of the controlling terminal. Firecracker attaches
guest serial input to stdin; reading the TTY from a background process group
raises `SIGTTIN`, which stops the process immediately after it prints its banner.
The guest never runs, and there is no error message.

It does not reproduce over a pipe — `ssh` with no `-t` has no controlling TTY, so
plain `timeout` is fine there. Every automated run in this document went through
a pipe, which is why the bug survived until someone ran it by hand.

The fix is `timeout --foreground`, which leaves the child in the shell's process
group. `amd64/run-firecracker.sh` stages a `run.sh` that uses it and tees to
`boot.log`.

## 3.9 Stage D: hardware interrupts, and `MemAttr` earns its place

```
  lapic: base=0x00000000fee00000 id=0 timer vector=32 periodic
  test: timer interrupts 5 ticks in 612346 spins   [OK]     # QEMU
  test: timer interrupts 5 ticks in 10691123 spins   [OK]   # Firecracker, Zen 4
```

`amd64/src/lapic.rs`. Everything before this ran with `IF` clear from `boot.s`
onward: the kernel could fault, but nothing could *interrupt* it. A timer tick is
the prerequisite for preemption and therefore for a scheduler.

The spin counts are worth keeping. The same five ticks cost **612 K** spins under
QEMU and **10.7 M** on the Zen 4 — a real CPU spins ~17x further between ticks
than an emulated one. That ratio is itself the evidence that the ticks come from
a clock rather than from anything correlated with instruction count.

**Still no ACPI.** The LAPIC does not need discovering: its base is in
`IA32_APIC_BASE` (MSR `0x1B`), one `rdmsr` away. That is why a preemption timer
sits *before* ACPI in the plan rather than after it — the IOAPIC is what genuinely
needs the MADT.

### 3.9.1 The first real consumer of `MemAttr`

The LAPIC lives at `0xFEE0_0000`, above the 1 GiB `boot.s` identity-maps, so it
must be mapped explicitly — **and uncacheable**. A writeback-cached device mapping
lets the CPU satisfy a read from cache and never issue the access at all, which
makes a polled register appear frozen.

`paging::encode` was written in §3.5 with a deliberate hole: it took a `Prot` and
no attribute, with a comment saying the `MemAttr` half of item 1's
`encode(prot, attr)` was "deliberately absent rather than stubbed with a value
that would look meaningful". It is now present, because something needed it —
`MemAttr::{WriteBack, Device}`, two PTE bits (`PCD | PWT`) here and an `AttrIndx`
into `MAIR_EL1` on AArch64. No consumer cares which, which is the entire point of
the neutral vocabulary item 1 proposes.

That is the second time this port has produced evidence *for* item 1 rather than
consuming it: §3.5 showed the permission half cannot cross, and this shows the
attribute half is real rather than speculative.

### 3.9.2 Mask the legacy PICs before the first `sti`

The 8259s power up with lines unmasked and vectors overlapping the CPU exception
range, so enabling interrupts without masking them invites a spurious IRQ that
decodes as a `#GP` with a garbage error code. Four `outb`s, not optional. Nothing
here uses them — the serial console is polled — so they are masked rather than
remapped.

The timer's initial count is deliberately **uncalibrated**: the LAPIC counts at
the core crystal frequency, which needs CPUID leaf `0x15` or calibration against
another clock to convert to wall time, and nothing needs wall time yet. It only
has to tick fast enough for a bounded spin loop to observe. That loop is bounded
precisely so a timer that never fires reports a failure instead of hanging the
boot.

Port I/O moved out of `serial.rs` into `amd64/src/port.rs` when this became its
second consumer.

## 3.10 Stage E: context switching and a scheduler

```
  test: scheduler 3 tasks x 4 rounds, 5 switches, ticks=17   [OK]
  test: tick-driven resched observed   [OK]
```

`amd64/src/sched.rs`. Three tasks on separate stacks, round-robin, paced by the
LAPIC tick.

**It is a real context switch and it is not preemption.** A task that never calls
`yield_now` runs forever; the tick sets a flag the yield consumes. True
preemption means switching *inside* the interrupt handler so `iretq` returns onto
a different task's stack, which needs each task's interrupt frame on its own
stack and a TSS once ring 3 exists. The module says so in its header rather than
letting "scheduler" imply more than was built.

The test proves the switch is real rather than a function call that returns: each
worker accumulates a checksum in a **local**, read and written across a yield, so
a switch that failed to preserve the task's stack or callee-saved registers
produces the wrong value. The expected checksums are computed independently.

### 3.10.1 The test caught its own shortcut

First run: `test: scheduler ... ticks=5 [OK]` and
`test: tick-driven resched never observed [FAIL]`.

Nothing was broken. The workers yielded immediately, so the whole test finished
inside a single timer period and observed zero ticks — `ticks=5` was left over
from the LAPIC test. The scheduler was correct and the *pacing claim* was not.
Fixed by making each round wait for a tick (bounded, so a dead timer fails rather
than hangs) and shortening the timer period from 1,000,000 to 100,000.

This is the argument for reporting the two properties separately. A single
combined "scheduler works" line would have passed, and the fact that the tick
drove nothing would have gone unnoticed.

### 3.10.2 `global_asm!` inherits the previous file's section

Adding `switch_context` broke the link with an error that names the wrong file:

```
error: BSS section '.bss' cannot have non-zero bytes
```

Module-level `global_asm!` blocks from every module are concatenated into one
object file, and **the assembler carries its current section across that
boundary**. `boot.s` ends with `.section .bss` (the boot stack), so the new
block's instructions were emitted into `.bss` — which is `NOLOAD`. The fix is one
`.section .text` directive; the rule is that every asm block in this crate opens
by naming its section.

### 3.10.3 What this says about proposal item 4

Item 4 wants `akuma-exec-core`'s `Context` — a `repr(C)` struct of twenty public
mutable AArch64 register fields — replaced by constructors and accessors. The
x86 `Context` here is that argument taken to its conclusion: it holds **one**
field, `rsp`, and is built only by `Context::for_task`. Everything else lives on
the task's own stack, pushed by the switch routine.

That is not an x86 trick — the same structure works on AArch64. It is the
strongest available evidence for item 4's instinct: once a context can only be
built by a constructor, the register set stops being part of the interface, and
the crate that owns fork and exec no longer needs to know what a callee-saved
register is. Item 4's `spsr` hazard (any of 19 call sites could write `EL1h` and
turn `enter_user_mode` into a privileged jump) simply cannot be expressed against
a type with no public fields.

## 3.11 Stage F: ring 3, and the bug only real hardware could find

```
  test: ring 3 entered, 2 syscalls, arg=0x0000000000001234 status=0x0000000000002468   [OK]
```

`amd64/src/gdt.rs` (GDT + TSS) and `amd64/src/usermode.rs` (`syscall`/`sysret`).
The kernel drops to ring 3, userspace makes a syscall, the kernel doubles the
argument, userspace gets the result back and passes it out as an exit status:
`0x2468 = 0x1234 * 2` proves the value made the whole round trip rather than the
call merely returning.

This is the first use of `Prot::USER_RX` and `Prot::USER_RW`, which had existed
since Stage B, been unit-checked, and never actually been mapped.

### 3.11.1 The bug: `#GP` on AMD, clean on QEMU

The ring-3 test passed under QEMU and faulted immediately on the Ryzen:

```
[EXCEPTION] #GP general protection err=0x0000000000000018
  rip=0x00000000002030e2  cs=0x08  rflags=0x10006
```

`rip` disassembled to the `iretq` in `idt::timer_interrupt` — so an interrupt had
been delivered *in ring 3* and the fault was on the way back out. The error code
named GDT entry 3 (user data), which was not enough to say why: the descriptor
looked correct.

Dumping the five words at the faulting `rsp` — for a fault *on* an `iretq` that
is exactly the frame being rejected — settled it in one run:

```
  [rsp]= rip=0x50000000  cs=0x23  rflags=0x202  rsp=0x50010ff0  ss=0x18
```

`CS = 0x23` carries RPL 3; **`SS = 0x18` carries RPL 0**. `iretq` requires
`SS.RPL == CS.RPL`, hence `#GP(0x18)`.

The cause is in `IA32_STAR`. `sysret` does not take selectors, it *computes*
them: `CS = STAR[63:48] + 16`, `SS = STAR[63:48] + 8`. The base was `0x10`, so SS
came out as `0x18` with RPL 0. The fix is the standard one — put the RPL in the
base, `STAR[63:48] = 0x10 | 3` — after which `CS = 0x23` and `SS = 0x1b`.

**Emulation hid it.** QEMU forces RPL 3 onto both computed selectors, so a base
of `0x10` works there and faults on real AMD hardware at the first interrupt
taken in ring 3. Nothing about the local run was wrong; the local machine simply
could not express the failure. This is the clearest argument in this document for
the Firecracker host existing at all — three stages of QEMU-green work, and the
first thing ring 3 did on real silicon was expose a selector bug.

Fixing it also validated more than the syscall path: the interrupt that triggered
the fault was a *real* LAPIC tick delivered in ring 3, so the TSS `rsp0` stack
switch and the interrupt-return path are exercised too, not just `syscall`.

### 3.11.2 The frame dump stays

`idt::fatal` now dumps the words at the faulting `rsp`. It is only meaningful for
a fault on an `iretq`, and for that case it is the difference between "the error
code says GDT[3]" and "the frame says `ss=0x18`". Cheap, bounds-checked against
the identity map, and it earned its place on its first run.

### 3.11.3 Two constraints the GDT layout carries silently

`sysret` computing selectors rather than taking them means user **data** must sit
immediately *below* user code (`0x18` then `0x20`), which is the reverse of the
intuitive order. `syscall` is the mirror — `CS = STAR[47:32]`, `SS = that + 8` —
forcing kernel code and data adjacent at `0x08`/`0x10`.

Neither constraint is checked at load time, by anything. A wrong order links
fine, boots fine, and faults on the first transition. Both are now written down
in `gdt.rs`'s header, which is the only enforcement available.

The kernel entries are byte-identical to the ones `boot.s` installed, deliberately:
`CS` stays valid across the `lgdt`, so no far return is needed and the only
reload is `ltr`.

## 3.12 Stage G: the Linux ABI, and proposal item 5 becomes load-bearing

```
  test: ring 3 — userspace output follows
    [ring3] hello from userspace via write(2)
  test: ring 3 97-byte program, 2 syscalls, wrote 46 bytes, exit_group(0)   [OK]
```

That middle line is written by **ring-3 code calling `write(2)`** — a real Linux
syscall with the real x86_64 number — and printed by the kernel's serial driver.
The program then calls `exit_group(0)` and the excursion returns.

### 3.12.1 Why item 5 stopped being cosmetic

The proposal filed item 5 as "real, small, least present-day payoff — defer this
one". The amd64 port changes that, because `akuma-syscalls-linux::nr` is
`asm-generic` numbering, which is aarch64's, and x86_64 Linux numbers everything
differently:

| | aarch64 (`asm-generic`) | x86_64 |
|---|---:|---:|
| `read` | 63 | **0** |
| `write` | 64 | **1** |
| `exit` | 93 | **60** |
| `exit_group` | 94 | **231** |
| `mmap` | 222 | **9** |
| `openat` | 56 | **257** |

The dangerous row is `read`. Number `0` is `read` on x86_64 and **`io_setup`**
under `asm-generic`, so a dispatcher fed the wrong table does not fail to find a
handler — it finds the *wrong* one. `akuma-syscalls-linux`'s own header names
that class as worse than a crash: "a wrong field offset or flag bit does not
crash, it corrupts". A wrong syscall number is the same shape.

### 3.12.2 `akuma-syscalls-abi`, and why it is not in `akuma-syscalls-linux`

The first attempt put the `Syscall` enum and the x86_64 table inside
`akuma-syscalls-linux`. That was wrong, and the crate's own description says why:
it is *"The Linux/aarch64 syscall ABI"*. A second architecture's numbering inside
it makes the name quietly false, and leaves a reader no way to tell which table a
bare `nr::WRITE` means.

`akuma-syscalls-abi` is the arch-plural concept one level up. It **reads**
`akuma-syscalls-linux::nr` for the asm-generic numbers rather than copying them,
so the two can never drift, and owns the x86_64 table that has no home below.
`akuma-syscalls-linux` is untouched, so none of the 192 `nr::` call sites in
`akuma-syscalls-glue` moved and the aarch64 kernel took no risk.

Three host tests, all of which run without a VM:

- **`round_trip_on_both_architectures`** — every variant has a number on both and
  decodes back. `to_x86_64`/`to_aarch64` return `u64`, not `Option<u64>`, so
  adding a variant without adding both numbers fails to *compile*.
- **`tables_disagree_where_linux_does`** — modelled on `akuma-firecracker`'s
  `no_address_is_hardcoded`, and for the same reason: the bug being guarded
  against is a second table copied from the first, which looks right until it is
  used. If this ever passes trivially, that bug is already in the tree.
- **`zero_means_different_things`** — pins the `read`/`io_setup` collision above.

### 3.12.3 The test program's numbers come from the table under test

`build_user_program` emits `Syscall::Write.to_x86_64()` rather than a literal `1`,
so the program and the kernel that decodes it cannot disagree. The program is
built rather than written as a byte literal, because the message address is an
operand: a hand-assembled blob with a hardcoded offset stays correct until
somebody edits the string by one character.

### 3.12.4 Known gaps in the syscall path

- **`sys_write` dereferences a user pointer directly.** That works only because
  `CR4.SMAP` is not enabled; with SMAP on it needs `stac`/`clac`. There is no
  `copy_from_user` validating the range against the page tables — the length is
  bounded and a bad pointer lands in the `#PF` handler, which is the honest limit
  of what it can promise.
- **The entry stub forwards four arguments**, not six. Linux passes `a4`-`a6` in
  `r10`, `r8`, `r9`; nothing needs them yet.
- **One address space.** `enter_user_mode` does not switch `CR3`, so ring 3 runs
  in the kernel's tables with the kernel pages simply not marked user-accessible.
  That is sound but it is not isolation, and it is what a real process needs next.

## 3.13 Stage H: per-process address spaces

```
    [ring3 A] first process, own address space
    [ring3 B] second process, same VA, different frame
  test: processes 0x1264000 vs 0x126a000 at the same VA, exits 0x0a/0x0b   [OK]
  test: address-space teardown frames 126366 -> 126366   [OK]
```

Two processes, each with its own PML4, both mapping `USER_CODE_VA` — and
resolving it to **different physical frames**. The kernel's own address space
maps that VA to nothing at all. That is isolation rather than merely "ring 3
cannot write kernel pages".

### 3.13.1 Sharing the kernel by aliasing an entry, not copying it

The kernel runs identity-mapped in the first 1 GiB — image, stacks, heap, PMM
pool and every page table live there — so an address space missing it faults on
the instruction *after* `mov cr3`, with no way to report it. A new space
therefore has to contain the kernel.

It shares rather than copies: the new PDPT's slot 0 points at the very same page
directory the boot map uses. One kernel mapping exists, so the copies cannot
drift. Slot 3 (`0xC000_0000`..`0x1_0000_0000`, containing the LAPIC at
`0xFEE0_0000`) is shared for the same reason — the timer can fire while a process
runs, and the handler writes EOI.

Everything else is private. `USER_CODE_VA` is `0x5000_0000`, which is PDPT slot 1,
so the two spaces differ exactly where they should.

`paging::activate` is `unsafe` and its doc states the whole obligation: the root
must map every page the kernel is executing from and will touch before switching
back. That is the one contract this stage rests on.

### 3.13.2 `free` is not `Drop`

Freeing an address space that is still in `CR3` unmaps the code doing the
freeing. A destructor that fires on falling out of scope makes that far too easy,
so teardown must be asked for by name. It frees the PML4, the PDPT and every
table under a *non-shared* slot — skipping the shared ones, or the first process
to exit would free the kernel's own page directory.

The frame-count check exists because that class of bug is silent: page tables are
frames, and leaking them looks exactly like working correctly.

### 3.13.3 The bug the test caught

First run: isolation passed, both processes printed correctly, teardown was
clean — and process B exited with **`0x37`** where `0x0b` was expected. `0x37` is
55, which is the length of B's message.

`LEAVE_RING3`, the flag the handler sets to end an excursion, was never cleared.
A's `exit_group` left it set, so B returned to the kernel immediately after its
*first* syscall — `write` — and `enter_user_mode` reported that syscall's return
value as the exit status. Both processes still ran and still printed, which is
why nothing else noticed.

The flag's lifetime is exactly one excursion, so it is now cleared at the top of
`enter_user_mode` rather than at the exit. Checking the *exit status* rather than
"did it return" is what surfaced it.

## 3.14 A shared self-test harness, and the leak it found immediately

```
Akuma/amd64 self-test: 39 passed, 0 failed
Akuma/amd64 — all self-tests passed
```

`crates/akuma-selftest` — no dependencies, no allocation, `forbid(unsafe_code)`,
output through a caller-supplied `fn(&str)` so it knows nothing about PL011 vs
16550 vs a host `print!`.

It exists because both sides of this tree had grown the same shape. `src/tests.rs`
and `src/process_tests.rs` are 36k lines of `fn test_*() -> bool` with ad-hoc
printing at each site, and `amd64/` had reached seven smoke tests each
hand-rolling `if ok { "[OK]" } else { "[FAIL]" }`. **Neither had a tally**, so a
failing check printed `[FAIL]` and the boot carried on and announced success.
`Suite::report` is `#[must_use]` for exactly that reason.

### 3.14.1 It found a leak on its first run

Converting `idt::smoke_test` turned a passing test into a failing one:

```
demand paging: no frame leak   [FAIL] got 0x1ed80 want 0x1ed82
```

The previous version printed `frames 126348 -> 126346` and scored itself `[OK]`.
The two-frame shortfall was **in the output and not in the condition** — the
`ok` flag covered faults, readback and unmap, and the frame count was decoration.

The frames are real and the behaviour is correct: they are the page directory and
page table allocated to describe the 2 GiB test region. `unmap_page` clears the
leaf and deliberately does not reclaim the tables above it, because doing so
safely needs a per-table live-entry count — another mapping may still sit in the
same table. So the test now pins the number at exactly 2, with the reasoning; if
table reclaim is ever implemented it becomes 0 and the test says so.

### 3.14.2 `check_eq` rather than `check`

A bool can only report that something was wrong. The Stage H bug (§3.13.3) was
diagnosed by its *value*: an exit status of `0x37` is 55, the length of the
message the process had just written, which named the cause immediately. So
`check_eq` prints both sides on failure and is the primary API, not a
convenience.

### 3.14.3 The harness's own bug, caught on the host

`NUM_BUF` was sized 18 — `0x` plus 16 hex digits. Decimal needs **20**:
`u64::MAX` is 18446744073709551615. `dec` panicked with "index out of bounds: the
len is 18 but the index is 18".

In a kernel that is a panic raised *by the diagnostic path*, which is the one
place a panic is least useful — the harness would have taken down the boot it was
reporting on. It was caught by a host unit test on the edge values, in a crate
that needs no VM to test. That is the argument for the extraction in miniature.

### 3.14.4 Not adopted in `src/` yet

`src/` keeps its existing style. Converting 36k lines of boot tests is a separate
change with its own risk, and the harness is useful to the amd64 target on its
own. The crate is deliberately arch-neutral and dependency-free so that
conversion can happen incrementally, a suite at a time, whenever it is worth it.

## 3.15 Stage I: multitasking

```
  -- userspace output follows --
    [ring3 A] round
    [ring3 B] round
    [ring3 A] round
    [ring3 B] round
    [ring3 A] round
    [ring3 B] round
  ring3: processes interleaved   [OK]
```

Two user processes, each in its own address space, taking turns. The scheduler
now installs a task's page-table root in `CR3` on every switch, so a context
switch changes *which memory exists*, not just which stack is live.

Cooperative, via a `sched_yield` syscall: the user program loops
`write` / `sched_yield`, and the switch happens on the kernel side of that
syscall. The tick still only sets a flag.

### 3.15.1 Counts cannot prove multitasking

Three writes from A followed by three from B satisfies every count-based check
and means the scheduler never switched. So the kernel records **which task**
performed each `write` and the test asserts on the *transitions*: 6 writes must
produce 5 changes of task. A run that batched would report 1.

### 3.15.2 Three globals had to become per-task, and only one was obvious

`syscall_entry` kept the saved user stack, the kernel stack to run on, and the
leave-ring-3 flag in three globals. Multitasking makes all three wrong, for three
different reasons:

- **Saved user stack** — a syscall by A would overwrite B's saved `rsp`.
- **Kernel stack** — this is the subtle one. Two processes sharing one syscall
  stack is harmless *until a context switch happens inside a syscall*, which is
  exactly what `sched_yield` does: the switch pushes A's callee-saved registers
  onto that stack and B then resumes and pops its own frame from the same place.
- **Leave flag** — A's `exit_group` would make B return early from whatever
  syscall it was in. This is the same bug as §3.13.3 in a new disguise: there it
  was one process leaking into the next excursion, here it is one process leaking
  into another *task*.

All three now live in a `UserCtx` the scheduler repoints on switch. Its field
offsets are load-bearing — `syscall_entry` indexes it as `[rax+0]`, `[rax+8]`,
`[rax+16]` — which is stated on the struct, since reordering the fields would
silently change what that assembly reads.

### 3.15.3 What stayed global on purpose

The TSS `rsp0` trap stack is still one shared region. That is safe *only* because
nothing switches tasks from inside an interrupt handler — the timer handler
counts, sets a flag and returns. The moment preemption switches from the handler,
`rsp0` has to become per-task too, and that is the next real constraint on
preemption rather than an oversight.

### 3.15.4 Task slots are not recycled

`spawn` looks for `State::Unused` and a finished task stays `Finished`, so its
stack is never handed to someone else. `MAX_TASKS` therefore bounds the tasks a
boot may *ever* create, not the tasks alive at once — it went 4 → 8 when the two
processes had nowhere to go alongside the three scheduler workers. Reuse needs
the stack freed first, which needs to know nothing still points into it.

## 3.16 Stage J: preemption

```
  -- userspace output follows (no yields) --
    [ring3 C] spinning, never yields
    [ring3 D] spinning, never yields
    [ring3 C] spinning, never yields
    ...
  preempt: timer interleaved two non-yielding processes   [OK]
  preempt: task switches observed between writes 5      # QEMU
  preempt: task switches observed between writes 4      # Firecracker, Zen 4
```

The two processes in this test contain **no `sched_yield`**. They write, then
spin 8 million iterations in ring 3. The only thing that can take them off the
CPU is the timer interrupt — which is the difference between Stage I's
cooperative scheduling and preemption, and why it needed its own test rather
than a stronger assertion on the existing one.

The switch happens inside `timer_interrupt`, on the interrupted task's own trap
stack. When that task is scheduled again it returns from `preempt_if_needed`, the
handler returns, and `iretq` resumes it — in ring 3 or ring 0, wherever it was.

### 3.16.1 The blocker named in §3.15.3, cleared

Per-task TSS `rsp0`. `rsp0` is where the CPU pushes the interrupt frame for a
ring-3 trap, and **a preempted task is suspended sitting on that frame** — so two
tasks sharing one `rsp0` would have the second's interrupt overwrite the first's
saved state, and the first would resume into a frame describing the second.
Every task now gets its own trap stack, installed by `gdt::set_kernel_stack` on
each switch.

### 3.16.2 Preemption broke the test that motivated it

The Stage E scheduler test hung. Its workers waited for `need_resched()` before
yielding — and `preempt_if_needed` now **consumes that flag inside the interrupt
handler**, so a worker polling for it in ring 0 could never observe it and spun
out its whole 200-million-iteration budget.

The check was measuring the wrong thing to begin with: it inferred "the tick
drives scheduling" from a flag that two parties race for. It now calls
`sched::preemptions()`, which counts switches made from the interrupt handler —
the thing itself rather than a proxy for it.

### 3.16.3 Why the assertion is `>= 1`

How often the timer lands inside a spin depends on the tick period against the
delay loop, and those differ by roughly 17x between QEMU and real silicon
(§3.9). QEMU produced 5 switches, the Ryzen 4. Asserting an exact count would be
asserting on the host's speed; the count is reported as a `note` instead.

The delay is sized for the slower-to-tick of the two — the Ryzen, at roughly 2.1M
spins per tick.

## 3.17 Stage K: the higher-half kernel

The kernel no longer lives in the lower half. The address space is now:

```text
  0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF   userspace   (PML4 0..255)
  0xFFFF_8000_0000_0000 + pa                       physmap     (PML4 256)
  0xFFFF_8080_0000_0000 + pa                       device MMIO (PML4 257)
  0xFFFF_FFFF_8000_0000 + pa                       kernel image(PML4 511)
```

The payoff is immediate and concrete: `USER_CODE_VA` moved from `0x5000_0000` —
a value picked to *dodge* the kernel's identity map — to **`0x40_0000`**, which
is where a static Linux x86_64 binary is linked. An ELF loader stops being
artificial the moment a program can be mapped where it expects to be.

An address space now shares three PML4 slots with the kernel instead of two PDPT
slots, so the entire lower half is the process's.

### 3.17.1 Two windows onto the same memory, on purpose

The physmap maps RAM with 2 MiB writeback pages. The LAPIC at `0xFEE0_0000` is
inside the first GiB, so it already *has* a cached alias there. Rather than split
that 2 MiB page, device registers get a second window mapped 4 KiB at a time with
`MemAttr::Device`. Two mappings of one physical page with different cacheability
is exactly what that type was added for in §3.9.1.

### 3.17.2 Three failures, each a different layer

The move took three debugging rounds, and none of them was a paging bug.

**1. Code model.** The link failed with `relocation R_X86_64_32S out of range`
pointing at ordinary statics. The default `small` code model cannot express a
2^47 displacement. `-C code-model=kernel` is built for the top -2 GiB, which is
why the kernel image is linked at `0xFFFF_FFFF_8000_0000` and the physmap is a
*separate* window — a kernel-model image cannot be linked into slot 256's range.

**2. The GDT was in the lower half.** After dropping the identity map, the kernel
triple-faulted inside the heap allocator. QEMU's `-d int` gave the shape:

```
check_exception old: 0xffffffff new 0xe    # a page fault
check_exception old: 0xe        new 0xe    # ...whose delivery also faulted
check_exception old: 0x8        new 0xe    # #DF, delivery faulted again
Triple fault
```

`CR2` pointed at `0x2010d8` — the GDT `boot.s` builds in the low boot region,
because the 32-bit trampoline must reach it with paging off. **The CPU reads the
GDT on every exception delivery**, loading CS from the IDT entry's selector, so
unmapping it makes every fault a triple fault. `gdt::init` rebuilds the table in
the kernel's high `.bss`; it now runs *before* `drop_identity_map` rather than
just before ring 3.

**3. An unplaced output section.** With the GDT fixed, the fault became
reportable, and said:

```
[EXCEPTION] #PF  rip=0xffffffff8021876d  cr2=0x0000000000217010
callq *0x7fffe89d(%rip)   # 0x217010 <__boot_stack_top>
```

An indirect call through a GOT slot — a `memset` inside the allocator. The
linker script never named `.got`, so lld placed it wherever it liked: **on top of
`__boot_stack_top`** in the low boot region. `relocation-model=static` removes
most GOT use but not all of it, so the section exists even though it is nearly
empty. Naming `.got`/`.plt` explicitly puts them in the high image.

The general lesson is the third one: a section a linker script does not mention
is still emitted, and where it lands is not the script author's decision.

### 3.17.3 The IDT moved earlier, and stayed there

Failure 2 was invisible because the IDT was installed *after* the memory
subsystem — so a fault during memory bring-up had no handler. `idt::init` needs
nothing but its own static table, so it now runs before `mem::init`. Failure 3
was diagnosed in a single run because of that change.

### 3.17.4 What still uses the lower half

The kernel's own test VAs (1 GiB for `paging::smoke_test`, 2 GiB for demand
paging) are in PML4 slot 0 of the *kernel's* space. That is not a conflict —
each process has its own slot 0 — but it does mean the kernel is still willing to
map low addresses for itself. A stricter split would refuse.

## 3.18 Stage L: an ELF loader, and the first real kernel bug

The kernel no longer runs programs it wrote itself. `amd64/src/loader.rs` parses
an ELF64 image, places its `PT_LOAD` segments into a process address space, lays
out a System V initial stack, and jumps to `e_entry`.

The blocker was Stage K, not the loader. Until the kernel left the lower half
there was nowhere to *put* a program linked where a static Linux binary is
linked, and a loader that can only place an image at an address chosen to dodge
the kernel is a loader for one program. `USER_CODE_VA` is `0x40_0000` because
that is where the program says it goes.

```
  elf: rejects a non-ELF image   [OK]
  elf: rejects ET_DYN   [OK]
  elf: rejects a non-x86-64 machine   [OK]
  elf: rejects ELF32   [OK]
  elf: rejects a truncated image   [OK]
  elf: rejected loads leak nothing   [OK]
  elf: image loaded   [OK]
  elf: every PT_LOAD was placed   [OK]
  ...
  -- userspace output follows (from an ELF image) --
    [elf] loaded from a real ELF image
  elf: program ran and reported every check   [OK]
  elf: teardown leaks nothing   [OK]
```

### 3.18.1 The bug it found

The guest program exited with `0x401000` — the address of its own `.rodata` —
where a six-bit mask was expected, because `syscall_entry` clobbered six
registers the Linux x86_64 syscall ABI preserves, and rustc had left the live
value in one of them.

It has its own document: **`docs/archive/AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md`**.

The part that belongs here is *why five stages missed it*. Every program before
this one was emitted by `usermode::build_user_program`, and its live state across
a syscall sat in `r12`/`r13` — callee-saved, therefore preserved by an
`extern "C"` handler for free. The kernel's test programs used exactly the
registers the kernel's bug did not touch, because the same author chose both.

That is the actual argument for building against a real toolchain, and it is
worth separating from the usual one. The point is not that a compiled program is
larger or more complex than a hand-assembled one. It is that **its choices are
uncorrelated with the kernel's**. Every stage before this validated the kernel
against its own assumptions.

### 3.18.2 Where the image comes from

`userspace/amd64/hello/hello.rs`, compiled by `amd64/build.rs` with the same
`rustc` that is building the kernel, `--target x86_64-unknown-none`, and
embedded with `include_bytes!`.

`rustc` directly rather than a nested `cargo build`: a build script that invokes
cargo shares the parent's target directory and package lock, which deadlocks, and
working around it means a separate target dir and a second copy of every
dependency. This program has no dependencies — one file against `core` — so the
thing cargo adds is exactly the thing that breaks.

`include_bytes!` rather than a file because this target has no disk driver, so
there is nowhere to put a binary it could open by path. That is the honest
constraint and it is temporary; nothing about the loader assumes it.

Two flag differences from the kernel's own build are load-bearing:

* `-C code-model=small`. The kernel's `code-model=kernel` comes from a
  `.cargo/config.toml` target entry, which does **not** reach a hand-rolled
  `rustc` invocation — and would be wrong here anyway, since this image is at
  `0x40_0000` and not in the top -2 GiB.
* `-C link-arg=--no-pie` as well as `relocation-model=static`. The model decides
  what relocations are emitted; the link flag decides what kind of object comes
  out. Without it the result is `ET_DYN`, which the loader refuses.

### 3.18.3 The link script is part of the design

`userspace/amd64/user.ld` puts `. = ALIGN(0x1000)` between every output section.
Without it lld packs `.text`, `.rodata` and `.data` into one page, and a single
page then has to carry the union of three permission sets — an executable,
writable page.

The loader refuses that mapping outright, which makes the alignment load-bearing
rather than cosmetic: a link that stops satisfying W^X fails the boot instead of
silently handing ring 3 a writable code page. `Prot` has never offered a
`USER_RWX` constructor; this is where that convention became enforcement.

Two things about the resulting image were not what the script implies, and both
are why the self-test derives its expectations from the file rather than from a
literal:

* lld emits **four** `PT_LOAD`s from three named sections, because `-z relro`
  splits `.data` from `.bss`. `count_pt_load` reads the phdr table at its
  architectural offsets and compares — a number derived independently of the
  parser under test.
* `.data` and `.bss` share the page at `0x402000`, so the "two segments in one
  page" path in `map_range` runs on every boot. Both are `PF_W`, so the union is
  a no-op and no remap happens — but the code is exercised rather than dead.

### 3.18.4 What the exit status is for

`hello.rs` checks seven properties of the load and reports them as bits of its
exit status rather than printing a verdict. A program that printed would have
"passed" by running at all; the status is compared against a value computed in
`usermode::elf_test`, so a wrong load fails the boot.

| bit | claim | what it would catch |
|---|---|---|
| 0 | `.data` holds its linked contents | segment mapped but file bytes not copied |
| 1 | `.bss` is zero across 32 KiB | `p_memsz > p_filesz` tail not zero-filled, or only the first page |
| 2 | `.data` is writable | `PF_W` never reached the PTE |
| 3 | `argc` is on the stack | no initial frame built |
| 4 | `argv[0]` points at its string | pointer written, string not |
| 5 | auxv carries `AT_PAGESZ` | vector absent or unterminated |
| 6 | a syscall preserved the ABI's registers | §3.18.1 |

Bit 1 is 32 KiB rather than one word on purpose: a zero-fill that clears the
segment's first page and stops would pass a smaller check.

`elf_test` names each bit individually when the mask comes back short, because
`got 0x2F want 0x7F` makes the reader decode a bitmask to learn that `argv[0]`
was wrong.

### 3.18.5 Rejection is tested before acceptance

Five malformed images — non-ELF magic, `ET_DYN`, `EM_AARCH64`, ELFCLASS32, and a
truncated file — must all be refused, and the free-frame count must be unchanged
afterwards. Each is a *mutation of the real image* rather than a hand-written
header, so a change to how `hello` is linked cannot leave these testing a shape
the loader no longer sees.

They check that a rejection happened, not which message came back. The messages
are diagnostics; pinning them would make rewording one a test failure.

`loader::load` frees nothing on the failure path — it records frames as it
allocates them and leaves them to the caller. A half-loaded image whose frames
the loader had reclaimed would leave the space's page tables pointing at memory
the PMM has since handed to someone else, which is the `AddressSpace::free`
split (tables are its own, leaves are the caller's) followed through.

### 3.18.6 Why not `akuma-elf`

The tree has an ELF loader and it is arch-neutral in everything that matters
here: `source.rs` parses through the vetted `elf` 0.7 crate and names no
architecture. It is unusable from this target for one structural reason —
`load.rs`, `interp.rs` and `stack.rs` are all written against
`akuma_mmu::UserAddressSpace`, and `akuma-mmu` is AArch64 page-table code. That
dependency is real rather than incidental, and the crate's own manifest says why:
a loader that cannot name an address space cannot place a segment.

So the extraction that would let the two share is a **parse/place split** — the
`ElfSource` + `parse_headers` half is neutral and currently `pub(super)`; the
mapping half is not. That is the shape a future crate should take, and it was not
this stage's work.

What this stage deliberately did *not* do is re-implement the parsing.
`loader.rs` calls the same `elf` 0.7 crate through the same
`parse_ident`/`parse_tail`/`SegmentTable` path, so the tree has one ELF parser
and two consumers — not two parsers, which is the defect
`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §3 spent a verification campaign over
2,387 binaries removing.

### 3.18.7 `MAX_TASKS` was sized against the tests that existed

`spawn` returned `None` and only the ELF test failed. Slots are never recycled —
a finished task keeps its two 32 KiB stacks — so the table has to hold every task
the *whole boot* creates, not every task alive at once. Three scheduler workers,
two cooperative processes, two preempted ones and one ELF process is eight, plus
the boot task: nine, against a `MAX_TASKS` of 8.

Raised to 12. The right fix is recycling, and it is deliberately not this: a slot
cannot be reused until its stacks can be, and reclaiming those needs the
scheduler to know no frame is still on them.

### 3.18.8 What the loader does not do

No demand paging — every page of every segment is allocated and copied up front.
The `#PF` handler can already service a not-present fault, so the machinery
exists; wiring segments to it needs a per-space region table, which is the next
thing rather than part of this one.

No `PT_INTERP`, no relocations, no `PT_GNU_RELRO` enforcement, and no `AT_PHDR` —
a program that walks its own program headers gets nothing useful from this auxv.
No `PROT_NONE` guard page below the stack, and no stack growth.

W^X is enforced at *map* time and never observed being enforced by the hardware:
a ring-3 write to a code page would be a `#PF`, and this kernel's `#PF` handler
is fatal for anything outside its armed demand-paging window. Proving the
refusal is a separate stage that needs faults to be deliverable to a process
rather than to the console.

## 4. What is deliberately missing

Corrected 2026-09-04. This list was written at Stage B and went stale as the
stages landed — it still claimed there was no upper-half mapping (Stage K did
it), no IDT (Stage C), and that the target had never run under Firecracker
(§3.6, on real hardware). A stale "missing" is worse than an edited record, so
what follows is the state as of Stage L rather than the original text.

- **No ACPI, and it is further away than expected.** PVH supplies the memory map
  outright, so nothing so far has needed it. It becomes necessary for IOAPIC (device
  interrupts), MADT (SMP) and MCFG (PCI) — not before. Even the preemption timer does
  not need it, since the LAPIC base comes from MSR `IA32_APIC_BASE`. When it is
  needed, it will have to be found the hard way: `hvm_start_info.rsdp_paddr` exists
  in the ABI but **both** QEMU and Firecracker v1.16.1 report it as 0 (measured,
  §3.6). Do not build on that field.
- **No devices at all.** No disk, no NIC, no virtio. The console is the 16550 at
  I/O port `0x3F8` and nothing else, which is why the ELF loader's image is
  `include_bytes!`d into the kernel rather than opened by path (§3.18.2). A VGA
  text path was considered and dropped: it would be dead code on Firecracker.
- **No demand paging for user segments.** The `#PF` handler services a
  not-present fault inside one armed region (Stage C); an ELF's segments are
  allocated and copied eagerly. Wiring the two together needs a per-space region
  table (§3.18.8).
- **No FP/SIMD state save on the syscall path.** `syscall_handler` touches no
  vector register today, which is a property of the current handler rather than
  a guarantee — the AArch64 side saves `q0`-`q31` for exactly this reason. Same
  class as the bug in `AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md` §8, still open.
- **No TSS and no IST.** A double fault runs on the faulting stack, which is
  fine while nothing can overflow it and wrong the moment a guard page exists.
  When one appears, vector 8 needs an IST entry *before* it.
- **Page tables exist but are not shared with `akuma-mmap`.** `amd64/src/paging.rs`
  can map, unmap, translate and now report a page's permissions, with a local `Prot`
  struct deliberately shaped the way proposal item 1 wants. It is local because
  `MmapRegion.flags` is still a raw AArch64 `u64` and the two encodings share no
  field — see that module's table. Item 1 remains the prerequisite for *sharing* the
  region bookkeeping.
- **`akuma-elf` is not shared either**, for a different and more specific reason:
  its mapping half is written against `akuma_mmu::UserAddressSpace`. The
  extraction that would fix that is a parse/place split (§3.18.6).
- **No SMP.** One vCPU. `invlpg` is core-local and there is no IPI shootdown, so
  the TLB maintenance is complete only because there is one TLB (proposal item 3).
- **No `CR4.SMAP`/`SMEP`.** `sys_write` dereferences a user pointer directly from
  ring 0; with SMAP on that needs `stac`/`clac`. A real gap, not a hypothetical.

## 5. Files

| Path | |
|---|---|
| `amd64/src/boot.s` | PVH note, 32-bit trampoline, long-mode entry |
| `amd64/src/serial.rs` | polled 16550 on port 0x3F8 |
| `amd64/src/main.rs` | `kmain`, panic handler, x86_64-only guard, the self-test order |
| `amd64/src/hvm.rs` | `hvm_start_info` and its memory map |
| `amd64/src/mem.rs` | heap then PMM, and why that order |
| `amd64/src/phys.rs` | the physmap / device / kernel windows |
| `amd64/src/paging.rs` | 4-level tables, `Prot`/`MemAttr`, `AddressSpace` |
| `amd64/src/gdt.rs`, `amd64/src/idt.rs` | descriptor tables, exceptions, demand paging |
| `amd64/src/lapic.rs`, `amd64/src/port.rs` | timer, I/O ports |
| `amd64/src/sched.rs` | context switch, round-robin, preemption |
| `amd64/src/usermode.rs` | ring 3, `syscall`/`sysret`, `Process`, the ring-3 tests |
| `amd64/src/loader.rs` | the ELF loader and the initial user stack (Stage L) |
| `userspace/amd64/user.ld` | guest link script: `ET_EXEC` at 0x40_0000, page-aligned segments |
| `userspace/amd64/hello/hello.rs` | the loader's probe; reports through its exit status |
| `amd64/linker.ld` | load at 2 MiB, explicit `PHDRS` incl. `PT_NOTE` |
| `amd64/build.rs` | the kernel link script, and building the guest programs |
| `amd64/run.sh`, `amd64/run-firecracker.sh`, `amd64/README.md` | |
| `.cargo/config.toml` | `[target.x86_64-unknown-none]` → `relocation-model=static`, `code-model=kernel` |
| `crates/akuma-cpu/src/lib.rs` | the gate, and a rewritten header |

`relocation-model=static` is a property of the *target*, not the package: a bare-metal
image links to a fixed load address and is not position independent, and
`x86_64-unknown-none` defaults to PIE, which makes rust-lld reject every absolute
reference in `boot.s` with `R_X86_64_32 cannot be used against local symbol`.

---

**Background:** the one kernel defect this port has found so far has its own
document: `docs/archive/AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md`.
`docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §0 carries the corrected
claim and the reproduction commands; `amd64/README.md` is the current-state doc for
this target. The aarch64 Firecracker port — a different machine with a different
device model — is `docs/archive/AKUMA_FIRECRACKER_KVM.md` and
`proposals/FIRECRACKER_PORT.md`. The instruction-chokepoint work this gate belongs to
is `docs/archive/INLINE_ASM_CLEANUP.md`.
