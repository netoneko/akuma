# The 1 GiB guest-RAM cap, lifted to 4 GiB

**Status: FIXED 2026-10-30** (host-tested; the metal build and boot are the supervisor's — see
the session's `RESULT-X11.md`). The cap was never a setting: it was the nested page table's
shape, and it was enforced in four places that had to move together.

## Symptom

Any VM spec with more than 1024 MiB of RAM was refused before a single guest instruction ran:

- `akuma_vm_spec::spec::MAX_RAM_MIB = 1024` — `ram_mib=1025` failed the spec parse
  (`Error::OutOfRange`), so herd's `vm_ram_mib` hit the same wall one layer up.
- The `/dev/kvm0` Linux loader's NPT was one directory page of 512 2 MiB leaves — exactly
  1 GiB of guest-physical space. More RAM had no mapping to live in.
- `crates/akuma-kvm-abi::slots::GPA_LIMIT = 1 << 30` — `SET_USER_MEMORY_REGION` refused any
  slot at or past 1 GiB (`Refused::Geometry`), bounding the `/dev/kvm` API path the same way.

The Taoxichuan self-hosting fixed point needs 3 GiB (the `-j8` cargo wall, gaps.md G8: a
3 GiB guest OOMs and an 8 GiB one dies differently) and 4 GiB is the target, so the cap had
to go, not be tuned.

## Cause

The first GiB of guest-physical space was the whole world: one NPT directory of 2 MiB leaves
covers 1 GiB, so everything downstream — the spec ceiling, the herd docs row ("16-1024"), the
ABI's `GPA_LIMIT`, the "Guest RAM <= 1 GiB" line in `kvm.md` — echoed that one page's size.
Raising the number without rebuilding the page table would have corrupted memory at the first
2 MiB leaf past index 511.

## Fix (and the decision it encodes)

Guest RAM up to `MAX_RAM_MIB = 4096` MiB is laid out by `akuma_svm_amd64::rammap` (new, pure,
`forbid(unsafe_code)`, host-tested):

- **Flat to 3 GiB**, exactly as before: guest-physical `g` is host frame `base + g`.
- **Past 3 GiB, the PC shape**: the 3..4 GiB band is the *device window* — the virtio-mmio
  pages (`0xD000_0000` disk, `0xD000_1000` NIC) and the local APIC page (`0xFEE0_0000`) live
  there — and RAM cannot map over them. So RAM past 3 GiB moves **above 4 GiB**
  (`[4 GiB, 4 GiB + bytes - 3 GiB)`) and the band stays unmapped, the way a real PC splits
  its RAM at the PCI hole. A 4096 MiB guest is 3 GiB below the window and 1 GiB above it.
  Chosen over the alternatives because: a flat 4 GiB map is impossible without moving the
  devices (every guest command line, ACPI table and exit handler names them where they are),
  and splitting at 3 GiB keeps every existing ≤ 3 GiB guest bit-for-bit on its old layout.
- **The NPT grows by directories, not by depth**: PML4 → PDPT → one directory page per GiB
  that holds RAM. The window's GiB gets no directory at all, so a 4 GiB guest allocates four
  directories (plus the ever-present PML4 and PDPT) — 16 KiB more than the old single page.
  They come from the same `FreshFrame` source the one page came from, are held in the same
  `Npt` struct and dropped with it; nothing new is allocated on the fault path.
- **The guest is told**: the PVH memory map reports both extents (RAM, type 1) with the
  window as the gap, and the handoff pages (cmdline/memmap/start_info, and the ACPI page for
  Linux guests) sit under the top of the *upper* extent, excluded from the reported RAM.
  `pvh::plan` takes the second extent; `add_acpi` appends after however many RAM entries the
  map already names.
- **The API path mirrors it**: `slots::GPA_LIMIT` is 5 GiB (4 GiB of RAM around the window),
  `slots::check` refuses a memslot in or across the window (ending exactly at its base is
  allowed), and the api NPT fills one directory per GiB a slot covers.

`MAX_RAM_MIB` stayed the single source of the bound: the spec tests parse the ceiling and
refuse ceiling+1 by building the values from it, and herd's test feeds `MAX_RAM_MIB + 1`
where the literal `4096 -> OutOfRange` case used to sit.

## Verification

- `akuma-svm-amd64` host tests: the 4096 MiB NPT, walked with the crate's own MMU-walk, maps
  each of its 2048 leaves exactly once to the right host frame and maps nothing in 3..4 GiB;
  the 4096 MiB PVH map sums to the RAM less the handoff pages and names the hole; `plan`
  refuses 0, non-2-MiB-multiples and past-ceiling sizes; `fill_slot_directory` refuses
  window-crossing slots.
- `akuma-vm-spec`: ceiling parses, ceiling+1 and 0 refused. `akuma-kvm-abi`: slots above the
  window accepted, in/across refused. `herd`: the moved refusal case.
- `cargo check --release --target x86_64-unknown-none -p akuma-amd64 --features no-tests`
  clean; `check_unsafe_budget.py` unchanged (494 sites).

## Still open

- A guest past 3 GiB has a two-extent physical layout; nothing is lost (the excess sits above
  the window whole), but a guest that assumed one contiguous RAM extent without reading the
  PVH map would see only the first 3 GiB. Linux and the Akuma kernel both take the map.
- The reserve (`kvmmem=`) still has to hold the whole RAM block contiguously: a 4 GiB guest
  needs `kvmmem=4096` (entry 30 already carries it) plus room for RAM-disk copies.
- SMP, `taoxichuan/`, boot flows and the epoll gap are untouched.
