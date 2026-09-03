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

## 4. What is deliberately missing

- **No upper-half mapping.** The aarch64 `linker.ld` splits kernel VA from physical at
  `0xFFFF000040000000`; amd64 runs on the identity map. Absent rather than half-done.
- **No `hvm_start_info` parsing.** The pointer is printed, not read. Its memory map is
  where the amd64 equivalent of the proposal's §2 `PlatformInfo` comes from — x86_64
  Firecracker passes **no DTB**, so `akuma-fdt` and `akuma-firecracker` (both
  DTB-driven, both aarch64) have nothing to say on this machine.
- **No use of the 34 crates that now compile.** See §1.3 before wiring the first one up.
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
