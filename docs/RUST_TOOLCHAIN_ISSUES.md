# Rust Toolchain on the Devbox — `cargo` Crash (Open Issue)

Tracks a `cargo` crash found while bringing up the self-hosted nightly Rust
toolchain (`aarch64-unknown-linux-musl` host, installed under `/usr/local` by
`overlays/devbox/bootstrap.sh` step 7) on the `devbox` profile. `rustc` works;
`cargo` does not. This is an **investigation report, not a fix** — written so
the next session can pick it up without re-deriving the repro.

## Status

| Component | State |
|---|---|
| `rustc --version` | ✅ works |
| `cargo --version` | ❌ crashes the kernel's exception handler; process killed, SSH session gets `Hangup` (exit 129) |
| VM survives the crash | ✅ yes — thread is recycled, subsequent SSH connections still work. Not the same class as the documented curl/DNS EC=0x20 "spins forever" wedge (`overlays/devbox/README.md` Backlog) |
| Root cause | ⚠️ **not confirmed** — best-evidence hypothesis below, needs a targeted repro to confirm |

## Symptom

Over SSH into the devbox (`ssh -p 2223 root@localhost`, `export PATH=/usr/local/bin:$PATH`):

```
$ rustc --version
rustc 1.98.0-nightly (c397dae80 2026-07-02)

$ cargo --version
Hangup
```

Kernel log (QEMU `-serial mon:stdio`) right before the crash:

```
[T65.86] [syscall] execve(path="/usr/local/bin/cargo", args=["cargo", "--version"]) PID 14
[T65.89] [mmap] pid=14 fd=3 file=/usr/lib/libgcc_s.so.1 off=0 len=0x31000 = 0x30100000 (lazy-file, 7 regions)
[T65.89] [mprotect] pid=14 owner=14 addr=0x3012f000 len=0x1000 prot=0x1
[T65.93] [mprotect] pid=14 owner=14 addr=0x11da0000 len=0x150000 prot=0x1
[T66.01] [IA-DP] file region: fault_va=0x1167fc20 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[T66.05] [IA-DP] file region: fault_va=0x10ee1ee8 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[T66.07] [IA-DP] file region: fault_va=0x11022cd8 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[T66.10] [IA-DP] file region: fault_va=0x1101ee40 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[Exception] Unknown from EL0: EC=0x0, ISS=0x0
  Thread=12, ELR=0x1101eea0, FAR=0x1101ee40, SPSR=0x800
  TTBR0=0x15000063517000, SP=0x60575830
[Cleanup] Thread 12 recycled after 10056us cooldown
```

`cargo` (dynamically linked, ~47 MB binary at `/usr/local/bin/cargo`) demand-pages
its own text segment via the lazy-file-region mechanism (`IA-DP`), then faults at
the address it just paged in, with ESR exception class `EC=0x0` — architecturally
"Unknown reason". `exceptions.rs`'s dispatch (`src/exceptions.rs` ~line 3467
onward) only special-cases `EC_SVC64`, `EC_DATA_ABORT_LOWER`,
`EC_INST_ABORT_LOWER`, `EC_MSR_MRS_TRAP` (`0x18`), and `EC_BRK_AARCH64`; anything
else — including `0x0` — falls into the catch-all `_ =>` arm at line ~3467, which
logs `[Exception] Unknown from EL0: ...` and kills the process (`return_to_kernel(-1)`).
That catch-all is why the process dies cleanly instead of the kernel wedging.

## Leading hypothesis: QEMU HVF traps physical-timer (`CNTP_*`) register access as a bare `EC=0x0`

`EC=0x0` already has a confirmed, documented precedent **in this exact codebase**,
for a different code path:

- `src/timer.rs` (`read_counter`, ~line 110): *"the physical timer/counter is
  owned by the hypervisor under QEMU HVF and trapping to it faults the guest
  (EC=0x0); the virtual timer works under HVF, TCG, and bare-metal EL1 alike."*
- `src/main.rs` (~line 880): *"The physical timer (CNTP/PPI 30) is not used — it
  is inaccessible to the guest under QEMU HVF (programming it faults with
  EC=0x0)."*

Those are about the **kernel** (EL1) avoiding `CNTP_*` and using `CNTV_*`
instead. The mechanism they describe — QEMU/HVF's nested-virtualization layer
denying access to the physical timer/counter and delivering a bare, unclassified
`ESR_EL1` (both `EC` and `ISS` reading as `0`) rather than the architecturally
normal trap — is not EL-specific. If **userspace** code executes an `MRS`/`MSR`
against a `CNTP_*` register (`CNTP_CTL_EL0`, `CNTP_TVAL_EL0`, `CNTP_CVAL_EL0`,
`CNTPCT_EL0`), the same HVF quirk would plausibly produce the same bare `EC=0x0`
from EL0 — matching our log exactly, including `ISS=0x0` (a real, architecturally
decoded `EC_MSR_MRS_TRAP` would carry non-zero `ISS` fields identifying the
register and destination `Xt`; ours has none, consistent with HVF handing back
an empty `ESR_EL1` rather than a decodable trap).

Supporting detail: `exceptions.rs`'s `EC_MSR_MRS_TRAP` handler (`src/exceptions.rs`
~line 3409) only emulates `CTR_EL0` reads specifically; anything else (including a
real, correctly-classified `CNTPCT_EL0` trap) falls through to `else { 0 }` and
returns a *harmless* dummy `0` — it does **not** crash. That rules out "the kernel
doesn't emulate `CNTP` reads" as the direct cause; the crash requires the
malformed/bare `EC=0x0` path specifically, which bypasses `EC_MSR_MRS_TRAP`
handling entirely and lands in the generic catch-all instead.

**Why `cargo` and not `rustc`:** both are dynamically linked against
`libstd-*.so` and go through the same `PT_INTERP` path (confirmed: a later,
slower `cargo build`-style rustc invocation also mmaps
`libstd-4c2645ca1464cad6.so`), so "dynamic linking" alone isn't the
differentiator. Something `cargo` does early in its own startup (before
`--version` even prints) that `rustc`'s startup doesn't — a timing check,
jobserver/thread setup, or a libgcc_s unwind-info init path — is the most likely
place a stray `CNTP_*` read would come from. Not yet pinned to a specific
instruction/call site.

**Why the documented self-hosting milestone's `cargo build --release` (June
2026, also under HVF — see `docs/AKUMA_SELF_HOSTING.md` line ~160) didn't hit
this:** unconfirmed, but the most likely explanation is toolchain drift, not
environment drift. That milestone pinned whatever nightly was current in June
2026; `overlays/devbox/bootstrap.sh` step 7 downloads whatever nightly is
*currently* live from `static.rust-lang.org` (as of this writing,
`1.98.0-nightly (c397dae80 2026-07-02)`). A newer `cargo` or one of its vendored
dependencies could easily have picked up a new timing/instant call on the
`aarch64-unknown-linux-musl` path between June and July that the older nightly's
`cargo` didn't execute.

## What would confirm/fix this

1. **Confirm the register.** Patch the `_ =>` catch-all in `src/exceptions.rs`
   (~line 3467) to also disassemble/print the 4 bytes at `ELR-4`..`ELR` (the
   trapping instruction itself) before killing the process. An `MRS`/`MSR`
   encoding targeting `CNTP_CTL_EL0` (`op0=3 op1=3 crn=14 crm=2 op2=1`),
   `CNTP_TVAL_EL0` (`crm=2 op2=0`), `CNTP_CVAL_EL0` (`crm=2 op2=2`), or
   `CNTPCT_EL0` (`crn=14 crm=0 op2=1`) would confirm the hypothesis directly.
2. **Cheap experiment:** boot the same devbox image with `-accel tcg` instead of
   HVF (edit `overlays/devbox/run.sh`'s `cargo run` invocation, or run the
   underlying `qemu-system-aarch64` command by hand with `-accel tcg`) and retry
   `cargo --version`. Per the `timer.rs` comment, TCG does *not* have this
   quirk for `CNTP`; if the crash disappears under TCG, that's strong
   confirmation (at the cost of much slower emulation — not a real fix, just a
   diagnostic).
3. **If confirmed:** the real fix is a generic one, not `cargo`-specific — add
   `CNTP_CTL_EL0`/`CNTP_TVAL_EL0`/`CNTP_CVAL_EL0`/`CNTPCT_EL0` to the
   `EC_MSR_MRS_TRAP` emulation table in `exceptions.rs` (return a synthesized
   value, e.g. proxy through the already-working `CNTV_*` equivalents) so any
   userspace binary that happens to probe the physical timer degrades
   gracefully instead of dying — mirroring how the kernel already routes its
   own timer usage through `CNTV` instead of `CNTP`. That only helps if the
   trap is ever classified as `EC_MSR_MRS_TRAP` in the first place; if HVF
   really does hand back a bare `EC=0x0`/`ISS=0x0` with no decodable fields (as
   the evidence above suggests), then the fix may need to live in the `_ =>`
   arm itself: detect "`ELR` points at an `MRS`/`MSR` instruction referencing a
   `CNTP_*` register" by decoding the raw instruction bytes (since `ISS` gives
   nothing to go on here), and handle it the same way `EC_MSR_MRS_TRAP` does.

## Repro

```
cd /Users/netoneko/github.com/netoneko/akuma
./overlays/devbox/run.sh   # boot; wait for "[herd] Started sshd" in the log
ssh-keygen -R "[localhost]:2223"   # only needed after a rebuild (new host key)
ssh -o StrictHostKeyChecking=no -p 2223 root@localhost
export PATH=/usr/local/bin:$PATH
cargo --version   # crashes; rustc --version works fine
```

**Caution:** the devbox VM is single-core (`-smp 1`) and 4 GB RAM. `rustc` alone
needs ~2 GB+ per `docs/RUST_TOOLCHAIN.md`. Running two compiler invocations
concurrently (e.g. a hung SSH client retried while the first invocation is still
running server-side) can starve `sshd` of CPU entirely and make the VM look
wedged even though it isn't crashed — observed firsthand during this
investigation. Wait for one SSH command to fully return before starting
another, and use a generous client-side timeout (rustc/cargo invocations can
legitimately take minutes on this hardware).
