# `akuma-fdt`: one `unsafe` for the device tree, and two easy wins in `smp_shared`

**2026-09-01.** Started as an audit of `src/smp_shared.rs`'s `unsafe` blocks and
ended with a new crate, because five of the eight blocks in that file were
duplicates of code that already existed somewhere else in the tree.

## The audit

`src/smp_shared.rs` had **8 `unsafe` blocks** (plus an `unsafe extern "C"` item
block and one `#[unsafe(no_mangle)]` attribute, neither of which is a site):

| line | what | verdict |
|---|---|---|
| `psci_call` | `hvc #0` / `smc #0` | **keep** — genuinely unsafe |
| `resolve_dtb` | `read_volatile` of the DTB magic | duplicate of `main.rs`'s |
| `probe_dtb` | `Fdt::from_ptr` | duplicate of `main.rs`'s and `platform.rs`'s |
| `mmio_w32` | `str {v:w}, [{a}]` | verbatim copy of `gic_v3.rs`'s |
| `mmio_r32` | `ldr {v:w}, [{a}]` | verbatim copy of `gic_v3.rs`'s |
| `secondary_gic_init` | `ICC_SRE`/`PMR`/`BPR1`/`IGRPEN1` | same four registers, same order, same values as `gic_v3::init` steps 1 + 5 |
| `secondary_stack_base` | `adrp`/`add` symbol load | expressible in safe Rust |
| `set_shared_vbar` | `adrp` + `msr vbar_el1` | duplicate of `exceptions::init`'s |

Only **one of the eight** is a place where a human genuinely has to vouch for
something. `psci_call` starts another core executing at an address we hand it —
correctly excluded from `akuma-cpu`, whose admission test is "safe to execute".

This document covers the four that were fixed. The three GIC ones are a separate
change (see "Left undone").

## 1. `secondary_stack_base` — asm to safe code

It resolved a linker symbol and added an offset:

```rust
let addr: usize;
// SAFETY: resolves the `.bss.smp_shared` symbol's address; no memory access.
unsafe {
    core::arch::asm!(
        "adrp {t}, secondary_boot_stacks_shared",
        "add {t}, {t}, :lo12:secondary_boot_stacks_shared",
        t = out(reg) addr, options(nomem, nostack),
    );
}
addr + (core << STACK_SHIFT)
```

Taking the address of an `extern` static is **safe in edition 2024** — `&raw
const` resolves the symbol without reading through it — and this tree is
edition 2024 throughout. So:

```rust
unsafe extern "C" {
    static secondary_boot_stacks_shared: [u8; MAX_CORES << STACK_SHIFT];
}

pub fn secondary_stack_base(core: usize) -> usize {
    (&raw const secondary_boot_stacks_shared) as usize + (core << STACK_SHIFT)
}
```

Not just shorter: the declared array type states the bound that the asm version
could only assert in a comment, and it is the same bound the trampoline's `add
x0, x0, x20, lsl #STACK_SHIFT` walks.

## 2. `set_shared_vbar` — one vector-table install, not two

`exceptions::init` already did `&raw const exception_vector_table` + `msr
vbar_el1` + `isb`; `smp_shared` had written its own `adrp`/`add`/`msr` sequence
for the same table. Extracted as `exceptions::install_vbar()`, called by both.

The `msr vbar_el1` stays `unsafe` — installing a vector table is a change of
control flow, which is why `akuma-cpu` excludes it by name. The win is not the
count. Cores in this build share **one kernel image**, so "the BSP's vector table
and the secondaries' vector table" is one object; having two pieces of code
install it is two places for it to drift.

## 3. `akuma-fdt` — six `unsafe` operations become one

Three consumers each materialised the DTB for themselves:

```
src/main.rs:289,291   read_volatile magic + totalsize   (scan_for_dtb)
src/main.rs:324       Fdt::from_ptr                     (detect_memory)
src/platform.rs:287   Fdt::from_ptr                     (install_fdt_device_map, an `unsafe fn`)
src/smp_shared.rs:567 read_volatile magic               (resolve_dtb)
src/smp_shared.rs:587 Fdt::from_ptr                     (probe_dtb)
```

Six operations, one obligation: *this address holds a complete FDT*. It is true
once per boot, not once per consumer.

The fact that makes it collapse: **`fdt::Fdt::new(&[u8])` is safe.** Only
`from_ptr` is not, and only because it has to dereference to discover the blob's
length. `akuma-firecracker` had already been trading on this — `describe_ptr`
became `describe_fdt` on 2026-08-30 and the crate took `forbid(unsafe_code)`,
with the pointer work pushed out to `platform::install_fdt_device_map`. That
relocation put the `unsafe` in *a caller*. This one gives it *a home*.

```rust
// crates/akuma-fdt
pub const QEMU_VIRT_DTB_PA: usize = 0x4020_0000;

pub struct Dtb<'a> { base: usize, blob: &'a [u8] }

pub fn header_totalsize(head: &[u8; 8]) -> Option<u32>;   // pure, host-tested
pub unsafe fn locate<'a>(pa: usize) -> Option<Dtb<'a>>;   // the only unsafe fn
impl<'a> Dtb<'a> {
    pub fn from_slice(blob: &'a [u8]) -> Option<Self>;    // safe, host-testable
    pub fn parse(&self) -> Option<Fdt<'a>>;               // safe
}
```

`kernel_main` calls `locate` once, in a block, and passes `Option<&Fdt>` to all
three consumers. `platform::install_fdt_device_map` stopped being an `unsafe fn`.

### Why `Dtb<'a>` and not `&'static [u8]`

The blob is **not** valid for the kernel's lifetime — which is the entire reason
`smp_shared::probe_dtb` exists, snapshotting CPU topology into statics because on
large-RAM configs the heap can be placed on top of the DTB. A `'static` blob
would be a lie the type system would then propagate.

`locate` returns `Dtb<'a>` for a caller-chosen `'a`. In `kernel_main` that binds
to a block that closes before heap init, and the borrow checker keeps every
derived `Fdt` inside it. **"Read the DTB before heap init" was a comment on
`probe_dtb`; it is now a lifetime.**

### Two things the duplication was hiding

**The five sites disagreed about validation.** `main.rs`'s scan checked the magic
*and* bounded `totalsize` to `64..=16 MiB`; `smp_shared::resolve_dtb` checked the
magic only; and both, along with `platform.rs`, did **no** validation at all when
the bootloader supplied a non-zero pointer. Meanwhile `Fdt::from_ptr` reads a
40-byte header, `unwrap()`s the parse, and builds a slice of whatever `totalsize`
it finds with no bound at all — a wild pointer that happens to carry the magic
yields a multi-gigabyte slice.

`locate` reads **8 bytes**, one byte at a time (so it carries no alignment
obligation, which a `read_volatile::<u32>` would), checks the magic before
trusting anything, and bounds the size. Every path now gets the strictest of the
three.

**A reachable panic in a `panic = "abort"` kernel.** `Fdt::new` validates the
magic and that the buffer is at least `totalsize`, and nothing else — a blob with
a good header and a zeroed body parses "successfully". This was found by a unit
test written to assert the opposite, which failed. It matters because two of the
`fdt` crate's accessors panic rather than return an option:

```rust
pub fn memory(&self) -> Memory { self.find_node("/memory").expect("requires memory node") }
pub fn cpus(&self)   -> ...    { self.find_node("/cpus").expect("/cpus is a required node") }
```

`main::detect_memory` calls the first, `smp_shared::probe_dtb` the second, and
`akuma_firecracker::describe_fdt` the first again. A tree missing either node was
a dead kernel at boot. Reachable only via the scan path — where a stale blob may
sit at `QEMU_VIRT_DTB_PA` and four bytes of magic are the whole filter — so not
likely, but not costly to rule out either: `Dtb::parse` checks both nodes, and a
tree that fails gets the fallbacks its consumers already had ("using default
256MB", "staying single-core", "keeping bootstrap device map").

Requiring both nodes couples two questions that are in principle separate. That
is deliberate and documented on `parse`: no machine here has one without the
other, and the alternative is three scattered guards that a fourth consumer would
forget. Note `.chosen()` — which has the same `expect` — is called nowhere in the
tree; if that changes, it belongs in the same check.

### Tests

11 host tests, including the two that matter in opposite directions:

- `rejects_byte_swapped_magic` — the replaced sites all spelled the magic as a
  pre-swapped little-endian constant compared against a native `u32` load, which
  is correct only on a little-endian CPU and states nothing about the format.
  `header_totalsize` reads big-endian bytes, and this pins it.
- `accepts_every_real_tree` — the Firecracker `.dtb` fixtures from
  `docs/reference/firecracker/fdt/` and the QEMU virt ones from
  `crates/akuma-firecracker/fixtures/`, asserting both that `parse` accepts them
  and that `memory()`/`cpus()` return something. A validator only ever tested
  against garbage is a validator that can be too strict.

## Measurements

`python3 scripts/cloc_akuma.py src crates`, against `--rev HEAD` for the
baseline:

| scope | before | after | |
|---|---:|---:|---|
| `src/` production `unsafe` sites | 113 | **104** | −9 |
| `crates/` production `unsafe` sites | 321 | **324** | +3 (all `akuma-fdt`) |
| tree production | 434 | **428** | **−6** |
| `src/smp_shared.rs` `unsafe` blocks | 8 | **4** | −4 |
| crates | 34 | 35 | `akuma-fdt`, which cannot `forbid` |

The tree total *falling* is the unusual part — the extraction programme's normal
shape is a relocation, `src/` down and `crates/` up by the same amount. Here five
of the nine removed sites were duplicates of each other, so they did not need a
home to move to.

## Verification

- `cargo test --target aarch64-apple-darwin` — full host suite, 0 failures.
- `cargo clippy -p akuma-fdt --target $HOST -- -D warnings` — clean.
- `cargo clippy --release -- -D warnings` — clean.
- `cargo clippy --profile extreme-size --no-default-features --features
  no-tests,smoltcp,extreme,userspace-sshd -- -D warnings` — clean. (This is the
  path with `smp-shared` **off**, so `probe_dtb` is compiled out and the other two
  consumers still have to build.)
- `INSTANCE=3 SMP=4 MEMORY=2048 cargo run --release` — **314 PASSED, 0 FAILED**:

```
[Platform] qemu-virt device map installed
[DTB] found at 0x48000000 (1048576 bytes)
[Platform] FDT device map: GICR=0x80a0000
[Memory] Detected from DTB: base=0x40000000, size=2048 MB
[SMP-shared] probed 4 core(s)
[SMP-shared] core 1 online (idle tid 1)
[SMP-shared] core 2 online (idle tid 2)
[SMP-shared] core 3 online (idle tid 3)
[SMP-shared] ✓ 3 secondary core(s) online (shared kernel)
```

All three secondaries coming up is the check on items 1 and 2 — every one of them
runs on a stack from the new `secondary_stack_base` and takes its first interrupt
through the shared `install_vbar`. `2048 MB` is the check on `detect_memory`, and
`GICR=0x80a0000` on `install_fdt_device_map`.

Note QEMU passed a real pointer here (`0x48000000`, and the blob is padded to
1 MiB — worth knowing against `MAX_TOTALSIZE`), so this run exercised the
`pa != 0` path. The scan path (`locate(0)` -> `QEMU_VIRT_DTB_PA`) is for ELF
kernels and was not exercised; its logic is the same function and its validation
is host-tested, but it has not been booted.

## Left undone

`src/smp_shared.rs` still has 4 `unsafe` blocks. One is `psci_call` and stays.
The other three are the GIC ones, and they want a single change:

- Make `gic_v3::{mmio_w32, mmio_r32}` `pub(crate)` — `smp_shared` has copies, and
  its comment already points at `gic_v3` for the ISV/HVF rationale (writeback and
  pair forms set `ESR.ISV=0`, and QEMU's HVF backend asserts on a data abort it
  cannot decode).
- Add `gic_v3::cpu_interface_init()` for the `ICC_SRE`→`isb`→`PMR`→`BPR1`→
  `IGRPEN1` sequence, called by `gic_v3::init` and `secondary_gic_init`.

The redistributor half must stay in `smp_shared` — it indexes `gicr_base() + idx
* GICR_STRIDE` per core, where `gic_v3` uses the fixed `DEV_GICR_*_VA` window for
core 0 — but with shared accessors it becomes ordinary safe code.

Held back deliberately: it touches the boot path on real hardware, and it needs
one question settled first. `gic_v3` is `#[cfg(not(feature = "gic-v2"))]` and
`smp_shared` has no such gate. The combination is already broken today
(secondaries would poke a GICv3 redistributor that a GICv2 machine does not
have), so the dependency adds no constraint — it converts a silent
misconfiguration into a link error, which is an improvement — but that should be
confirmed against `build.rs` rather than assumed.

## Background

- [`INLINE_ASM_CLEANUP.md`](INLINE_ASM_CLEANUP.md) — `akuma-cpu`, and the
  exclusion list that keeps `msr vbar_el1` and the PSCI conduit outside it.
- [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) — the same "move the
  operation to the crate that owns what it pokes" argument, applied to
  `src/syscall/`.
- [`AKUMA_NET_SPLIT.md`](AKUMA_NET_SPLIT.md) §6.5 — the previous case of a test
  rewrite surfacing a real defect that the old formulation hid.
- [`crate-safety.md`](../reference/crate-safety.md) — the counts, and why
  `akuma-fdt` is in the "not enforceable" table on purpose.
- [`smp-shared.md`](../reference/subsystems/smp-shared.md) — current-state design
  of the shared-kernel SMP path.
