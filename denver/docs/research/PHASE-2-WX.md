# PHASE-2-WX — research (R1, written before any code)

**Gate G1** (`denver/docs/REQUIREMENTS.md`): the W^X policy refuses `W|X` and RW→R+X
(`akuma-syscalls-mem::wx`) — HotSpot's executable-page marking dies at VM init
(`Failed to mark memory page as executable`, the taoxichuan attempt's blocker 1, reproduced
first-hand on the denver guest, `denver/gates/PHASE-1-METHOD.log`). Phase 1's transition line
fixed the pass condition: **`RWX PASS rw_to_rx=42` from the guest arm, and the java arm past
the blocker-1 line**, captured in `denver/gates/PHASE-2-WX.log` via
`sh denver/probe-wx.sh 2>&1 | tee denver/gates/PHASE-2-WX.log`. Expected after the pass: the
workload dies one blocker later, at `getcpu` (G2, the attempt's blocker 2) — that movement is
the evidence G1 closed; `java -version` passing outright would contradict phase-1's G2
evidence and would be recorded as a surprise, not assumed.

## Crates, files, functions found (named)

| piece | path | what it is |
|---|---|---|
| the pure policy | `crates/akuma-syscalls-mem/src/wx.rs` (133 LOC) | `check_prot` (W+X together), `check_page` (X on a written page), `check_range` (whole range before any change), `Wx::errno()` → `EACCES`; `#![no_std]`, `#![forbid(unsafe_code)]`, host tests |
| enforcement, mprotect | `crates/akuma-tokio-kernel-amd64/src/lib.rs` ~L1128 (`mprotect_arm`) | `if wx_on() && let Some(no) = wx::check_range(prot, page_written, pages).errno()` — refused before any PTE changes |
| enforcement, mmap | same file ~L1919 (`MMAP` arm) | `if wx_on() && let Some(no) = wx::check_prot(prot).errno()` |
| the widening choice | same file ~L1965 | `PROT_EXEC` mapping is `USER_RX` while `wx_on()`, else `USER_RWX` ("with `wx=0` the old widening to RWX is back") |
| the runtime hatch | same file ~L269 + ~L3014 | `static WX_ENFORCE: AtomicBool = new(true)`; `wx=0` on the cmdline stores `false`. **One switch already flips every enforcement point** — checks skipped, exec widens to RWX, mprotect unguarded |
| the probe (written in phase 1, before this code — lesson 2) | `denver/probes/jvmprobe.c`, `rwx` arm | mmap RWX (policy line, `RWX INFO`), then RW→write `mov eax,42;ret`→`mprotect(R|X)`→call: `RWX PASS rw_to_rx=42` is the verdict line the gate greps |
| the probe runner shape | `denver/vm-probe.sh`, `denver/probe-method.sh` | free slot → boot → ssh round trip → probe → always stop; `RESULT PASS` marker |
| the propagation chain | `denver/Cargo.toml` → `akuma-jvm-kernel/Cargo.toml` → `crates/akuma-tokio-kernel/Cargo.toml` → `crates/akuma-tokio-kernel-amd64/Cargo.toml` → `crates/akuma-syscalls-mem/Cargo.toml` | 1:1 feature relay, every level names the feature the same (`console` precedent, phase-0 D2) |
| the rig | `/root/denver/w/` | `denver` (proven kernel binary, untouched), `denver-jdk.img` + `vm-jdk.spec` (phase-1 image), key, port 2247 |

## What is reusable as-is

**Almost everything — this pass is a default, not new logic.** The `wx=0` runtime hatch
already implements the entire relaxed behavior path (proven in the taoxichuan W^X gate:
"With `wx=0` the RWX mmap succeeds"); the pure checks are already pure and host-tested; the
probe arm, image, and probe-runner shape exist from phase 1. New code = one `cfg!(feature)`
in `wx.rs`, one `cfg!(feature)` default on `WX_ENFORCE`, and feature lines in five
`Cargo.toml`s.

## Precedents (R9) — searched docs/archive, docs/reference, the sibling kernels

1. **`docs/archive/TAOXICHUAN_WX_POLICY.md`** (the canonical policy doc): "No page is both
   writable and executable … Stricter than Linux on purpose … a JIT (V8, a JVM, LuaJIT) cannot
   run under it. **Escape hatch: `wx=0` on the kernel command line restores the old behaviour
   (`PROT_EXEC` widens a mapping to RWX)**." Its verification section is also the probe's
   ancestry (`scripts/kvm/goose/wx_probe.c`, 11 checks — the `rwx` arm is its lineal descendant).
2. **git `0dcc33e7`** ("taoxichuan: remove the wx=0 hatch — W^X is unconditional (production
   kernels)"): the sibling branch later deleted the runtime hatch — a kernel *generation*
   decides its default policy, and this branch's default stays W^X (the runbook's R6: the
   repo's default build never changes). Denver selects the other policy *by building with a
   feature*, which is exactly the axis that commit drew.
3. **The metal went the other way**: `docs/archive/BUG_FIX_LIST.md` (2026-10-08, amd64
   mmap/mprotect W^X entry) and `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md` Fix 18 —
   the metal kernel **grants** `W|X` so V8/Chromium run, gated by `jitprobe.c`; phase-1's metal
   leg confirmed it first-hand (`RWX PASS rw_to_rx=42` on kernel 0.0.9). Two sibling kernels
   already chose opposite policies — a per-kernel policy switch is the *established shape*, and
   an off-by-default feature is how denver states its choice in the build.
4. **`docs/README.md`** known-symptoms row: "`EACCES` … W^X, on purpose … `wx=0` on the kernel
   command line turns it off" — userland-facing documentation of the hatch this feature
   compile-times.
5. **`TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`** (recovered, git `d7878378`): blocker 1 is this
   gate; its own run patched the two checks away *ad hoc* ("the two W^X checks in
   `akuma-tokio-kernel-core/src/mm.rs` replaced by `None`") and its Policy note says: "Running
   any JVM here means **giving up W^X** (RWX code cache). That is a policy decision, not a
   bug." This pass makes that throwaway patch a named, default-off build policy.
6. **Phase-0 D2 / the front crate's own docs**: "Capabilities added in later passes land behind
   features *here*, mapped onto existing infra — never a fork (R2), never a default the repo's
   build did not already have (R6)"; the `console` relay is the template every level copies.

No precedent found for a *per-program* exemption anywhere (`wx.rs`'s own header: "the kernel
keeps a switch rather than a per-program exemption") — none is added; the policy is global,
exactly like `wx=0` (R7).

## Decisions

- **D13 — the relaxation is one feature, `wx-relaxed`, named identically at every level**
  (the `console` naming rule; kebab-case like `no-tests`/`trace-err`). Off by default in every
  crate, including `denver` — its default build (phase-0 proven) is untouched, and the RED rig
  stays byte-reproducible.
- **D14 — the feature flips the pure check (`wx.rs`) and the enforcement default
  (`WX_ENFORCE`) — nothing else.** With the feature on, `WX_ENFORCE` boots `false`, which is
  precisely the proven `wx=0` behavior path (checks skipped; `PROT_EXEC` widens to `USER_RWX`;
  `mprotect` unguarded). The pure crate's flip is its host-tested policy statement (R3: the
  decision lives in the syscall-family crate; `cargo test -p akuma-syscalls-mem` strict,
  `--features wx-relaxed` relaxed); at runtime under this kernel it is consulted only when
  `wx_on()` — the feature chain guarantees both flip together. The cmdline hatch stays; with
  the feature on, `wx=0` is a no-op (no `wx=1` is parsed — R7: the policy is the policy).
- **D15 — the gate kernel is a second rig binary, not an overwrite**: `cargo build -p denver
  --features wx-relaxed` → `/root/denver/w/denver-wx`, spec `vm-wx.spec` (copy of
  `vm-jdk.spec`, `kernel=` line changed). Precedent D4: "the phase-0 rig is never touched, so
  the gate stays reproducible" — `probe-image.sh` did exactly this for the image.
- **D16 — the probe runner is `denver/probe-wx.sh`, `vm-probe.sh`'s shape** (brief: copy the
  shape, keep free-the-slot and the stop), with: platform sanity (`tokioprobe` `RESULT PASS` —
  the policy flip must not regress tokio), the `rwx` arm (the verdict), the `java` arm
  (blocker-movement evidence), and the phase-1 RED lines quoted in comments as the expected
  delta. `uname` in the capture carries the build hash — the relaxed kernel's identity is in
  the record.

## Composition plan and new-LOC estimate (R5 budget)

1. `crates/akuma-syscalls-mem/Cargo.toml` — `[features] wx-relaxed = []` + comment. ~5 LOC.
2. `crates/akuma-syscalls-mem/src/wx.rs` — `RELAXED` const, early-`Allow` in `check_prot` /
   `check_page`, doc paragraph, tests moved under `cfg` (strict off / relaxed on) + a relaxed
   test. ~18 LOC.
3. `crates/akuma-tokio-kernel-amd64/Cargo.toml` — feature mapping onto
   `akuma-syscalls-mem/wx-relaxed`. ~4 LOC.
4. `crates/akuma-tokio-kernel-amd64/src/lib.rs` — `WX_ENFORCE` default
   `!cfg!(feature = "wx-relaxed")` + comment citing the policy doc. ~3 LOC.
5. `crates/akuma-tokio-kernel/Cargo.toml`, `akuma-jvm-kernel/Cargo.toml`,
   `denver/Cargo.toml` — one relay line each + comment. ~12 LOC.
6. `denver/probe-wx.sh` — the runner. ~70 LOC. Kernel-side delta ≈ **45 LOC**, one feature.

**Risks.** HotSpot's reserve/commit may want more than mmap/mprotect (getcpu is next — that is
G2's gate, not this one); a RWX-mapping program other than the probe could now run under the
relaxed kernel (that is the policy, recorded); OOM/hang bounded by `timeout` on every ssh call;
the relaxation must not change any default build (R6 check: `cargo build -p
akuma-tokio-kernel-amd64 / -p taoxichuan --target x86_64-unknown-none` after the change).
