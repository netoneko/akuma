# Go `compile` crash — handoff theory (errno-shaped faults)

This note summarizes evidence from **`crash9.log`** (`go tool compile`), **`crash12.log`** (`forktest` / mmap stress vs `GODEBUG=asyncpreemptoff`), userspace stack traces, and the xattr syscall stub. It is intended for another agent to turn into a fix plan and tests.

## Symptoms (userspace)

When running plain **`go build .`** (e.g. tiny module under `/tmp/hello`) with **no `GODEBUG` overrides**, **`go tool compile`** can die with:

- **`unexpected fault address 0xffffffffffffffc0`**
- **`fatal error: fault`** / **`SIGSEGV`**
- **`pc`** in **`bytes.(*Buffer).Write`** → **`fmt.Fprintf`** → **`cmd/compile/internal/ir.MethodSymSuffix`** → **`reflectdata.WriteRuntimeTypes`** (compiler emitting runtime type data)

As a **signed** fault address, **`0xffffffffffffffc0`** is **−64** (errno **`ENONET`** on Linux). As an unsigned pointer bit-pattern it is not a valid user VA.

## What the kernel log proves (`crash9.log`)

For **`pid=169`** (`/usr/lib/go/pkg/tool/linux_arm64/compile`), around **T131.85**:

- **`[DP] no lazy region for FAR=0xffffffffffffffc0`** — demand paging cannot satisfy a fault at that VA.
- **`[WILD-DA]`**: **`FAR=0xffffffffffffffc0`** (**−64**), **`ELR=0x1013273c`** (matches userspace **`pc`**), **`last_sc=18446744073709551615`** (`!0` — “no active syscall” in the global marker sense).
- Register dump at fault: **`x0=0xffffffffffffffa0`** (**−96** as a signed interpretation of the bit pattern).

Immediately **before** **`SIGSEGV` delivery** (~T132.03), the **per-process syscall ring buffer** for the same workload ends with:

```text
131850872    5       8  18446744073709551520
```

**`18446744073709551520`** = **`0xffffffffffffffa0`** = AArch64 syscall return encoding for **−96** (i.e. **negative errno 96**, **`EPFNOSUPPORT`** in Linux).

So the last logged syscall return before the fault is **errno-shaped (−96)**, not a success.

On **Linux aarch64**, **`nr == 5`** in the **asm-generic** numbering used by glibc/musl callers maps to the **setxattr** family (extended attributes). Akuma routes **`syscall_num` 5–16** through one stub (see below).

## Kernel code path (xattr — fixed in tree)

Extended-attribute syscalls **5–16** historically used **`(!95i64) as u64`**, which encodes **−96** (`0xffffffa0`), not **−95** (`0xffffffa9`). That **exactly matched** **`crash9.log`** syscall returns and **`x0`** at the **`compile`** fault site.

**Resolution:** the stub now uses **`neg_errno(95)`**:

```779:786:src/syscall/mod.rs
        // Extended attributes syscalls (5-16) - return EOPNOTSUPP (95) on Linux
        // AArch64. Must be encoded as `x0 = -95` (0xffffffa9), never `!95`
        // which is `-96` (0xffffffa0 = EPFNOSUPPORT) and breaks musl/Go callers.
        5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 => {
            // setxattr, lsetxattr, fsetxattr, getxattr, lgetxattr, fgetxattr
            // listxattr, llistxattr, flistxattr, removexattr, lremovexattr, fremovexattr
            neg_errno(95)
        }
```

**Hypothesis (for `crash9`):** Musl/Go interpret **`errno`** from **`x0`**. Wrong encoding breaks libc invariants and can surface later as **`bytes.Buffer`** faults with errno-shaped addresses.

Regression coverage: **`test_syscall_errno_compliance`** in [`src/process_tests.rs`](../src/process_tests.rs) asserts **`setxattr` / `lsetxattr` / `fremovexattr`** return **−95**, not **−96**.

## Forktest / mmap stress vs async preempt (`crash12.log`)

Serial was split at each **`[exit_group] … forktest_parent`** boundary. Two early slices used **`GODEBUG=asyncpreemptoff=1`**; the **last** slice used **default Go** (no `GODEBUG`, async preempt on).

| Condition | Role in log | `[EINVAL] nr=113` (`clock_gettime`) | `sig=23` (SIGURG) | `[JIT] IC flush` | `WILD-DA` |
|-----------|-------------|-------------------------------------|-------------------|------------------|-----------|
| **`asyncpreemptoff=1`** | Earlier runs (forktest parents **90**, **112**) | **0** | **0** | **4** | **6** |
| **Default preempt** | Last run (parent **134**) | **2** | **25** | **2** | **8** |

**Conclusion:** The **`compile`** / xattr failure was a **deterministic errno-encoding bug** in the stub (above). **`forktest` + mmap stress** is a **different** path but **correlates with async preempt** on Akuma: **`asyncpreemptoff=1`** removes **`nr=113`** and SIGURG bursts in those slices; **default preempt** reproduces **`clock_gettime` EINVAL**, SIGURG spam, and more **`WILD-DA`**. Use **`GODEBUG=asyncpreemptoff=1`** as an **isolation knob** while debugging signal/syscall interaction; see **[GO_FORKTEST_DEBUG.md](GO_FORKTEST_DEBUG.md)**.

## Relationship to other “errno as pointer” bugs

This ties into the same family as **[GO_FORKTEST_DEBUG.md](GO_FORKTEST_DEBUG.md)** (**`WILD-DA`**, **`clock_gettime` / EINVAL**, small negative FARs) and **[SYSCALL_ERRNO_COMPLIANCE_CHANGES.md](SYSCALL_ERRNO_COMPLIANCE_CHANGES.md)** — syscall returns must be **bit-accurate** vs Linux so userspace never mistakes **`-(errno)`** for a pointer or length.

## Suggested follow-ups

1. Re-run **`go build .`** on a minimal module and confirm serial no longer shows **`compile`** faulting after **`nr=5`** with **`0xffffffa0`** (**`crash9`** scenario).
2. Optional: grep the tree for **`(!`** `i64)` **`as u64`** errno tricks and replace with **`neg_errno`**.
3. For **`forktest`** / preempt-on failures, trace **`clock_gettime`** **`EINVAL`** and SIGURG paths per **`GO_FORKTEST_DEBUG.md`** (kernel + runtime).

## Follow-ups (tooling / UX — not kernel)

- **`neko` (editor):** Does not track or apply **current working directory** the way a normal shell session does — easy to edit or save the **wrong file** when paths are relative or duplicated across dirs. **Bug fix:** teach `neko` cwd semantics (or make cwd explicit in the UI).
- **Pipes:** Shell pipelines **do not carry cwd** per stage — only the starting process’s cwd applies unless each stage does its own `cd`. Misleading when debugging “file exists” vs “command failed.” **Bug fix:** document in tooling; optionally wrappers that set cwd per pipeline segment.
- **In-kernel shell (`akuma-shell`):** **`;`** and **`&&`** between commands **are** parsed ([`parse_command_chain`](../../crates/akuma-shell/src/parse.rs)). A lone **`&`** (background job) is **not** implemented — do not rely on it. For **non-interactive SSH one-liners**, prefer **`env KEY=val cmd …`** so vars apply without `export …; …`, or **`go build -o /tmp/hello/hello /tmp/hello`** (package path) to avoid **`cd`** / chaining quirks entirely.

## References

- [`src/syscall/mod.rs`](../src/syscall/mod.rs) — `handle_syscall`, **`neg_errno`**, xattr stub **`5 | … | 16`**
- [`docs/GO_FORKTEST_DEBUG.md`](GO_FORKTEST_DEBUG.md) — errno-as-pointer, **`clock_gettime`**, SIGURG / preempt
- [`docs/GOLANG_MISSING_SYSCALLS.md`](GOLANG_MISSING_SYSCALLS.md) — broader Go-on-Akuma syscall history
- [`docs/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`](SYSCALL_ERRNO_COMPLIANCE_CHANGES.md) — **`neg_errno`** motivation
