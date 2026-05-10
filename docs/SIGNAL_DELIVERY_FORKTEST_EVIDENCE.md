# Signal delivery vs forktest (Go): conclusion and evidence

This document records why the **general signal delivery and return-to-user path** (async delivery, sigframe, `rt_sigreturn`, register restore) is the **leading hypothesis** for forktest / Go failures involving `SIGURG`, and why a **narrow syscall-stub deferral** is insufficient. It is meant to support a **roll back** of heuristic mitigations and a **focused audit** of signal machinery.

---

## Conclusion (one paragraph)

Observed failures line up with **incorrect or fragile user context across `SIGURG` delivery and `sigreturn`**, not only with “async signal while still in the musl syscall trampoline.” Kernel serial logs show **`SIGURG` delivered to the forktest child while interrupted in the Go heap (`ELR` ~`0x86768`, `memclr` / malloc path)**, immediately followed by a **data abort** whose **FAR** is a **small negative integer** (`0xfffffffffffffffa`, i.e. **-6**) and **general-purpose registers** holding **errno-like negatives** (e.g. **`x0 = 0xffffffffffffffea`**, **-22**), while **`last_sc`** is idle — consistent with **resumed user code executing with corrupted argument/state**, not with a simple “wrong syscall number at SVC” story. A mitigation that only re-pends `SIGURG` on **some** syscall returns when **`ELR` is in a fixed low-VMA window** does not cover **other threads**, **other PCs**, or **other delivery boundaries**, and logs show **defer and deliver in quick succession** for the same process anyway.

---

## Evidence A: Child thread — delivery in Go allocator, then bogus fault

Source: serial capture **`crash27.log`** (repo root), around **T22.72–T22.76**.

Sequence (abbreviated; see file for full lines):

1. **Two** `SIGURG` injections: `tkill(tid=8, sig=23)` (parent) and **`tkill(tid=17, sig=23)`** (child).
2. **`[signal] deliver sig=23 slot=17`** for **pid=94** (forktest child), with **`fault_pc=0x86768`** — matches userland stacks showing **`runtime.memclrNoHeapPointers`** / malloc (`pc≈0x86768`), **not** the musl stub band (~`0x13060`).
3. **`[sigreturn] restoring: ... pc=0x86768`** — handler returns to the same allocator site.
4. Immediately: **`[DA-MISS]`** / demand-paging path for **`va=0xfffffffffffffffa`** (FAR **-6**).
5. Kernel diagnostic **`[WILD-DA]`** explicitly flags FAR as a **small negative** consistent with **“syscall error used as pointer”** pattern; register dump includes **`x0=0xffffffffffffffea`** (**-22** / errno-shaped) and **`last_sc=18446744073709551615`** (idle / no active syscall tracking).

**Interpretation:** The failure is triggered in a path where **async `SIGURG` + `sigreturn`** should leave the thread able to continue **large zeroing / allocation** safely. The subsequent fault and register shapes argue for **broken user register file or wrong resume context** after signal handling, i.e. **signal delivery / restore**, not “parent stuck in `read` glue only.”

---

## Evidence B: Parent thread — stub defer fires but does not end the story

Same log shows many lines of the form:

- **`[SIGURG] re-pend tid=8 (stub defer syscall ret) ELR=0x13060 x8=21`** (and similar for `x8=22`, `63`).

So the **narrow** mitigation can run on **tid=8** (`forktest_parent`).

But the same capture also shows:

- **`[exit_group] pid=90 ... forktest_parent code=0`** after ~30s — parent may survive that run, while **child** already hit the allocator path above.
- In other places (e.g. around **T51.10**), **stub defer** for **tid=8** appears **near** a subsequent **`deliver sig=23 slot=8`** with **`fault_pc=0x86d04`** (Go runtime, not `0x13060`) — showing **delivery still occurs** on other boundaries / PCs.

**Interpretation:** The stub defer is **not** a proof of fix; it is at best **one skipped delivery opportunity**. It does **not** globally serialize or correctness-proof `SIGURG` for Go.

---

## Evidence C: Original Pattern 2 parent symptom (user-reported)

User capture (outside this file): **`SIGSEGV`** in **`syscall.Syscall(0x3f, …)`** (`read`), **`PC=0x13060`**, **`addr`** in a large heap/near-heap range, with **`m=0`**, **`GOMAXPROCS=1`**.

**Interpretation:** That symptom is **compatible** with **corrupt state when returning from or near the syscall path** (including wrong resume after async signal), but **by itself** it does not isolate “musl stub only” vs “full delivery bug.” Evidence A is what ties the issue more strongly to **delivery/restore** across **arbitrary** user PCs.

---

## Why a syscall-stub-only deferral is the wrong primary fix

| Limitation | Why it matters |
|--------------|----------------|
| **Thread scope** | Go uses **multiple OS threads** (`tid=8` parent, `tid=17` child). Deferring only on one thread’s syscall return misses the other. |
| **PC scope** | Child fault at **`~0x86768`** is outside a fixed **`0x10000..0x20000`** “stub” window. |
| **Syscall scope** | Allocator and runtime use many syscalls; `SIGURG` can deliver on **returns** and other paths not covered by a short list (`read` / `epoll_*`). |
| **Temporal** | Re-pend → **still pending** → delivery on **next** eligible boundary can be **microseconds** later; races remain. |

---

## Suggested audit scope (for follow-up work)

Prioritize **read-only** tracing and review of:

1. **`try_deliver_signal`** — how the interrupted frame is chosen and written; interaction with **altstack**; **`fault_pc`** / `si_addr` semantics for async signals.
2. **Sigframe layout and alignment** — AArch64 `ucontext`, **`RT_SIGFRAME`**, reserved fields, and **`SA_RESTORER`** path.
3. **`do_rt_sigreturn` / `rt_sigreturn`** — full register restore; interaction with **pending** `SIGURG` immediately after return (ordering vs syscall result in **`x0`**).
4. **Preemption / timer / `tkill`** — who is targeted (`tid` vs process-wide), and whether **nested** or **re-entrant** delivery is possible while building frames.
5. **NEON / FP** — EL0 sync handler saves/restores wide state; verify signal delivery does not assume a partial frame.

Cross-reference existing notes: **`docs/GO_FORKTEST_DEBUG.md`**, **`docs/SIGNAL_DELIVERY.md`**, **`docs/GO_FORK_EXEC_FIXES.md`**.

---

## Artifact reference

- **`crash27.log`** (user-provided serial log at repo root in the session that produced this write-up): lines **~3050–3085** (child `SIGURG` → `sigreturn` → `WILD-DA`), **~29790–29795** (parent defer vs deliver proximity), **~29904** (parent `exit_group` success on that run).

---

## Document history

- **2026-05-10:** Initial write-up after analysis of `crash27.log` and user-reported Pattern 2 stacks; supports rollback of narrow `SIGURG` stub deferral and shift to signal-path audit.
