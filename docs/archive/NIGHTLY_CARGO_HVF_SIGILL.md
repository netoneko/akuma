# Nightly `cargo` under HVF — undefined-instruction (EC=0x0) was an undelivered SIGILL

**Status: FIXED (2026-08-06).** Date of original report: 2026-08-05
(`docs/runbooks/selfhost-kernel-build.md` §6). Linked from that runbook and from
the `docs/README.md` symptom row for `EC=0x0` under HVF.

> **Follow-on, 2026-08-31 — the same shape at a second EC.** `cargo` startup
> was reported still printing `Unknown from EL0` lines, now interleaving
> **`EC=0x1d`** (*access to SME functionality trapped*) with `EC=0x0`, at ELRs
> ~27 KB from this doc's `0x112ac280`. The fix below covers `EC=0x1d` too — it
> is the same `_ =>` catch-all arm — so the probe *should* recover; what is
> unconfirmed is whether a handler is registered for it. Tracked as
> [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 27, which also notes that this
> doc's `EC=0x0` arm prints before it attempts delivery, so a **working** probe
> still looks like a crash in the console.

## TL;DR

Nightly `cargo` (1.99.0) died instantly under HVF at a constant
`ELR=0x112ac280` because **OpenSSL's `OPENSSL_cpuid_setup` armcaps probe executes
a `SM3SS1` instruction (FEAT_SM3) that Apple Silicon does not implement**. The
probe is *meant* to raise `SIGILL`, which a userspace handler catches to mark
the feature absent and continue — the standard OpenSSL/LibreSSL runtime
detection pattern. The Akuma kernel's `EC=0x0` (undefined-instruction) handler
**hard-killed the process instead of delivering `SIGILL`**, so the probe never
recovered and cargo died at startup. TCG (`-cpu max`) implements FEAT_SM3, so
the instruction executes there and no trap occurs — which is the entire reason
`HVF=0` avoided the crash.

The fix is one arm of the sync-exception handler in `src/exceptions.rs`: deliver
`SIGILL` (signal 4) via the existing `try_deliver_signal` path before killing,
mirroring what the kernel already did for `SIGSEGV` and for the spurious-SVC
`SIGILL` case. Verified: `cargo --version` runs under HVF, and a full
`cargo build` proceeds.

## Symptom (as observed)

```
[syscall] execve(path="/usr/local/bin/cargo", …) PID 121
[T18.66] [IA-DP] file region: fault_va=0x112ac220 seg_va=0x10000000 filesz=0x1da1c6c file_off=0x0
[Exception] Unknown from EL0: EC=0x0, ISS=0x0
  Thread=10, ELR=0x112ac280, FAR=0x112ac220, SPSR=0x800
[PSTATS] PID 121 (/usr/local/bin/cargo) 0.05s: 28 syscalls …
```

Established facts (all still hold, all explained by the root cause):

- `ELR=0x112ac280` is **constant** across every pid/thread/build — it is one
  fixed instruction at one fixed offset in the cargo binary, reached
  deterministically at startup.
- Dies ~tens of syscalls in, having only mmap'd/mprotect'd/read a few files —
  inside early startup, before any build logic.
- Nightly `rustc` from the same toolchain and apk `cargo` are fine under HVF;
  `HVF=0` (TCG) avoids it. Only this one binary, only under HVF.
- `EC=0x0` from EL0 is an **undefined instruction** (not a trap: a `CNTP`/`MRS`
  trap would be `EC=0x18`). The long-standing "traps HVF CNTP" note was a
  misattribution.

## Root cause (the chain)

1. `cargo` is a PIE dynamic executable linked against musl
   (`interpreter /lib/ld-musl-aarch64.so.1`, `NEEDED libc.so`, `libgcc_s.so.1`).
   Its git/HTTPS stack statically links **OpenSSL libcrypto**, which carries the
   `OPENSSL_cpuid_setup` AArch64 capability-detection code.
2. `OPENSSL_cpuid_setup` detects optional CPU features by executing the feature
   instruction inside a `SIGILL` handler (`sigaction(SIGILL,…)` + `sigsetjmp`):
   on a CPU that lacks the feature the instruction is UNDEFINED → `SIGILL` → the
   handler clears the capability bit and the probe loop continues. This is
   correct and is exactly what Linux does.
3. The probe functions are 8-byte thunks (`<feature insn>; ret`) laid out
   consecutively in `.text`:

   | vaddr (file off) | symbol | raw word | instruction | feature |
   |---|---|---|---|---|
   | `0x12ac250` | `_armv8_sm4_probe`   | `0xcec08400` | `SM4EKEY`     | FEAT_SM4 |
   | `0x12ac258` | `_armv8_sha512_probe`| `0xcec08000` | `SHA512H`     | FEAT_SHA512 |
   | `0x12ac260` | `_armv8_eor3_probe`  | `0xce010800` | `EOR3`        | FEAT_SHA3 |
   | `0x12ac268` | `_armv8_sve_probe`   | `0x04a03000` | SVE insn      | FEAT_SVE |
   | `0x12ac270` | `_armv8_sve2_probe`  | `0x04e03400` | SVE2 insn     | FEAT_SVE2 |
   | `0x12ac278` | `_armv8_cpuid_probe` | `0xd5380000` | `mrs x0, midr_el1` | (ID reg) |
   | **`0x12ac280`** | **`_armv8_sm3_probe`**  | **`0xce63c004`** | **`SM3SS1`** | **FEAT_SM3** |

   The neighbouring global symbols are `CRYPTO_memcmp` / `OPENSSL_cleanse`,
   confirming the OpenSSL provenance. cargo is loaded at base `0x10000000`, so
   the file offset `0x12ac280` maps to runtime `ELR=0x112ac280` — the exact
   faulting address in every crash. The faulting instruction is **`SM3SS1`**.
4. Apple Silicon (M1–M3) implements AES/PMULL/SHA1/SHA256/SHA512 and **SHA3
   (`EOR3`)** but does **not** implement FEAT_SM3/SM4/SVE/SVE2. So the SM3/SM4/
   SVE/SVE2 probes raise `EC=0x0`; the SHA3 and SHA512 probes execute normally.
   (The `EOR3` probe returning "not trapped" is what proves the issue is
   feature-specific, not a blanket "Apple rejects the probe mechanism".)
5. The kernel's `EC=0x0` handler (the `_ =>` arm of the EL0 sync handler,
   `src/exceptions.rs`) called `return_to_kernel(-1)` — it **never invoked the
   signal-delivery path**, so OpenSSL's `SIGILL` handler never ran and the
   process was killed outright. Every other fatal-fault arm in the same handler
   already used `try_deliver_signal` (SIGSEGV at the instruction/data-abort
   arms; SIGILL at the spurious-SVC arm); this arm was the lone exception.

## Evidence (decisive steps)

- **Static.** Pulled `/usr/local/bin/cargo` out of a *clone* of the disk image
  (read-only; never from an image a live VM holds). A whole-binary scan found
  **zero** `CPY*`/`SET*` (FEAT_MOPS), zero LSE128 `p`-form atomics, zero SVE in
  cargo's own text — ruling out the original prime suspects. The `ELR` mapped
  into OpenSSL's `_armv8_sm3_probe`; the 4 bytes at that offset are
  `0xce63c004` = `SM3SS1`.
- **Load base.** Read straight from the kernel's own `[IA-DP]` line:
  `seg_va=0x10000000`. `ELR - 0x10000000 = 0x12ac280`. No ASLR (constant ELR
  across pids) ⇒ one disassembly pins it for good.
- **Isolated repro (the theory test).** A tiny C program installs a `SIGILL`
  handler and executes a chosen instruction inside `sigsetjmp`; recover = handler
  ran. On the **unfixed** kernel: `udf`/`sm3`/`sm4` all print `PROBE …` then die
  (no `RECOVERED`); the serial shows `[Exception] Unknown from EL0: EC=0x0` for
  each. `sha3` (`EOR3`) does not trap. After the **fix**: `udf`/`sm3`/`sm4` print
  `RECOVERED sig=1 (SIGILL delivered)` and exit 0.
- **End-to-end.** After the fix, `cargo --version` prints
  `cargo 1.99.0-nightly …` (RC=0) under HVF. The serial shows **13 `SIGILL`
  deliveries** during cargo startup, at the exact probe PCs (`0x112ac250`/`268`/
  `270`/`280`/`364`), all caught by OpenSSL's handler at `0x112ac448`.

## The fix

`src/exceptions.rs`, the `EC=0x0`/`_ =>` arm of the EL0 sync handler — deliver
`SIGILL` to a registered handler before killing, exactly as the SIGSEGV and
spurious-SVC arms already do:

```rust
let (elr, spsr) = unsafe { ((*frame).elr_el1, (*frame).spsr_el1) };
crate::safe_print!(96, "[Exception] Unknown from EL0: EC={:#x}, ISS={:#x} ELR={:#x} — delivering SIGILL\n", ec, iss, elr);
if try_deliver_signal(frame, 4, elr, true, esr) {
    return 4; // SIGILL delivered to a userspace handler
}
// … no handler: fatal SIGILL; kill with -4 (was -1) …
```

- `try_deliver_signal` (`src/exceptions.rs`) returns `true` when a userspace
  handler was set up; the caller then returns the signal number to enter it.
- If no handler is registered, the process is killed with `SIGILL` (`-4`),
  matching Linux's default action for an unhandled undefined instruction (it was
  previously `-1`, which is not a valid signal number at all).
- Scope is tiny and the pattern is proven: the same `try_deliver_signal` call is
  already used for `SIGSEGV` (instruction/data abort arms) and for `SIGILL`
  (spurious-SVC arm).

## Verification matrix

| Gate | Result |
|---|---|
| Isolated SIGILL probe (`udf`/`sm3`/`sm4`) | unfixed: killed, no `RECOVERED`; **fixed: `RECOVERED sig=1`, RC=0** |
| `cargo --version` under HVF | was `EC=0x0` crash; **now `cargo 1.99.0-nightly`, RC=0** |
| `rustc --version` under HVF | still works (no regression) |
| `cargo build --release` under HVF | runs; compiles crates (see note below) |
| SIGILL deliveries during cargo startup, HVF | 13, all at OpenSSL probe PCs, all handled |
| TCG regression (`HVF=0`) | `cargo --version` RC=0; `EC=0x0` count = **0** (fix inert — TCG implements the features) |
| Host tests (crates) | pass, 0 failed |
| Clippy (`release-smp-shared` + `devbox-smoltcp,no-tests`) | clean |

### Note on the dependency fingerprint cache (§5.2c)

The fix makes nightly `cargo` **run**, which is the prerequisite for the
fast path. The first nightly-cargo build on a `target/` previously written by
apk cargo will still recompile the ~97 dependency crates once, because the two
cargos' fingerprints are incompatible (the existing §5.2c behaviour). That
one-time rebuild writes **nightly-cargo** fingerprints, so the *next*
nightly-cargo invocation reports `Finished` without recompiling the deps — the
outcome §5.2c says the apk-cargo fallback was throwing away. The apk-cargo
fallback (`/usr/bin/cargo` + nightly `rustc` on `PATH`) is no longer needed.

## What this is NOT (rule-outs, with evidence)

- **Not FEAT_MOPS / LSE128 / SVE in cargo's text.** A whole-binary
  `rust-objdump` scan of both cargo and the musl loader found *zero* `CPY*`/
  `SET*`, zero LSE128 `p`-form atomics, zero SVE mnemonics. The faulting
  instruction is `SM3SS1` (FEAT_SM3), a crypto instruction not on the original
  prime-suspect list — found by disassembling the actual faulting offset.
- **Not a CPUID/HWCAP misadvertisement.** The kernel's `MRS` emulator returns
  `0` for every ID register except `CTR_EL0`, and the auxv it builds hardcodes a
  conservative `AT_HWCAP` (FP/ASIMD/AES/SHA/CRC32/**ATOMICS**/…) with
  `AT_HWCAP2 = 0` — so nothing advertises SM3/SVE. The OpenSSL probe is
  unconditional runtime detection, not IFUNC selection.
- **Not the ld-musl startup class.** That class has a different, advancing
  (`N × 0x30000000 + 0x6c964`) ELR pattern and ASCII FARs; this ELR is constant.
- **Not a `CNTP` trap.** That would be `EC=0x18` (MRS), handled elsewhere. The
  old "traps HVF CNTP" note was a misattribution.

## Background

- Original open write-up: `docs/runbooks/selfhost-kernel-build.md` §6.
- The fingerprint-cache consequence: `docs/runbooks/selfhost-kernel-build.md`
  §5.2c.
- The `try_deliver_signal` framework and prior signal-delivery fixes
  (`SIGABRT`/`SIGSEGV`/default-action-for-pended-signals):
  `archive/SELFHOST_DEVBOX_SMOLTCP.md` ("SIGABRT delivery") and
  `docs/reference/subsystems/syscalls/signal.md`.
- The probe/thunk layout mirrors OpenSSL's `crypto/armcap.c`
  (`OPENSSL_cpuid_setup`); the same mechanism is used by libgcc's
  `__init_cpu_features` and by glibc.
