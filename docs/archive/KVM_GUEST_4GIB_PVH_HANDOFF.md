# A 4 GiB guest could not boot, then saw 3 GiB: two PVH handoff bugs

**Status: FIXED 2026-11-02**, verified on the ryzen (Akuma bare metal, entry 29, `kvmmem=4096`). The 4 GiB
guest layout landed on 2026-10-30 ([`KVM_GUEST_RAM_CAP_LIFTED.md`](KVM_GUEST_RAM_CAP_LIFTED.md)). Its host
tests passed, but no guest had ever been booted past 3 GiB. The first one, a Taoxichuan guest for
[`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md), hit two bugs, one after the other.

## Symptom 1: the guest halts at its first instruction that reads the handoff

`ram_mib=4096` in the spec:

```
[KVM] linux: running (start=3ffff000, ram=4096 MiB, vcpus=4, cores=4,5,6,7)
Taoxichuan/amd64 -- long mode reached
  hvm_start_info @ 0x000000003ffff000
  [FATAL] no hvm_start_info magic at the PVH handoff address
[KVM] linux: stopped: guest halted with interrupts off
```

## Cause 1: the handoff pages sat above 4 GiB, and EBX is 32 bits

`pvh::plan` packs cmdline, memmap and `hvm_start_info` directly under `top_gpa`. The 10-30 change passed it
the top of the **upper** extent (`rammap::Map::top()`, 5 GiB for a 4 GiB guest), so `start_info` landed at
`0x1_3fff_f000`. The PVH entry ABI hands the guest that address in **EBX**, a 32-bit register (the guest is
entered in 32-bit protected mode). The guest therefore read `0x3fff_f000`, where nothing was written. The
`running (start=...)` line prints 8 hex digits, so it showed the same truncated value.

## Symptom 2: with the pages moved, the guest boots but sees 3 GiB

```
[KVM] linux: running (start=bffff000, ram=4096 MiB, ...)
  machine: 3071 MiB RAM, 1 memmap entries, 2 virtio-mmio slot(s)
```

## Cause 2: `start_info.memmap_entries` was a literal 1

`write_memmap` wrote both RAM entries (low and high extent) into the memmap page and recorded the count
only as a scratch word in that page. `write_start` always wrote `memmap_entries = 1`. The count was corrected
only by `add_acpi`, which appends an ACPI entry and sets `n + 1`. The Akuma kernel finds its RSDP by scanning
the BIOS window (`acpi_by_scan`), so `add_acpi` is never called for it, and the guest read one entry: the
low 3 GiB. A Linux guest with a disk gets `add_acpi` and would have seen the right count, which is how the
host tests (which always called `add_acpi`) missed it.

## Fix

- `amd64/src/kvm/linux/machine.rs`: the handoff pages, and Linux's ACPI page, sit under the top of the
  **low** extent whatever the RAM size (`top = m.low.bytes`). The low RAM entry stops short of them, and
  the high extent is reported whole.
- `amd64/src/kvm/linux.rs`: the kernel loader's segment limit is the low extent less the handoff pages
  in both shapes. The `Ram::bytes` field, which only the old limit read, is gone (`map.bytes` has it).
- `crates/akuma-svm-amd64` (`pvh`): `write_start` sets `memmap_entries` from the layout (`ram_entries`:
  2 when the RAM crossed the device window, else 1). `plan`'s doc states the 32-bit constraint.

## Verification

- Host: `akuma-svm-amd64` 165 unit + 14 doc tests. The 4096 MiB memory-map test now asserts every handoff
  page ends at or below 4 GiB, and that `start_info` names both RAM entries **before** any `add_acpi`.
  `cargo check --release -p akuma-amd64` clean with `-D dead-code` (the box's build denies it; a plain check
  missed the dead field).
- Metal: the fix built on the ryzen (`kbuild`, on the box's `fca1f9ed` + this patch, md5 `01ae56af...`), installed
  as p3's `/boot/akuma-amd64.verify` (the previous one kept as `.verify.pre-pvh-ebx`), entry 29. The same
  Taoxichuan guest at `ram_mib=4096`:

  ```
  [KVM] linux: running (start=bffff000, ram=4096 MiB, vcpus=4, cores=4,5,6,7)
    machine: 4095 MiB RAM, 2 memmap entries, 2 virtio-mmio slot(s)
    pmm:  arena 0x0000000000100000 + 5119 MiB spanning 2 RAM region(s)
    pmm:  4094 MiB RAM, 915987 free frames (3578 MiB)
  ```

  `free -m` in the guest: 4095 total, 3575 free. It then ran GLM's infinity build.

## Still open

- More than 4 GiB: `rammap::MAX_BYTES`/`MAX_DIRS` and `akuma-vm-spec::MAX_RAM_MIB` cap a guest at 4 GiB, and
  entry 29's `kvmmem=4096` is in `grubx64.efi`, which only Pop can rewrite.
- The `/dev/kvm` API path (`amd64/src/kvm/api.rs`) leaves handoff placement to the userspace VMM, which must
  keep `start_info` below 4 GiB too.
