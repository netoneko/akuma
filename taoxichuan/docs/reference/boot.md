# Boot — PVH entry to init spawn

**Stability: C** (active risk — the kernel is mid-ladder; verify against the
console of a current boot).

The whole boot lives in `akuma-tokio-kernel-amd64`'s `boot` (`src/lib.rs`), which `taoxichuan/src/main.rs`
reaches through `akuma-tokio-kernel`'s `entry!` (the `kmain` symbol) and its `Kernel` chain
(`.console().disk().net().smp().run_init()`); it sits on top of the boot crate
[`crates/akuma-boot-amd64`](../../../crates/akuma-boot-amd64) which owns the
PVH trampoline (`boot.s`), the physmap, the kernel page tables and the
heap + PMM bring-up. The linker script is the crate's `linker.ld` (handed to the binary through `links` metadata): low trampoline at the
load address (2 MiB, VMA == LMA for `.boot`/`.bootbss`), everything else in
the top −2 GiB (`KERNEL_VMA = 0xFFFFFFFF80000000`), the physmap a separate
window at PML4 slot 256. The same layout `akuma-amd64` uses; the `.got`/`.plt`
sections are placed **explicitly** because an unnamed output section once
landed on top of the boot stack (the story is the comment in `linker.ld`).

## Boot order, and why

Mirrors `akuma-amd64`'s `boot::early_init` order, which is load-bearing and
paid for: console → GDT → IDT → `CR0.WP` → machine description → heap + PMM →
LAPIC → runtime twin → the tokio surface → root filesystem (`.disk()` or `.root(..)`) → NIC (`.net()`) →
application processors (`.smp()`) → banner → init spawn. A part the root does not wire is absent.

```mermaid
flowchart TD
    A["QEMU/kvm0 loads ELF at 2 MiB<br/>(PVH note, hvm_start_info at 0x6000)"] --> B["boot.s trampoline (akuma-boot-amd64)<br/>32-bit protected → long mode<br/>boot page tables, physmap, boot stack"]
    B --> C["kmain (entry!) → Kernel::run_init → boot (lib.rs)"]
    C --> D["console::init if .console() — COM1 (akuma-comconsole)<br/>+ safe_print! hook (akuma-primitives); else silent"]
    D --> E["gdt_init — GDT+TSS (akuma-gdt)<br/>before the IDT: exception delivery reads the GDT"]
    E --> F["idt_init — every vector reports and halts<br/>(akuma-idt-amd64 + akuma-trap-amd64)<br/>so a wrong kernel is reportable, not a triple fault"]
    F --> G["CR0.WP set — kernel writes honour RO pages"]
    G --> H["akuma-ryzen-amd64::describe<br/>hvm_start_info, memmap, virtio-mmio slots"]
    H --> I["mem::init — heap (akuma-alloc) + PMM (akuma-pmm)<br/>no-op reaper hooks"]
    I --> J["lapic_init — mask PICs (akuma-port), enable xAPIC,<br/>calibrate vs PIT, arm periodic timer (akuma-lapic-amd64)"]
    J --> K["exec_twin::init — register ExecRuntime / ThreadRuntime /<br/>ProcessHooks / X86ArchHooks (akuma-exec, akuma-threading)"]
    K --> K2["install_surface — akuma-runtime-tokio::install, uname,<br/>vfs-glue mount table"]
    K2 --> L["map_virtio — virtio-mmio window (disk and NIC)"]
    L --> M["root: .root(make) if given, else .disk() — ext2 on device 0<br/>(akuma-ext2, BlockDevice = Vda); mounted at / and as ROOT_FS"]
    M --> N["net::net_init if .net() — smoltcp on virtio-net<br/>(akuma-net over akuma-net-nic), DHCP wait ≤10 s;<br/>else loopback only. smp=N APs if .smp()"]
    N --> O["banner (src/banner.txt, verbatim)<br/>epoch= cmdline token → UTC offset store (akuma-primitives::clock)"]
    O --> P["parse_init: init=/initargs=/initenv=<br/>(one program, fixed slots)"]
    P --> Q["run_init: read init off ext2, start it as pid 1 on a pool thread"]
    Q --> R["spawn → load_task → ring3::enter_user<br/>ring 3, IF set"]
```

Canonical mermaid source: [`../diagrams/boot-to-init.md`](../diagrams/boot-to-init.md).

## The identity map is NOT dropped, deliberately

That retirement exists to hand the lower half to userspace; keeping it keeps
`boot.s`'s low GDT reachable, so fault delivery has one fewer way to be wrong.
Each task's own root page table has no identity map (the lower half is the
task's); the kernel's root keeps it. See [`memory.md`](memory.md).

## Traps: every vector is a way to report, not a way to vanish

`idt_init` registers `fault_trap` for every `Kind` in
[`crates/akuma-trap-amd64`](../../../crates/akuma-trap-amd64), fills the rest
of the table ([`crates/akuma-idt-amd64`](../../../crates/akuma-idt-amd64)) with
plain reporter stubs, and points vector 32 (`TIMER_VECTOR`) at the trap crate's
entry for `Kind::Timer` → `timer_trap` → `akuma_lapic_amd64::on_tick()`.

- Before either: a ring-3 fault on a not-present page below the stack grows it; a write to a
  present copy-on-write page, from either ring, is resolved
  ([`memory.md`](memory.md) § fork's memory); a ring-0 fault inside
  `akuma-user-access`'s copy loop resumes at its `EFAULT` trampoline.
- A fault **from ring 3** is the task's death: `[Fault] ring3 … task killed`,
  and the process ends with wait status SIGSEGV while the machine runs on. The
  boot task, which has no slot to reap through, halts.
- Any other fault prints `[Fault] <kind> rip=… rsp=… cr2=… code=…` and halts.
  The `[Fault]` token is the gate's vocabulary; a correct boot has zero.
- A tick that interrupts ring 3 (every kernel path runs with interrupts masked)
  may switch tasks — see [`scheduling.md`](scheduling.md).

The multiboot2 entry (`kmain_mb2`, also from `entry!`) exists because `boot.s` names it
unconditionally; it prints "multiboot2 boot -- PVH/VMM only" and halts.

## The command line

Parsed per-token straight off `hvm_start_info` (never a full copy unless a
stage greps it). Tokens this kernel reads:

| token | consumer |
|---|---|
| `init=` / `initargs=` | first program; args are comma-separated |
| `epoch=` | wall-clock anchor: unix seconds → `akuma_primitives::clock::set_utc_time_us`, anchored at the uptime sampled at banner time. Written new (guest-cmdline policy): the metal's anchors are RTC/NTP mechanisms this guest cannot reach |
| `initenv=` | init's environment, comma-separated |
| `smp=N` | application processors to start, with `.smp()`; absent or 1 is one core |
| `mmdbg=N` | the mmap-slab bisect mask (default 7) |

## Background

`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` (the brief),
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (stage 1 gate),
`docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md` (the LAPIC/timer slice).
