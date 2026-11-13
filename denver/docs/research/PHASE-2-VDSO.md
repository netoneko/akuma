# PHASE-2-VDSO — research (R1, written before any code)

**Gate G3** (`denver/docs/REQUIREMENTS.md`): "no vDSO: `AT_SYSINFO_EHDR` never in auxv … on
metal HotSpot then falls back to the legacy vsyscall page and SIGSEGVs inside it". The
FAULTNAME pass guest-proved the death (`rip=cr2=0xffffffffff600800 code=0x14`). The row's
two candidate closings: "a minimal vDSO page + `AT_SYSINFO_EHDR` in `akuma-elf-exec`" vs.
"provably serving the fallback address". This artifact decides with first-hand binary
evidence, and it corrects the transition line's assumption before any code is written.

## Finding 0 — the consumer is the *page*, not auxv; nobody in this userland reads AT_SYSINFO_EHDR

The transition line expected "a minimal vDSO page + `AT_SYSINFO_EHDR`" to unblock the
workload. The metal `hs_err` (phase 1) said the fault frame was `V [libjvm.so+0xaef710]` with
`RAX=RIP=0xffffffffff600800`. Disassembling that site in the exact JRE the guest runs
(`/usr/lib/jvm/java-21-openjdk/lib/server/libjvm.so`, the alpine musl Corretto 21 build staged
into `denver-jdk.img`):

```asm
aef6f0:  push   %rbp
aef6f1:  xor    %edx,%edx                  # getcpu(node=NULL)
aef6f3:  xor    %esi,%esi                  # getcpu(node=NULL)
aef6f5:  mov    $0xffffffffff600800,%rax   # 48 c7 c0 00 08 60 ff — sign-extended imm32!
aef703:  lea    -0x4(%rbp),%rdi            # &cpu, pre-zeroed
aef70e:  call   *%rax                      # → the legacy vsyscall getcpu slot
aef710:  cmp    $-1,%eax                   # fault return address lands here
aef718:  cmovne -0x4(%rbp),%eax            # cpu on success, -1 only on a literal -1
```

Three facts, each first-hand:

1. **HotSpot 21 hardcodes the legacy vsyscall getcpu address as a sign-extended 32-bit
   immediate** (`0xff600800` → `0xffffffffff600800`) and calls it as a *C function*
   (`call *rax`, `int(void *cpu, void *node, void *tcache)` returning `int`). This is why the
   session-1 byte grep for the 8-byte constant found nothing in any binary — the constant is
   4 bytes in a `mov r32, imm32`. The load is **not** routed through `AT_SYSINFO_EHDR`.
2. **A full disassembly sweep of libjvm.so finds exactly one vsyscall reference in code**:
   `$0xffffffffff600800` (getcpu, +0x800). No code reference to +0x000 (gettimeofday) or
   +0x400 (time) anywhere in the JRE's `.so`s (`objdump -d | grep '0xffffffffff600[048]00'` →
   1 hit); the earlier imm32 "hits" (`00 00 60 ff`) were masks and `movl $0x21` auxv-key
   stores, not address loads. The only *proven* slot is +0x800.
3. **No consumer reads `AT_SYSINFO_EHDR` here**: musl (`ld-musl-x86_64.so.1`) does not parse
   auxv key 33 at all (it consumes only the load/entry keys — that is why `AUXV RED` was
   cosmetic for the workload all along); HotSpot's getcpu path is proven above; Go (the other
   known key-33 reader) validates the ELF magic before use and would bail — and no Go runs in
   this gate.

**Decision the evidence forces:** a real ELF vDSO + `AT_SYSINFO_EHDR` alone would flip the
probe arm and fix *nothing* — the death would recur. The consumer interface is the **legacy
vsyscall page at its ABI-fixed address**. G3's closing is the row's *second* option:
"provably serving the fallback address" — plus the auxv *disclosure* of that page (D21), which
is what the phase-1 probe arm actually greps.

**The shape of the fix.** The legacy page layout is a Linux ABI constant: three C-callable
syscall stubs — `mov $nr,%eax; syscall; ret` — at +0x000 (gettimeofday, nr 96), +0x400
(time, nr 201), +0x800 (getcpu, nr 168), `0xcc` padding elsewhere. All three are served (same
page, zero marginal cost) even though only +0x800 is proven consumed. Consequence worth
recording: when the page exists, the +0x800 stub executes syscall 168, which is still ENOSYS
(G2's arm) → rax = -38; HotSpot's wrapper tests only `cmp $-1`, does **not** treat -38 as
failure, and reads the pre-zeroed cpu → **0**. So for the JVM path the getcpu need *softens*
to "cpu is always 0" without G2 closing; the raw `getcpu(2)` arm stays RED and stays G2's
gate. Whatever the workload does next is movement to record, not a verdict to assume.

## Crates, files, functions found (named)

| piece | path | what it decides |
|---|---|---|
| the widen rule | `crates/akuma-pte-amd64/src/lib.rs` `step_down` (~L279) | a mapping descent **widens** a present intermediate entry to `P\|RW\|US`, `& !NX` — "an intermediate entry has to be at least as permissive as any leaf beneath it. Enforcement lives entirely in the leaf." |
| the walk | `crates/akuma-pagetable/src/amd64.rs` `map_leaf` (~L213) | descends present entries (applying `widened`), allocates fresh zeroed tables via the PMM closure, stamps `pte::table_entry(frame, user)` — user iff the leaf is user |
| the one-call mapper | `crates/akuma-boot-amd64/src/paging.rs` `map_page`/`map_page_in` (~L104/114) | kernel-root mapping + core-local `invlpg`; "edits only the boot-time kernel tables, where core-local is complete" |
| the leaf vocabulary | `akuma_pte_amd64::PteProt::USER_RX` | "User read + execute, not writable" — the exact W^X-compliant shape the stubs need |
| the propagation | `crates/akuma-tokio-kernel-amd64/src/lib.rs` `copy_kernel_half` (~L2307) | every task root copies PML4 **256..512** from the live CR3 — a boot-time kernel-root map is inherited by every later task, zero per-exec cost |
| the frees | same file `free_user_root` (~L2649) | frees PML4 **0..256 only** — the upper half is never touched, so the page cannot leak into a teardown path |
| the auxv table | same file `load_task` (~L2590) | `AUX_N = 14` fixed entries, `ring3::AT_*` keys — the table the phase-1 auxv arm reads via `getauxval` |
| the auxv key home | `crates/akuma-ring3-amd64/src/lib.rs` (~L56–90) | the `AT_*` ABI constants + `build_stack`; host-tested (`#[cfg(test)] mod tests`) |
| the boot insertion point | same machine half, `boot()` (~L2956) | after `install_surface()` (PMM/heap up), before `map_virtio`/smp — single core, kernel root still CR3: `map_page`'s documented preconditions |
| the fault proof | the FAULTNAME capture + `/root/denver/w/vmrun-diag.log` | ring3 fetch on the top-GiB path faults not-present ⇒ PDPT[511] is absent in the kernel root itself (task PML4[511] is a copy of it) — no `Step::Large` risk in the walk |
| the probe vocabulary | `denver/probes/jvmprobe.c` `arm_auxv` | `getauxval(33)` → `AUXV PASS AT_SYSINFO_EHDR=<val>` / `AUXV RED AT_SYSINFO_EHDR missing` — the exact flip this gate greps |

Why PML4[511] is safe to widen: `KERNEL_VMA = 0xFFFFFFFF80000000` (linker.ld) puts the kernel
image in PML4[511] **PDPT[496..~510]**; the vsyscall address is PML4[511] **PDPT[511]** (the
top GiB, unmapped — proven by the guest fault). U/S ANDs down the walk: widening PML4[511]
makes nothing user-accessible by itself; the kernel-image levels below stay `U=0`; only the
fresh `PDPT[511]→PD→PT→leaf` chain is `U=1`. This is also exactly Linux's own arrangement —
the vsyscall page lives in the kernel-half VA range, mapped user-executable globally.

## Precedents (R9) — searched docs/archive, docs/reference, the sibling kernels, git history

1. **The metal kernel's loader** (`amd64/src/loader.rs` ~L90–611): a fourteen-entry auxv
   (`AT_PHDR`/`AT_PHNUM`/`AT_PHENT`/`AT_BASE`…), **no `AT_SYSINFO_EHDR`, no vsyscall page** —
   and the metal's own `hs_err` (pc=`0xffffffffff600800`, SEGV_MAPERR) is the recorded
   consequence. Absence precedent: no sibling serves this; there is nothing to copy, only an
   ABI to serve. The loader's `STACK_WORDS_MAX` comment ("the auxv grew past what … budgets")
   is the warning this pass heeds by *gating* `AUX_N` 14→15 behind the feature.
2. **The metal's docs name the gap as a class**: `userspace/ext2probe/c/read_syscall_cost.c`
   ("on Akuma … no vDSO — `clock_gettime` is a real `svc`"),
   `docs/archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md` ("`clock_gettime` … no vDSO on Akuma"),
   `docs/reference/subsystems/syscalls/signal.md` (~L291, runtimes "that rely on the vDSO").
   Serving the page does *not* close the clock gap (G5): the stubs still syscall; only the
   *interface* HotSpot hardcodes is now present.
3. **Sibling kernels** (`shanghai/`, `taoxichuan/`, `akuma-tokio-kernel*`): `rg -il
   'vsyscall|vdso|AT_SYSINFO'` → zero hits. `akuma-elf-exec`'s doc records the lineage
   ("nothing at all in `taoxichuan`, whose `AT_BASE` was hard 0"). This pass is the first
   vDSO/vsyscall work in the tree — decided from the consumer evidence, not copied.
4. **The attempt doc** (`git show d7878378:docs/archive/TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`,
   not on this branch): blocker 2 was `getcpu` ENOSYS on aarch64 (raw syscall → HotSpot exits).
   Same capability gap, *different interface per arch*: on x86_64 HotSpot asks through the
   hardcoded vsyscall page. R9's discipline (cite before deciding) is why the fix targets the
   x86_64 ABI shape, not a `getcpu` syscall (that stays G2).
5. **The WX gate's policy shape** (D13/D14, `denver/docs/research/PHASE-2-WX.md`): a kernel
   generation states policy by *features at compile time* ("the hatch at compile time"), and
   the relay is the `console` naming rule. Reused: feature `vsyscall`, relayed 1:1, no builder
   knob (R7).
6. **The rig rule** (D15/D18): a second kernel binary + spec (the phase-0/1/WX/FAULTNAME
   binaries stay untouched), rig identity asserted in the capture.

## Decisions

- **D21 — serve the page; disclose it in auxv.** The consumer interface is the legacy
  vsyscall page at `0xffffffffff600000` (disassembly, Finding 0). `AT_SYSINFO_EHDR` is
  emitted pointing at that base — the kernel's one kernel-provided fast-path code page, in the
  auxv key reserved for exactly that concept. This flips the gate arm honestly (the value is
  the real address of a real kernel-provided page) while the actual consumer keeps using the
  ABI constant.
- **D22 — the semantic caveat is recorded, the real ELF vDSO is deferred (G5).**
  `AT_SYSINFO_EHDR` points at a non-ELF page: the legacy layout fixes stub slots at +0/+0x400/
  +0x800, an ELF header needs +0 — mutually exclusive. Known consumers are safe by
  construction: musl ignores the key; HotSpot never reads it (proven above); Go validates the
  ELF magic before use and would bail. A real vDSO (symbols, vvar fast clock) serves no
  current consumer and is G5's home (`CYCLES` measured the trap tax in phase 1) — not built
  here (R5: budget for what the gate greps).
- **D23 — the map is one boot-time call into the kernel root.**
  `paging::map_page(0xffff_ff60_0000, frame.pa(), PteProt::USER_RX, MemAttr::WriteBack)` with
  the stub bytes written through the physmap first (`phys::Frame::alloc_zeroed` +
  `write_bytes`, the `load_task` block pattern). Relies on `step_down`'s documented widen +
  `copy_kernel_half`'s PML4[256..512] propagation; placed before the first spawn and before
  SMP starts (`map_page`'s own precondition: "boot-time kernel tables … core-local is
  complete"). Zero per-exec cost; `free_user_root` (0..256) never sees it; no region
  registered (it is not a loader region; Linux likewise reserves `[vsyscall]`).
- **D24 — the stub bytes are machine-half consts** in the exact legacy layout (`0xcc`
  padding, three `mov imm32,%eax; syscall; ret` stubs at 0x000/0x400/0x800). Machine-code data
  has the machine half as its home (the shootdown-stub precedent); the pure, host-testable
  decision — the auxv key value — is the gated `AT_SYSINFO_EHDR = 33` const in
  `akuma-ring3-amd64`, pinned by that crate's host test. The +0x800-ENOSYS-softens-getcpu
  consequence (Finding 0) is recorded as an expectation for the gate run: the JVM should move
  *past* the SIGSEGV; the raw `GETCPU` arm stays RED (G2 unchanged).
- **D25 — feature `vsyscall`, off by default, relayed 1:1**
  `denver → akuma-jvm-kernel → akuma-tokio-kernel → akuma-tokio-kernel-amd64 →
  akuma-ring3-amd64` (the `wx-relaxed` shape, R6/R7). `AUX_N` 14→15 and the boot map are both
  behind the feature; the default build serves the same auxv table and maps nothing new. Gate
  kernel: `denver-vdso` (`--features wx-relaxed,vsyscall` — it rides the WX policy) +
  `vm-vdso.spec` (`vm-wx.spec` with `kernel=` swapped); the proven binaries stay byte-put.
- **D26 — the probe is `denver/probe-vdso.sh`** (probe-faultname shape: epoch stamps, console
  window, `-a`-tolerant greps). Verdict arms, in order: `uname` says `denver` **and not** the
  WX rig hash `8a658723` (staging proof: the rig *did* change) → tokioprobe `RESULT PASS`
  (platform sanity) → `RWX PASS rw_to_rx=42` (wx-relaxed still active) → `AUXV PASS
  AT_SYSINFO_EHDR=` (the G3 flip) → the java arm, then the console window must carry **no**
  `rip=0xffffffffff600800` (the closed death must not recur) → `RESULT PASS`. The workload's
  next shape (new fault elsewhere, clean nonzero exit, or outright `JVM PASS`) is recorded as
  movement/surprise lines for the re-rank — it does not gate.

## Composition plan and new-LOC estimate (R5 budget)

1. this artifact (~150 LOC).
2. `crates/akuma-ring3-amd64`: feature + gated const + one pin test (~10 LOC).
3. `crates/akuma-tokio-kernel-amd64`: gated stub-page module + `map_vsyscall()` + boot call +
   gated auxv entry (~60 LOC); feature line in Cargo.toml.
4. Relay manifests: `akuma-tokio-kernel`, `akuma-jvm-kernel`, `denver` (3 lines).
5. `denver/probe-vdso.sh` (~120 LOC, faultname shape).
6. Bookkeeping: REQUIREMENTS row + re-rank, LOOP_LOG, RESULT, lessons.

Kernel LOC behind the feature: ≈ 60 (machine half) + ≈ 10 (ring3). Verification: default
build unchanged (`cargo build -p denver`, `-p taoxichuan`), host test (`cargo test -p
akuma-ring3-amd64`), clippy clean, then the gate run on the metal.
