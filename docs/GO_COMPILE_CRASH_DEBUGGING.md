# Go `compile` crash — handoff theory (errno-shaped faults)

This note summarizes evidence from **`crash9.log`**, userspace stack traces, and the kernel syscall stub for **extended-attribute** syscalls. It is intended for another agent to turn into a fix plan and tests.

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

## Kernel code path (actionable)

In [`src/syscall/mod.rs`](../src/syscall/mod.rs), extended-attribute syscalls are handled as:

```779:785:src/syscall/mod.rs
        // Extended attributes syscalls (5-16) - return ENOTSUP (not supported on this fs)
        5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 => {
            // setxattr, lsetxattr, fsetxattr, getxattr, ...
            const ENOTSUP: u64 = (!95i64) as u64; // Operation not supported
            ENOTSUP
        },
```

**Problem:** **`(!95i64) as u64` is not `-(95)`**. Bitwise **`!`** on **`95`** yields **−96** in two’s complement, so this stub returns **−96** (**`0xffffffa0`**), **not** **−95** (**`0xffffffa9`**, Linux **`EOPNOTSUPP`** / commonly used for “operation not supported on socket” — and the intended **`ENOTSUP`**-class value should use [`neg_errno(95)`](../src/syscall/mod.rs) per project convention).

This **exactly matches** the logged return **`0xffffffffffffffa0`** and **`x0`** at the fault site.

**Hypothesis (primary):** Musl/Go treat **`setxattr`** failure by checking **`errno`**. Returning the **wrong negative errno** (off-by-one in the encoded value) can violate libc invariants and surface as **corrupted Go heap/slice metadata**, later faulting in **`bytes.Buffer`** with addresses that look like small negative integers.

**Secondary:** **`FAR = −64`** may still be a **downstream** artifact (bad pointer arithmetic / slice header) rather than a second distinct kernel bug; prove or disprove with a fixed **`neg_errno(95)`** (or the correct Linux errno for “xattr not supported” on this ABI) and re-run **`go build`**.

## Relationship to other “errno as pointer” bugs

This ties into the same family as **[GO_FORKTEST_DEBUG.md](GO_FORKTEST_DEBUG.md)** (**`WILD-DA`**, **`clock_gettime` / EINVAL**, small negative FARs) and **[SYSCALL_ERRNO_COMPLIANCE_CHANGES.md](SYSCALL_ERRNO_COMPLIANCE_CHANGES.md)** — syscall returns must be **bit-accurate** vs Linux so userspace never mistakes **`-(errno)`** for a pointer or length.

Forktest / **`SIGURG`** / **JIT IC flush** tracks (**Bucket A** in earlier triage) may still matter for **other** crashes; this **`compile`** failure has a **clean, log-aligned** kernel stub bug independent of preemption.

## Suggested work for the next agent

1. **Fix** the xattr stub to use **`neg_errno(95)`** (or the errno number you confirm matches Linux “operation not supported” for xattr on musl — verify against **`errno(3)`** / **`musl` `bits/errno.h`**), **never** **`(!errno)`**.
2. Add a **kernel regression test** (see existing **`test_syscall_errno_compliance`** in [`src/process_tests.rs`](../src/process_tests.rs)): **`setxattr`** (nr **5**) returns **`-(95)`** bit pattern, **not** **`0xffffffa0`** unless errno **96** is intentionally required (it should not be here).
3. Re-run **`go build .`** on a minimal module and confirm **`crash9.log`** no longer shows **`WILD-DA`** at **`0xffffffc0`** for **`compile`** after **`nr=5`**.
4. Optional: grep the tree for **`(!`** `i64)` **`as u64`** errno tricks and replace with **`neg_errno`**.

## Follow-ups (tooling / UX — not kernel)

- **`neko` (editor):** Does not track or apply **current working directory** the way a normal shell session does — easy to edit or save the **wrong file** when paths are relative or duplicated across dirs. **Bug fix:** teach `neko` cwd semantics (or make cwd explicit in the UI).
- **Pipes:** Shell pipelines **do not carry cwd** per stage — only the starting process’s cwd applies unless each stage does its own `cd`. Misleading when debugging “file exists” vs “command failed.” **Bug fix:** document in tooling; optionally wrappers that set cwd per pipeline segment.
- **In-kernel shell (`akuma-shell`):** **`;`** and **`&&`** between commands **are** parsed ([`parse_command_chain`](../../crates/akuma-shell/src/parse.rs)). A lone **`&`** (background job) is **not** implemented — do not rely on it. For **non-interactive SSH one-liners**, prefer **`env KEY=val cmd …`** so vars apply without `export …; …`, or **`go build -o /tmp/hello/hello /tmp/hello`** (package path) to avoid **`cd`** / chaining quirks entirely.

## References

- [`src/syscall/mod.rs`](../src/syscall/mod.rs) — `handle_syscall`, **`neg_errno`**, xattr stub **`5 | … | 16`**
- [`docs/GOLANG_MISSING_SYSCALLS.md`](GOLANG_MISSING_SYSCALLS.md) — broader Go-on-Akuma syscall history
- [`docs/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`](SYSCALL_ERRNO_COMPLIANCE_CHANGES.md) — **`neg_errno`** motivation
