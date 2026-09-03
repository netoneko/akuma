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
`proposals/REDUCING_PLATFORM_DEPENDENCY.md` under *"what is already right, and must
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

## 4. What is deliberately missing

- **No upper-half mapping.** The aarch64 `linker.ld` splits kernel VA from physical at
  `0xFFFF000040000000`; amd64 runs on the identity map. Absent rather than half-done.
- **No ACPI, and it is further away than expected.** PVH supplies the memory map
  outright, so nothing so far has needed it. It becomes necessary for IOAPIC (device
  interrupts), MADT (SMP) and MCFG (PCI) — not before. Even a preemption timer does
  not need it, since the LAPIC base comes from MSR `IA32_APIC_BASE`. When it is
  needed, it will have to be found the hard way: `hvm_start_info.rsdp_paddr` exists
  in the ABI but **both** QEMU and Firecracker v1.16.1 report it as 0 (measured,
  §3.6). Do not build on that field.
- **Page tables exist but are not shared with `akuma-mmap`.** `amd64/src/paging.rs`
  can map, unmap and translate, with a local `Prot` struct deliberately shaped the way
  proposal item 1 wants. It is local because `MmapRegion.flags` is still a raw AArch64
  `u64` and the two encodings share no field — see that module's table. Item 1 remains
  the prerequisite for *sharing* the region bookkeeping; it was not needed to get
  paging working.
- **No IDT.** Nothing faults, demand-pages or reaches userspace until exception entry
  exists — new arch code, and not one of the six items.
- **No VGA console.** Considered and dropped: the target is Firecracker, whose console
  is the 16550 at I/O port `0x3F8`, and a VGA text path would be dead code on it.
- **Not run under Firecracker yet.** Firecracker needs KVM on an x86_64 host; the dev
  machine is Apple Silicon, so QEMU is the only local stand-in. The entry path is
  shared, the device model is not.

## 5. Files

| Path | |
|---|---|
| `amd64/src/boot.s` | PVH note, 32-bit trampoline, long-mode entry |
| `amd64/src/serial.rs` | polled 16550 on port 0x3F8 |
| `amd64/src/main.rs` | `kmain`, panic handler, x86_64-only guard |
| `amd64/linker.ld` | load at 2 MiB, explicit `PHDRS` incl. `PT_NOTE` |
| `amd64/build.rs` | passes the linker script as `rustc-link-arg-bins` |
| `amd64/run.sh`, `amd64/README.md` | |
| `.cargo/config.toml` | `[target.x86_64-unknown-none]` → `relocation-model=static` |
| `crates/akuma-cpu/src/lib.rs` | the gate, and a rewritten header |

`relocation-model=static` is a property of the *target*, not the package: a bare-metal
image links to a fixed load address and is not position independent, and
`x86_64-unknown-none` defaults to PIE, which makes rust-lld reject every absolute
reference in `boot.s` with `R_X86_64_32 cannot be used against local symbol`.

---

**Background:** `proposals/REDUCING_PLATFORM_DEPENDENCY.md` §0 carries the corrected
claim and the reproduction commands; `amd64/README.md` is the current-state doc for
this target. The aarch64 Firecracker port — a different machine with a different
device model — is `docs/archive/AKUMA_FIRECRACKER_KVM.md` and
`proposals/FIRECRACKER_PORT.md`. The instruction-chokepoint work this gate belongs to
is `docs/archive/INLINE_ASM_CLEANUP.md`.
