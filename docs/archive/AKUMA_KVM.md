# KVM on akuma: an SVM hypervisor developed from inside akuma

**Date:** 2026-10-20. **Machine:** the Ryzen 7 8845HS box ("ryzen", Zen 4,
`AuthenticAMD`, family 25 model 117 stepping 2), bare metal. **Running
kernel:** `27924603-release-smp-shared`, branch `more-amd64-fixes` — this
session is literally executing on it (`/bin/busybox uname -a` says
`Akuma localhost 0.0.8 ... x86_64 Linux`), which is the whole point.

[`POST_AMD64_CODE_AUDIT.md`](POST_AMD64_CODE_AUDIT.md) §3 asked "what would it
take to run Firecracker *on top of* akuma". The user reframed it as a
**dogfooding experiment**: the hypervisor is *developed from inside akuma, on
akuma metal*, by an agent that runs on that same metal — the same loop the
self-host work proved (`AKUMA_AMD64_BARE_METAL_SELFHOST.md`), pointed at a new
subsystem. The audit's VMX+EPT wording predates the ryzen-first decision, so
the ISA here is **AMD SVM (AMD-V) + nested page tables**, not Intel VT-x.

**Success ladder, set by the user:**

1. KVM runs something simple (a trivial guest exits and returns).
2. KVM runs Linux.
3. KVM runs akuma. **Done 2026-10-22, on QEMU TCG and on the ryzen metal** ([`AKUMA_KVM_AKUMA_GUEST.md`](AKUMA_KVM_AKUMA_GUEST.md)).

**License constraint (binding for the whole project):** no Linux code may be
used or consulted — KVM included — because of GPL incompatibility. Everything
below is built from the **publicly available AMD specification only** (APM
24593 rev 3.45). Where the text records a value from memory that the spec then
corrected, that is documented, because from-memory hypervisor constants are
exactly how this project would ship a kernel that `#GP`s ring 0.

---

## Timeline

Read downwards; the sections below are the reference record.

```
      KVM ON AKUMA — 2026-10-20 → · SVM + NPT, developed on the metal it runs

 [10-20] ═══ M1: THE SPEC, EXTRACTED FROM INSIDE AKUMA ═══  "no code yet"
   │
   ▼   APM Vol 2 (24593) rev 3.45 through a pure-stdlib PDF extractor:
   │   Identity-H subset CID fonts decode per-font /ToUnicode CMaps (§1).
   │   VMCB control + save-area layouts, three intercept dwords, the
   │   §15.5.1 consistency checks, Appendix C exit codes, the enabling
   │   sequence — every constant in the crate cites its table (§2).
   │   The extraction caught three from-memory errors on the way.
   ▼
 [10-20] ═══ THE SEED: akuma-svm-amd64 + kvmtest ═══  "installed — reboot pending"
   │
   ▼   13 host tests pin Table B-1/B-2 offsets, exit codes, segment
   │   attributes, the §15.5.1 validator, NPT encodings. APM Vol 3
   │   rev 3.38 §E.4.9: the Fn8000_000A.EDX bits (NP=0, NRIPS=3).
   │   amd64/src/kvm.rs: CPUID probe + trivial guest behind kvmtest
   │   (two vmmcalls, hlt, identity NPT, three unsafe sites).
   ▼
 [10-21] ═══ PHASE 0: A GUEST RUNS ON THE METAL ═══  "trivial guest: PASS"
   │
   ▼   Rehearsed in QEMU/KVM on ryzen (nested SVM, grub entry 27);
   │   three guest bugs, each visible only from the exit code:
   │     1. F3 0F 01 D9 is VMGEXIT; VMMCALL is 0F 01 D9 (3 bytes)
   │     2. the VMCB segment limit is byte-scaled: 0xFFFFF is 1 MiB
   │        and the code sits at ~17 MiB → intercepted #GP (exit 4Dh)
   │     3. NPT entries need U=1 → NPF 400h, info1=1_0000_0015
   │   Metal then reset ~1 min after PASS: crumb() read 9 — the guest
   │   had finished — so the cause was not the guest: #VMEXIT leaves
   │   GIF clear, the host got no interrupts, the timer went silent
   │   and the wdt fired. Fix: stgi in the same asm! as vmrun.
   │   After: PASS and the box stays up.
   │                        docs: AMD64_KVMTEST_METAL_DMESG_2026_10_21.txt,
   │                              AMD64_KVM_REBOOT_NOTE_2026_10_20.md
   ▼
 [10-21] ═══ THE PERMISSION MAPS + THE SHARED GPRS ═══  "rdmsr + out 0x80"
   │
   ▼   MSRPM (§15.11: two contiguous pages, 2 bits/MSR, 1=intercept) and
   │   IOPM (§15.10.1: 12 contiguous KiB, 1 bit/port) as .bss statics —
   │   physical contiguity is what the linker guarantees of an image
   │   object and two frames do not; PA = VA − KERNEL_VMA. Two bits
   │   armed at compile time: EFER read, port 0x80 (§2.7).
   ▼   The guest grows `rdmsr` + `out 0x80, al`; the VMM emulates RDMSR
   │   from the save area's EFER and feeds EDX to the next VMRUN as an
   │   asm input (RDX is shared, not saved). `vmrun` declares the shared
   │   GPRs clobbered per Table B-2 — rbx/rbp cannot be inline-asm
   │   operands in Rust (LLVM-reserved / frame pointer); the seed guest
   │   never writes them, phase 2 gets a real register file.
   ▼   SVM on the APs: the BSP reserves all 16 host save areas up front
   │   and leaks them (no allocation during AP bring-up); every AP
   │   enables through `kvm::ap_hook` in `ap_entry64`.
 [10-21] ═══ PHASE 3: LONG-MODE GUEST, REACHED THROUGH THE LADDER ═══ "PAE=1<<6"
   │
   ▼   The long-mode guest met VMEXIT_INVALID from both QEMU/KVM and the
   │   metal. A 12-probe consistency ladder (one VMRUN per ingredient set,
   │   unique ASID per row) bisected it; cr4=0x40 was the tell: `cr4::PAE`
   │   was `1<<6`, which is MCE — PAE is `1<<5`, and §15.5.1 refuses
   │   LME & PG & !PAE. Fixed (`bbaa81d1`) and every CR0/CR4/EFER bit
   │   constant pinned numerically: the validator *reasons through* the
   │   constants, so the symbolic tests stayed green at the wrong value.
   ▼   The long-mode guest then PASSED on the metal (kernel b6b8c8d2…,
   │   installed, rollback /boot/akuma-amd64.rung2): trivial + long-mode
   │   PASS, all 12 ladder rows entered, row-for-row identical to the
   │   QEMU rehearsal. Metal Fn8000_000A reads asids=32768 (16 under QEMU).
   ▼
 [10-21] ═══ PHASE 4: TWO-LEVEL TRANSLATION ═══  "written; rehearsal pending"
   │
   ▼   First guest with non-identity translation at BOTH levels: the guest
   │   walks its own long-mode page tables (gVA 0x5000_0000/0x5000_1000 →
   │   gPAs in the 0x4000_0000 band), and NPT maps those gPAs to the real
   │   hPAs. A PMM frame is not 2 MiB aligned, so each leaf's gPA is
   │   REGION | (hpa & 0x1F_FFFF) — computed at runtime, which translates
   │   back to exactly the frame; the banner prints gva= gpa= hpa=. Two
   │   vmmcalls — one after loading the exec magic, one after reading the
   │   data page through its gVA — prove fetch and data both survive both
   │   translations. The NPT leaf bit table (Vol 2 §5.3 Figs 519–529) and
   │   the NPF error-code bits (§15.25.6 p. 573) are transcribed and
   │   pinned for exactly this phase's failure mode (ASID 14; ladder 1–13).
   ▼   (metal PASS 2026-10-22; the Linux phase follows)
 [10-22] ═══ PHASES 5-6: PVH HANDOFF, THEN A REAL LINUX ═══  "banner on the metal"
   │
   ▼   Phase 5 (crush) enters a synthetic guest the PVH way; phase 6 loads the
   │   Firecracker CI vmlinux and runs it with CPUID/MSR/port-I/O emulated —
   │   `Linux version 6.1.128` arrives on the emulated COM1 and the guest runs
   │   on to its first timer wait. Found on the way: the PVH note type (9, is
   │   18), and a start_info layout the phase-5 guest agreed with and Linux did
   │   not. Metal 2026-10-22: PASS (entry 28,
   │   banner on real Zen 4). [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md)
 [10-22] ═══ PHASE 7: A TIMER — THE GUEST'S CLOCK TICKS ═══  "TCG breakthrough"
   │
   ▼   Pure, host-tested 8254 + 8259 models; interrupts reach the guest as
   │   virtual INTRs (V_IRQ | V_IGN_TPR, §15.21.4 — the hardware already waits
   │   for IF and the interrupt shadow, so no window logic). HLT now idles to
   │   the next PIT edge instead of ending the run. The PIT cannot calibrate
   │   the TSC (every poll is a VM exit; the gaps vary 5-10x), so the command
   │   line says `tsc_early_khz=`. Linux goes from spinning in its first delay
   │   loop (1.3 M exits) to the root-fs panic in 96 K, 144 interrupts taken.
 [10-22] ═══ PHASE 8: A DISK — LINUX BOOTS FROM A VIRTIO-BLK ROOT ═══  "TCG: init runs"
   │
   ▼   Four more pure models: a MOV decoder (nested page faults name the
   │   address and nothing else; 33 clang-assembled vectors), a virtio-mmio v2
   │   block device, ACPI tables (RSDP→XSDT→FADT/MADT→DSDT with an LNRO0005
   │   node — the only way this kernel finds a virtio device) and the 8250's
   │   THRE interrupt. Two traps found from the guest's own log: FADT
   │   HW_REDUCED_ACPI silently deletes the PIC (`nr_irqs: 8`), and a 16450
   │   stalls after one byte without a transmit interrupt. Backing store is
   │   the existing `akuma_virtio::block` driver, second device.
   ▼   QEMU TCG: ext2 root mounts, busybox init runs, the guest's md5 of a
   │   1.1 MB file equals the host's, a 256 KiB file the guest wrote reads
   │   back identical under host debugfs.
 [10-22] ═══ PHASE 8 ON THE METAL: LINUX BOOTS ON REAL ZEN 4 ═══  "marvels of engineering"
   │
   ▼   Nested SVM first, then entry 28 on the ryzen: the guest prints
       `model name : AMD Ryzen 7 8845HS`, detects the real 3.79 GHz TSC,
       mounts a file on p3 as its virtio-blk root, runs init, matches both
       md5s and reboots itself. 99 869 exits, 207 virtual interrupts.
       [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md) § Phase 7, 8
 [10-22] ═══ PHASE 9: AKUMA ON AKUMA ═══  "rung 3: PASS ON THE METAL"
   │
   ▼   The kernel's own ELF as the guest: a pure local-APIC model (register page,
       periodic/one-shot timer, IRR/ISR priority), delivered as the same virtual
       INTR; and the MMIO decoder learns CMP/TEST (LLVM folds a volatile load into
       the comparison) with flag arithmetic checked against 1 600 real x86 cmp/test
       results. Busybox init runs in the guest, its written file reads back
       identical on the host. Metal (ryzen entry 28): PASS, TSC 3794 MHz measured by
       the guest, 9 853 exits, written file verified from Pop with debugfs/e2fsck.
       [`AKUMA_KVM_AKUMA_GUEST.md`](AKUMA_KVM_AKUMA_GUEST.md)
```

---

## 1. Methodology: extracting the spec from inside akuma

The devbox has no `pip`, no `pdftotext`, and Python 3.14 stdlib only. The AMD
APM Vol 2 (24593) rev 3.45 PDF (11.4 MB, July 2026) came from
`https://docs.amd.com/api/khub/documents/LNBl5evreogi0r25VIFF2g/content`. A
pure-stdlib Python extractor was written in-session (indirect-object scan,
page tree walk, content-stream `Tj`/`TJ` reassembly) — and then produced
**zero** searchable text for every hypervisor keyword.

The reason is worth recording because it cost the session real time: the PDF
sets its body text in **Identity-H subset CID fonts**, so each glyph run is a
hex string of CIDs, not ASCII. The fix is to decode each run through the
font's **`/ToUnicode` CMap**. A first attempt merged all fonts' CMaps and
produced shifted, colliding text (glyph id N means different characters in
different subset fonts); the working version resolves each page's
`/Resources /Font` objects and decodes **per font**. The extracted text loses
the spaces between `TJ` runs, so keyword search means searching
`VMRUN`-adjacent tokens like `PlatformExtensions` without word boundaries.

Every constant in the tree that cites "APM Table B-1" or "§15.5.1" traces to
that extraction. The dumps live in `/tmp` (volatile); the durable form is the
`akuma-svm-amd64` crate's tests, which pin each offset/bit to its table
citation, and this document.

Still pending from the spec (see §4): the NPT leaf PTE bit table; the save-area
`LASTEXCPTO` offset (presumed `290h`, unverified).

## 2. What the spec says (APM Vol 2, 24593 rev 3.45)

### 2.1 Enabling SVM (§15.4, §15.30.4)

- Feature probe: `CPUID Fn8000_0001.ECX[2]` (SVM).
- `VM_CR` MSR `C001_0114`, bit 4 `SVMDIS` must be clear. If set, SVM may be
  firmware-locked: check `Fn8000_000A.EDX[SVML]` for the unlock key feature.
- `EFER` (`C000_0080`) bit 12 = `SVME` enables VMRUN. Precedent in tree:
  `amd64/src/usermode.rs:2922` already does a read-modify-write of EFER for
  `SCE`.
- `VM_HSAVE_PA` MSR (`C001_0117`) must hold a 4 KiB-aligned physical address
  of the **host save area** page; low 12 bits zero; must not be the VMCB page
  itself.
- `VMRUN` requires CPL0, protected mode, `EFER.SVME=1` (else `#UD`/`#GP`);
  RAX holds the VMCB physical address; VMCB mapped WB is recommended.
  `VMRUN`/`VMLOAD`/`VMSAVE` are CPL0-only instructions.

### 2.2 VMCB control area (Table B-1)

Offsets from the VMCB page base; the control area is padded to 1024 bytes;
**all unused bytes must be zero**, so VMCB pages are zero-filled on alloc
(`FreshFrame::alloc_zeroed` gives this for free).

| Offset | Content |
|---|---|
| `000h` | CR intercepts (15:0 reads CR0–15, 31:16 writes) |
| `004h` | DR intercepts (same split) |
| `008h` | exception intercepts (bit = vector, 0–31) |
| `00Ch` | instr intercepts dword0: 0 INTR, 1 NMI, 2 SMI, 3 INIT, 4 VINTR, 5 CR0-writes-other-than-TS/MP, 6–9 IDTR/GDTR/LDTR/TR reads, 10–13 their writes, 14 RDTSC, 15 RDPMC, 16 PUSHF, 17 POPF, 18 CPUID, 19 RSM, 20 IRET, 21 INTn, 22 INVD, 23 PAUSE, 24 HLT, 25 INVLPG, 26 INVLPGA, 27 IOIO_PROT, 28 MSR_PROT, 29 task switch, 30 FERR_FREEZE, 31 shutdown |
| `010h` | instr intercepts dword1: 0 VMRUN, 1 VMMCALL, 2 VMLOAD, 3 VMSAVE, 4 STGI, 5 CLGI, 6 SKINIT, 7 RDTSCP, 8 ICEBP, 9 WBINVD, 10 MONITOR, 11/12 MWAIT, 13 XSETBV, 14 RDPRU, 15 EFER-write-trap, 16 CR0-write-trap, 19 CR3-write-trap, 20 CR4-write-trap |
| `014h` | instr intercepts dword2: 0 INVLPGB-all, 1 INVLPGB-illegal, 2 INVPCID, 3 MCOMMIT, 4 TLBSYNC, 5 bus-lock-threshold, 6 HLT-if-no-virtual-interrupt |
| `03Ch` | pause filter threshold (15:0); `03Eh` pause filter count (15:0) |
| `040h` | IOPM_BASE_PA (bits 11:0 ignored) |
| `048h` | MSRPM_BASE_PA (bits 11:0 ignored) |
| `050h` | TSC_OFFSET |
| `058h` | 31:0 guest ASID; 39:32 TLB_CONTROL (0 = do nothing, 1 = flush entire TLB, 3 = flush this guest, 7 = flush non-global); bit 40 ALLOW_LARGER_RAP; bit 41 CLEAR_RAP |
| `060h` | int control: 7:0 V_TPR, 8 V_IRQ, 9 VGIF, 11 V_NMI, 12 V_NMI_MASK, 19:16 V_INTR_PRIO, 20 V_IGN_TPR, 24 V_INTR_MASKING, 25 VGIF_EN, 26 V_NMI_ENABLE, 30 x2AVIC, 31 AVIC, 39:32 V_INTR_VECTOR |
| `068h` | bit 0 INTERRUPT_SHADOW |
| `070h` | EXITCODE (u64); `078h` EXITINFO1; `080h` EXITINFO2; `088h` EXITINTINFO |
| `090h` | 0 NP_ENABLE, 1 SEV, 2 SEV-ES, 3 GMET, 4 SSSCheckEn, 6 RO-GPT, 7 INVLPGB/TLBSYNC-enable, 11 PML |
| `098h` | AVIC APIC_BAR; `0A0h` GHCB; `0A8h` EVENTINJ; `0B0h` N_CR3 |
| `0B8h` | LBR-virtualization bits; `0C0h` VMCB clean bits (31:0) |
| `0C8h` | nRIP (saved when NRIPS feature; next sequential RIP for trap-style intercepts) |
| `0D0h` | guest instruction bytes (7:0 count, 127:8 bytes) |
| `150h` | REQUESTED_IRR (255:0); `170h` EXITINTDATA; `178h` EVENTINJDATA |
| `1C8h` | PML_BASE; `1D0h` PML_INDEX |
| `3E0h`–`3FFh` | Reserved for Host usage |

From-memory errors the spec extraction caught: EFER in the save area is at
`0D0h` (not `080h`); there are **three** intercept dwords (`00Ch`/`010h`/
`014h`); IOPM_BASE is at `040h` (not `014h`). This is why every constant in
the crate cites its table.

### 2.3 State save area (Table B-2)

Begins at VMCB+`400h` (when SEV-ES is off); **offsets below are relative to
the save-area start**, i.e. VMCB+`400h`+offset.

- Segments, 16 B each (selector @+0, attrib @+2, limit @+4 dword, base @+8
  qword): `000h` ES, `010h` CS, `020h` SS, `030h` DS (base: lower 32 bits
  only), `040h` FS, `050h` GS (full 64-bit base).
- `060h` GDTR (selector/attrib reserved; limit `064h` lower 16; base `068h`),
  `070h` LDTR, `080h` IDTR (limit `084h`, base `088h`), `090h` TR (base
  `098h`).
- `0A0h`–`0CAh` reserved; `0CBh` **CPL** (byte); `0CCh` reserved.
- `0D0h` EFER. `0D8h`–`0DFh` reserved. `0E0h`–`0138h` PERF_CTL0–5 /
  PERF_CTR0–5.
- `148h` CR4, `150h` CR3, `158h` CR0, `160h` DR7, `168h` DR6, `170h` RFLAGS,
  `178h` RIP.
- `180h`–`1BFh` reserved; `1C0h` INSTR_RETIRED_CTR, `1C8h`
  PERF_CTR_GLOBAL_STS, `1D0h` PERF_CTR_GLOBAL_CTL.
- `1D8h` RSP, `1E0h` S_CET, `1E8h` SSP, `1F0h` ISST_ADDR, `1F8h` RAX.
- `200h` STAR, `208h` LSTAR, `210h` CSTAR, `218h` SFMASK, `220h`
  KernelGsBase, `228h` SYSENTER_CS, `230h` SYSENTER_ESP, `238h`
  SYSENTER_EIP, `240h` CR2, `248h`–`267h` reserved, `268h` G_PAT, `270h`
  DBGCTL, `278h` BR_FROM, `280h` BR_TO, `288h` LASTEXCPFROM, `290h`
  LASTEXCPTO (presumed, not yet verified).

**Host save area**: same layout, begins at `400h` of the host-save page
(host TR would be at `490h`). Software must **not** rely on the host save
area's format or contents — it is a scratch page the CPU writes on VMRUN and
reads on `#VMEXIT`.

**Segment attrib word** (12 bits = descriptor bits 55:52 | 47:40): 3:0 TYPE,
4 S, 6:5 DPL, 7 P, 8 AVL, 9 L, 10 DB, 11 G. A NULL segment is an all-zero
attrib (P=0). What the hardware actually observes: CS: D,L,P,R; SS: B,P,E,W,
C/D; DS/ES/FS/GS: D,P,DPL,E,W,C/D. Guest **CPL comes from the CPL field**
(`0CBh`), not from CS.DPL — this is a divergence from the ring discipline
every other part of this kernel assumes.

### 2.4 VMRUN consistency checks (§15.5.1 → VMEXIT_INVALID)

Guest `EFER.SVME=0`; CR0.CD=0 & NW=1; CR0[63:32]≠0; MBZ CR3 bit set; MBZ CR4
bit set; DR6[63:32]≠0; DR7[63:32]≠0; MBZ EFER bit set; LMA/LME on non-LM
CPU; LME & PG & !PAE; LME & PG & !PE; LME & PG & PAE & CS.L & CS.D all
nonzero; **the VMRUN intercept bit is set in the VMCB** (required — VMRUN
intercepting itself); MSRPM/IOPM beyond max PA; illegal event injection;
**ASID=0**; S_CET reserved bits; CR4.CET & !CR0.WP; CET + U_CET.SS +
EFLAGS.VM. VMRUN *may* load CR0 with PE=0, PG=1.

This list becomes the crate's `validate_guest_state()` — a host-testable
function so a `VMEXIT_INVALID` on metal is debuggable offline.

### 2.5 Exit codes (Appendix C)

`20h`–`2Fh` DR0–15 read, `30h`–`3Fh` DR write, `40h`–`5Fh` exception vector
0–31 (`52h` MC), `60h` INTR, `61h` NMI, `62h` SMI, `63h` INIT, `64h` VINTR,
`65h` CR0_SEL_WRITE, `66h`–`69h` IDTR/GDTR/LDTR/TR read, `6Ah`–`6Dh` writes,
`6Eh` RDTSC, `6Fh` RDPMC, `70h` PUSHF, `71h` POPF, `72h` CPUID, `73h` RSM,
`74h` IRET, `75h` SWINT, `76h` INVD, `77h` PAUSE, `78h` HLT, `79h` INVLPG,
`7Ah` INVLPGA, `7Bh` IOIO, `7Ch` MSR, `7Dh` TASK_SWITCH, `7Eh` FERR_FREEZE,
`7Fh` SHUTDOWN, `80h` VMRUN, `81h` VMMCALL, `82h` VMLOAD, `83h` VMSAVE,
`84h` STGI, `85h` CLGI, `86h` SKINIT, `87h` RDTSCP, `88h` ICEBP, `89h`
WBINVD, `8Ah` MONITOR, `8Bh` MWAIT, `8Ch` MWAIT_COND, `8Dh` XSETBV, `8Eh`
RDPRU, `8Fh`/`90h`/`93h`/`94h` EFER/CR0/CR3/CR4 write traps, `A0h` INVLPGB,
`A1h` INVLPGB_ILLEGAL, `A5h` BUSLOCK, `A6h` IDLE_HLT, `400h` NPF, `403h`
VMGEXIT, `407h` PML_FULL, `-1` INVALID (`0xFFFFFFFF`), `-2` BUSY, `-3`
IDLE_REQUIRED, `-4` INVALID_PMC.

HLT and PAUSE exits report "HW Advances RIP" = **Yes**. nRIP (control
`0C8h`) is saved when the NRIPS feature is present and is the next
sequential RIP for trap-style intercepts.

### 2.6 CPUID Fn8000_000A and NPT

`Fn8000_000A`: EAX = SVM revision, EBX = number of ASIDs. EDX bits
(numbered from Vol 3 rev 3.38 Appendix E.4.9, pp. 661–662 — the per-font
CMap extraction is §1): NP=0, LbrVirt=1, SVML=2, NRIPS=3, TscRateMsr=4,
VmcbClean=5, FlushByAsid=6, DecodeAssists=7, PmcVirt=8, PauseFilter=10,
PauseFilterThreshold=12, AVIC=13, VMSAVEvirt=15, VGIF=16, GMET=17,
x2AVIC=18, SSSCheck=19, SpecCtrl=20, ROGPT=21, HOST_MCE_OVERRIDE=23,
TlbiCtl=24, VNMI=25, IbsVirt=26, ExtLvtAvicAccessChg=27,
NestedVirtVmcbAddrChk=28, BusLockThreshold=29, IdleHltIntercept=30.
Unverified: `401h` AVIC_NOACCEL.

NPT: `NP_ENABLE` = `090h` bit 0, `N_CR3` = `0B0h`; requires `EDX[NP]`.
Consistency: an MBZ bit set in nCR3 → VMEXIT_INVALID; the host must have
hCR0.PG=1; a guest with gCR0.PG=0 + NPT is **legal** (guest-linear =
guest-physical, NPT still translates); A/D bits are hardware-set in NPT
entries. The exact NPT leaf PTE bit table is not yet transcribed; the plan is
to reuse `akuma-pte-amd64` constants (P|RW|A) for the identity map and verify
against §"Nested Table Walk" before the Linux phase.

### 2.7 The permission maps (§15.10.1 IOPM, §15.11 MSRPM)

**MSRPM** (§15.11 + Table 158): four 16-Kbit (2 KiB) permission vectors
in **two contiguous physical pages** (8192 bytes total). Two bits per
MSR — lsb = read, msb = write, **1 = intercept**. The byte ranges:
`0000h`–`07FFh` → MSRs `0000_0000`–`0000_1FFF`, `0800h`–`0FFFh` →
`C000_0000`–`C000_1FFF`, `1000h`–`17FFh` → `C001_0000`–`C001_1FFF`,
`1800h`–`1FFFh` reserved. An MSR the map does not cover auto-intercepts
when `MSR_PROT` is set — so an uncovered MSR is *more* trapped, not
less. 4-KiB aligned; a base at or past maximum physical address is
VMEXIT_INVALID.

**IOPM** (§15.10.1): "a linear array of 64K+3 bits (two 4-Kbyte pages,
and the first three bits of a third 4-Kbyte page)" — **12 contiguous
KiB**. Bit N = port N, **1 = intercept**; the three bits past 64K name
ports that do not exist. Same alignment and max-PA rules as the MSRPM.

**Where they live** (`amd64/src/kvm.rs`): `.bss` statics,
`#[repr(C, align(4096))]`, armed at compile time through the crate's
const builders (`msrpm::with_access`, `iopm::with_port`) — a `static`
stays free of interior mutability. Physical contiguity is the property
§15.11/§15.10.1 demand and two separately allocated frames do not have;
the linker guarantees it of image objects, and the PA is `VA −
KERNEL_VMA`, the derivation `linker.ld` bakes into every section (the
`multiboot2.rs` precedent). The seed arms exactly two bits: EFER read
(MSRPM vector for `C000_0000`+) and port `0x80`, exercised by the
guest's `rdmsr` and `out 0x80, al`.

## 3. Plan

**Phases:** 0 — trivial guest on metal (this milestone); 1 — (skipped;
folded into 0); 2 — Linux guest (PVH-style handoff; the Firecracker
device-map work in `crates/akuma-firecracker` and
`docs/reference/firecracker/memory-map.md` becomes the guest-side spec);
3 — akuma as its own guest.

As flown the phases renumbered: 0 trivial, 2 permission maps + AP hook,
3 long-mode guest (the consistency ladder was its diagnostic detour),
4 two-level translation guest (timeline below); the Linux phase is next,
and the original "3 — akuma as its own guest" stands behind it.

**Phase 0 design (decided, not yet built):**

- Crate `akuma-svm-amd64` (naming: `akuma-<thing>-<arch>`),
  `#![forbid(unsafe_code)]`, zero deps (or `akuma-pte-amd64` only): VMCB
  safe wrapper over `&mut [u8; 4096]` with const offsets, `ExitCode` enum
  (Appendix C), intercept bit constants (three dwords), `SegAttrib` builder
  (12-bit format), the §15.5.1 consistency-check validator, NPT table
  builder; host tests pinning every offset to its APM table citation.
- Kernel module `amd64/src/kvm.rs`: CPUID probe (one serial line either
  way), and behind a `kvmtest` cmdline flag (the `hdatest` precedent,
  `multiboot2.rs`) the trivial-guest run.
- Trivial guest: 32-bit flat protected mode, **no guest paging** (CR0.PE=1,
  PG=0 — legal with NPT on), CR4=0, RFLAGS=2, CPL field=0, guest
  EFER=SVME (LME/LMA=0), ASID=1, TLB_CONTROL=0; code donates a frame at
  GPA=PA under an identity-mapped NPT (2 MiB via one PML4→PDPT→PD 2 MiB
  leaf, 3 `FreshFrame`s — proving the NPT path from day one since phase 2
  needs it anyway). Guest body: `vmmcall` twice, `hlt`. VMMCALL exits log
  and resume (RIP += 4, or nRIP when NRIPS); HLT is completion.
- FS/GS zero-delta trick: guest FS/GS bases are set to the **current host
  values** so VMRUN changes nothing observable and VMLOAD/VMSAVE (which
  would clobber the kernel's per-cpu `%gs`) are never needed.
- Intercepts: dword0 INTR, CPUID, HLT, IOIO_PROT, MSR_PROT; dword1 VMRUN
  (mandatory) + VMMCALL; exceptions DB/UD/GP/PF for diagnosability.
- Unsafe budget: exactly three sites in `amd64/src/kvm.rs` — the
  EFER.SVME write, the VM_HSAVE_PA write, the VMRUN asm block. Everything
  else is built safely on `FreshFrame::bytes_mut` before `publish()`.
  After exit, the VMCB is re-read by mapping it at a kernel VA (the
  `paging::smoke_test` 1 GiB trick) and wrapping it in the safe crate type
  again. `scripts/check_unsafe_budget.py --update` carries the increase
  into review (the amd64/src budget was already +3 pre-existing on this
  branch).

## 4. Open items

- APM Vol 3 (24594) §3.3: exact `Fn8000_000A.EDX` bit numbers — **done
  2026-10-20**, extracted from the downloaded rev 3.38 PDF (Appendix E.4.9,
  pp. 661–662), table in §2.6 above.
- NPT leaf PTE bit table — **done 2026-10-21**: transcribed from Vol 2
  §5.3 (Figures 519–529, the 4-KiB leaf encoding) plus the
  §15.25.5/§15.25.7 accessed/dirty semantics into `crates/akuma-svm-amd64`:
  the `npt` constants + `leaf_4k()` builder, and a new `npf` module for
  the NPF `EXITINFO1` error-code bits (§15.25.6, p. 573) — bits 32
  (FINAL) vs 33 (GPT_WALK) name the failing level directly, which is the
  phase-4 diagnostic. Pinned numerically by
  `npt_bits_are_vol2_fig519_529` / `npf_error_bits_are_vol2_p573`.
- `LASTEXCPTO` offset verification; `401h` AVIC_NOACCEL.
- IOIO `EXITINFO1` bit decode (§15.10.2, not in the extract) and
  `VMEXIT_MSR` `EXITINFO1`/`EXITINFO2` layout — not yet transcribed; the
  exit handler reads RCX/RDX as shared GPRs instead (Table B-2: VMRUN
  saves only RSP/RAX/RIP/RFLAGS of the host, so the guest's RCX is the
  only copy of the MSR index there is).

---

## Background

- [`POST_AMD64_CODE_AUDIT.md`](POST_AMD64_CODE_AUDIT.md) §3 — the original
  Firecracker-on-akuma proposal and its effort table; §3.3 scopes it to the
  HP/Ryzen boxes.
- [`AKUMA_AMD64_BARE_METAL_SELFHOST.md`](AKUMA_AMD64_BARE_METAL_SELFHOST.md)
  — the self-host loop this experiment dogfoods: build on the box,
  `cp target/x86_64-unknown-none/release/akuma-amd64 /boot/akuma-amd64`,
  reboot (user in the loop), verify.
- `crates/akuma-firecracker` and
  [`../reference/firecracker/memory-map.md`](../reference/firecracker/memory-map.md)
  — the Firecracker memory-map vocabulary; from phase 2 it is the guest-side
  spec.
- AMD APM Vol 2 (24593) rev 3.45 — the sole source for §2; obtained from
  `docs.amd.com` (khub document `LNBl5evreogi0r25VIFF2g`). No Linux/KVM
  source was read, per the license constraint.
- AMD APM Vol 3 (24594) rev 3.38 (2026-07-21) — §E.4.9 extracted (see §2.6);
  obtained via the khub content URL below. No Linux/KVM
  source was read, per the license constraint.
  Direct PDF (verified 200, application/pdf, 31 MB, same khub scheme as Vol 2):
  `https://docs.amd.com/api/khub/documents/BqEoC2FadIAgbrUdEzDJIg/content`
  Viewer page: `https://docs.amd.com/v/u/en-US/24594_3.38_APM_Vol3_PUB`
  (khub document `BqEoC2FadIAgbrUdEzDJIg`). Fetch it with curl to `/tmp` and
  extract with the same stdlib PDF extractor used for Vol 2.
