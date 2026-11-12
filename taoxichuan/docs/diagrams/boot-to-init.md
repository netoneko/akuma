# Boot to init

Embedded inline in `../reference/boot.md`. Keep the two in the same commit.

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
    J --> K["exec_twin::init — register ExecRuntime / ThreadRuntime /<br/>ProcessHooks / ArchHooks (akuma-exec, akuma-threading)"]
    K --> K2["install_surface — akuma-runtime-tokio::install, uname,<br/>vfs-glue mount table"]
    K2 --> L["map_virtio — virtio-mmio window (disk and NIC)"]
    L --> M["root: .root(make) if given, else .disk() — ext2 on device 0<br/>(akuma-ext2, BlockDevice = Vda); mounted at / and as ROOT_FS"]
    M --> N["net::net_init if .net() — smoltcp on virtio-net<br/>(akuma-net over akuma-net-nic), DHCP wait ≤10 s;<br/>else loopback only. smp=N APs if .smp()"]
    N --> O["banner (src/banner.txt, verbatim)<br/>epoch= cmdline token → UTC offset store (akuma-primitives::clock)"]
    O --> P["parse_init: init=/initargs=/initenv=<br/>(one program, fixed slots)"]
    P --> Q["run_init: read init off ext2, start it as pid 1 on a pool thread"]
    Q --> R["spawn → load_task → ring3::enter_user<br/>ring 3, IF set"]
```
