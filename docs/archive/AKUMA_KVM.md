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
3. KVM runs akuma.

**License constraint (binding for the whole project):** no Linux code may be
used or consulted — KVM included — because of GPL incompatibility. Everything
below is built from the **publicly available AMD specification only** (APM
24593 rev 3.45). Where the text records a value from memory that the spec then
corrected, that is documented, because from-memory hypervisor constants are
exactly how this project would ship a kernel that `#GP`s ring 0.

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
against §"Nested Table Walk" before the Linux rung.

## 3. Plan

**Rungs:** 0 — trivial guest on metal (this milestone); 1 — (skipped;
folded into 0); 2 — Linux guest (PVH-style handoff; the Firecracker
device-map work in `crates/akuma-firecracker` and
`docs/reference/firecracker/memory-map.md` becomes the guest-side spec);
3 — akuma as its own guest.

**Rung 0 design (decided, not yet built):**

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
  leaf, 3 `FreshFrame`s — proving the NPT path from day one since rung 2
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
  pp. 661–662), table in §2.6 above. Still open: the NPT leaf PTE bit table
  (§"Nested Table Walk") before the Linux rung.
  still unknown.
- NPT leaf PTE bit table (Vol 2 §"Nested Table Walk", ~p. 571).
- `LASTEXCPTO` offset verification; `401h` AVIC_NOACCEL.
- `amd64/src/boot.s:366` carries a stale comment claiming no FPU context
  switch; `sched.rs` has done fxsave/fxrstor since the FxArea work. Fix in
  passing.

## Timeline

| date | milestone work |
|---|---|
| 10-20 | M1 started: APM Vol 2 rev 3.45 extracted with a pure-stdlib PDF/ToUnicode-CMap extractor; all VMCB layouts, exit codes, consistency checks and the enabling sequence recorded (§2); no code yet |
| 10-20 | `akuma-svm-amd64` written and host-tested (13 tests); APM Vol 3 rev 3.38 downloaded, §E.4.9 EDX bits extracted with the same extractor (NP=0, NRIPS=3, table in §2.6); `amd64/src/kvm.rs` wired behind `kvmtest`, built and installed to `/boot/akuma-amd64` — awaiting the reboot with `kvmtest` on the kernel line |
| 10-21 | First `kvmtest` run, **rehearsed in QEMU/KVM on ryzen** (nested SVM, `overlays/ryzen/grub.cfg` entry 27, serial log): three bugs in the trivial guest, each only visible from the exit code. (1) `GUEST_CODE` used `F3 0F 01 D9` — that is `VMGEXIT` (SEV-ES), not `VMMCALL` (`0F 01 D9`); fixed, length 3 not 4. (2) VMCB segment `limit` is the byte-scaled 32-bit value: `0xFFFFF` is a 1 MiB limit and the guest code sits at ~17 MiB, so the first fetch was an intercepted `#GP` (exit `0x4d`, rip = code_pa); now `0xFFFF_FFFF`. (3) NPT entries need U=1 (nested walks are user-mode accesses): without it, exit `0x400` NPF with `info1=0x1_0000_0015` (present, fetch, final GPA). After all three: `trivial guest: PASS (vmmcalls=2)`. Metal before the fixes: entry 27 reset the box straight back to Pop (no klog — it never reached herd). Metal after: see below |

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
  — the Firecracker memory-map vocabulary; from rung 2 it is the guest-side
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
