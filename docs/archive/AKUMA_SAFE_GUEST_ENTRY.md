# Entering a guest is safe (2026-11-12)

## Before

`akuma-svm-switch-amd64` held every SVM instruction, but two of its functions stayed `unsafe fn`:
`run` (a bare `VMRUN` that passed RCX/RDX/RBX) and `run_with_regs` (`VMLOAD` + `VMRUN` + `VMSAVE`
around a full register file). Their contract was that the VMCB confines the guest to memory it
owns. Nothing in the types said so:

- guest RAM was a physical range carved from the `kvmmem=` reserve and tracked by an arena offset;
- the nested page table was a set of frames the caller filled by hand;
- the permission maps were `.bss` statics whose base the caller wrote into the VMCB;
- ASIDs were constants (`1`, `2`, `14`, `15`, `ASID + i`, `id + 1`).

`akuma-kvm-amd64` had seven `unsafe` call sites, each with its own SAFETY paragraph. Reading them
against the code found four ways the contract did not hold. None was exploited, and the guests that
existed did not trigger them.

1. **The rehearsal guests mapped 2 MiB of host memory around each 4 KiB frame.** The trivial,
   long-mode, ladder and PVH guests gave the code page a 2 MiB NPT leaf over the aligned window
   that holds the frame. The other 511 pages in that window belong to whoever the page allocator
   gave them to. The guests are fixed programs that touch only their own page, so nothing went
   wrong, but the table granted more than the SAFETY comment claimed.
2. **The rehearsal permission maps passed nearly everything through.** The MSRPM intercepted only
   reads of `EFER`. The IOPM intercepted only port `0x80` and the CMOS range. `run` does no
   `VMLOAD`/`VMSAVE`, so a native `wrmsr LSTAR`, `swapgs` or `mov gs` in such a guest would have
   written the host's live registers.
3. **ASIDs were not unique among live guests.** Every Linux machine used `ASID + i`, and every
   `/dev/kvm` VM used `id + 1`. Two machines running at once (the VM services do this) shared TLB
   tags, so on a shared core one could use the other's translations. The first-entry flush did
   not help, because it ran once per run rather than once per core per address space.
4. **The `/dev/kvm` table could outlive nothing, but nothing stopped it.** `run_vcpu` took the
   table's root address under the VM lock and entered the guest after dropping it. Only the
   convention that `destroy_vm` runs once the owner is gone kept the table's frames alive.

## Change

The switch crate now owns everything that decides what a guest can reach, and it enters a guest
only through `Guest::run`. That function is safe.

- **`Asid`.** A global bitmap capped at `CPUID Fn8000_000A_EBX`. `alloc`/`claim` take a number and
  `Drop` gives it back, so no two live values hold the same ASID.
- **`GuestRam`.** Contiguous, zeroed frames, made only by `GuestRam::alloc` (the page allocator)
  or `reserve::carve` (the `kvmmem=` reserve, which moved here from `ctl.rs`). `Drop` returns them
  to their source. `reserve::init` refuses a second reserve, which would orphan every slice of
  the first.
- **`Npt`.** Owns its table frames, the `GuestRam`s it adopted and the pages it adopted. Leaves
  are written only by `map_ram` (bounds-checked against adopted RAM) and `adopt_page`. It is
  append-only, so no translation outlives its memory before the whole table drops. Guests hold
  it through an `Arc`.
- **`Guest`.** A VMCB frame, an `Asid` and `Option<Arc<Npt>>`. On every entry, after `CLGI`, it
  rewrites the confinement fields from those values:
  - `NP_ENABLE` is set to exactly nested paging (no SEV, SEV-ES or PML bits), and `N_CR3` to its
    table;
  - the I/O and MSR permission maps are the crate's own all-intercept statics;
  - `IOIO_PROT`, `MSR_PROT`, `SHUTDOWN`, `VMRUN`, `VMLOAD`, `VMSAVE`, `STGI`, `CLGI`, `SKINIT` and
    `XSETBV` are intercepted;
  - virtual `VMLOAD`/`VMSAVE` and AVIC are off, and `V_INTR_MASKING` is on;
  - `GUEST_ASID` is its own;
  - the clean bits are zero;
  - the first entry on each core (tracked in a 256-core bitmask, reset by `set_npt`) flushes its
    ASID.
- **Host state.** `run` `VMSAVE`s the host into the `HostState` page on every entry, with GIF
  already clear. A stale save, or one taken on another core, can no longer be restored.
- **One entry primitive.** The bare `run` is gone: without `VMLOAD`/`VMSAVE` a guest shares the
  host's live FS, GS, TR, LDTR and syscall MSRs, so it cannot be made safe. The rehearsal guests
  enter through `Guest::run` with a `GuestRegs`.

The wiring changes:

- `akuma-kvm-amd64` is `#![forbid(unsafe_code)]`.
- The rehearsal guests map each page with a 4 KiB leaf.
- The Linux machine's `Ram` is a view that holds the `Arc<Npt>` owning its `GuestRam`.
- The `/dev/kvm` VM keeps each memslot's `GuestRam` until the first `RUN`, when the table adopts
  them.
- The disk's RAM copy is a `GuestRam` held by `DiskLease`.

`unsafe` sites went from 7 to 0 in `akuma-kvm-amd64` and from 6 to 4 in `akuma-svm-switch-amd64`:
`enable`'s MSR writes, `VMSAVE`, `CLGI`, and the `enter` block.

## Behaviour that changed

- Rehearsal guests now run with every port and MSR intercepted, `SHUTDOWN` intercepted and
  `V_INTR_MASKING` set. Each executes only `rdmsr EFER` and `out 0x80`, both already intercepted,
  and runs with host `IF` clear, so its exits are unchanged.
- A `/dev/kvm` vCPU flushes its ASID on its first entry per core, not on the first entry of each
  `KVM_RUN`.
- `kvmmem=` given twice is refused (`[KVM] kvmmem: already reserved`).

## Verified

- `cargo clippy -p akuma --target x86_64-unknown-none --release -- -D warnings`, with the default
  features and with `headless`: clean.
- `scripts/check_unsafe_budget.py`: `akuma-kvm-amd64` 7 -> 0, `akuma-svm-switch-amd64` 6 -> 4.
- `scripts/kvm/akuma_rehearse.sh` on QEMU TCG (`-cpu EPYC-Milan,svm=on`, a `no-tests` build):
  - trivial, long-mode, two-level and PVH guests: PASS, with the same exit counts as before (the
    script passes `noladder`, so the ladder did not run here);
  - the Akuma guest: PASS (`banner=yes, init=yes`), and the guest-written disk checks clean with
    `e2fsck`.
- The amd64 boot suite with the default features (which link the hypervisor), SMP=2: 836 `[OK]`,
  0 `[FAIL]`.
- The first rehearsal found one bug in the conversion. The ASID was written only at entry, so the
  host-side `validate_guest_state` saw ASID 0 and refused every guest. `Guest` now writes the
  confinement fields when it is made and when its table is set.
- Still to run: the consistency ladder, nested SVM on Pop, and the ryzen metal with the `/dev/kvm`
  API path (Taoxichuan) and the VM services.

## Still open

- `copy_to_phys` is a safe function over any physical address, so the host-side copies into
  guest RAM (`Ram::write`, the virtio models) are bounds-checked by `rammap` but not by type.
  Making the device models borrow `&GuestRam` would close that. It is host memory safety, not
  guest confinement.
- The `/dev/kvm` path remaps the VMM's pages onto a reserve slice (`remap_user_to_phys`). The
  slice is given back when the VM is destroyed, which happens only once the owner is gone. That
  ordering is still a convention of `api.rs`, not a type.
- Guest FPU and XCR0: the `/dev/kvm` path swaps FXSAVE state around each entry; the Linux machine
  does not. XCR0 is intercepted and never loaded for the guest.

Background: [`AKUMA_KVM_AKUMA_GUEST.md`](AKUMA_KVM_AKUMA_GUEST.md),
[`AKUMA_MULTIARCH_ATTEMPT_0.md`](AKUMA_MULTIARCH_ATTEMPT_0.md).
