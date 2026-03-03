# Workflow Optimizations

Proposed changes to the Akuma development workflow, ordered by expected impact.

---

## 1. Automated Smoke Tests

**Problem:** Regressions are caught manually. Several docs describe fixing one thing and breaking another — DHCP deconfiguring during loopback tests ([DHCP_LOOPBACK_TEST_FIX](../docs/DHCP_LOOPBACK_TEST_FIX.md)), Embassy removal breaking SSH ([EMBASSY_REMOVAL](../docs/EMBASSY_REMOVAL.md)), smoltcp migration breaking external connectivity ([VIRTIO_RECEIVE_FIX](../docs/VIRTIO_RECEIVE_FIX.md)).

**Proposal:** A `scripts/smoke_test.sh` that boots QEMU, waits for SSH, runs a checklist, and reports pass/fail.

```bash
#!/bin/bash
set -e

TIMEOUT=60
PASS=0
FAIL=0

# Boot QEMU in background
scripts/run.sh &
QEMU_PID=$!
trap "kill $QEMU_PID 2>/dev/null" EXIT

# Wait for SSH
for i in $(seq 1 $TIMEOUT); do
    ssh -q -o ConnectTimeout=1 -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null user@localhost -p 2222 \
        "echo ready" 2>/dev/null && break
    sleep 1
done

run_test() {
    local name="$1"; shift
    if ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        user@localhost -p 2222 "$@" 2>/dev/null; then
        echo "  PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name"
        FAIL=$((FAIL + 1))
    fi
}

echo "=== Smoke Tests ==="
run_test "filesystem"       "ls / > /dev/null"
run_test "pipe"             "echo hello | grep hello"
run_test "process spawn"    "ps | grep -q herd"
run_test "network loopback" "curl -s http://127.0.0.1:8080/ > /dev/null"
run_test "meow one-shot"    "timeout 30 meow -m 'say hi' 2>/dev/null"
# Add more as subsystems stabilize

echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ]
```

Run before every commit that touches kernel code. Not CI — just a local script with a 60-second feedback loop.

**Effort:** Small. Most of the infrastructure (`run.sh`, SSH) already exists.

---

## 2. Batched Syscall Discovery

**Problem:** Bringing up a new binary (bun, xbps, apk, curl, git) follows a slow loop: run → crash on missing syscall → implement → repeat. Each cycle requires a rebuild and QEMU restart. The `*_MISSING_SYSCALLS` docs ([BUN_MISSING_SYSCALLS](../docs/BUN_MISSING_SYSCALLS.md), [XBPS_MISSING_SYSCALLS](../docs/XBPS_MISSING_SYSCALLS.md), [APK_MISSING_SYSCALLS](../docs/APK_MISSING_SYSCALLS.md), [CURL_MISSING_SYSCALLS](../docs/CURL_MISSING_SYSCALLS.md), [GIT_MISSING_SYSCALLS](../docs/GIT_MISSING_SYSCALLS.md)) all document this pattern.

**Current state:** The unknown syscall handler in `src/syscall.rs` already logs and returns ENOSYS without killing the process:

```rust
_ => {
    safe_print!(128, "[syscall] Unknown syscall: {} (args: [0x{:x}, ...])\n",
        syscall_num, args[0], ...);
    ENOSYS
}
```

This is good — the process isn't killed. But many programs abort on the first ENOSYS (e.g., glibc/musl wrappers that get -ENOSYS back may call `abort()`), so in practice you often only see one unknown syscall per run.

**Proposal:** Add a `SYSCALL_SURVEY_MODE` config flag in `src/config.rs` that, when enabled:

1. Logs unknown syscalls as today (already done).
2. For syscalls that are known to be safe to stub (file metadata, scheduling hints, memory advisories), returns 0 instead of ENOSYS. A whitelist:
   ```rust
   const SAFE_TO_STUB: &[u64] = &[
       nr::MADVISE, nr::MEMBARRIER, nr::SCHED_GETAFFINITY,
       nr::SCHED_SETAFFINITY, nr::SCHED_YIELD, nr::FLOCK,
       nr::UMASK, nr::FDATASYNC, nr::FSYNC, nr::FCHMOD,
       nr::FCHOWN, nr::UTIMENSAT, nr::MSYNC,
   ];
   ```
3. For others, still returns ENOSYS but increments a counter and continues.
4. On process exit, prints a summary: "Process used N unknown syscalls: [list with call counts]".

This turns 15 edit-build-boot-crash cycles into 1-2 runs to get the full syscall surface.

**Effort:** Small — ~30 lines in `syscall.rs`, one new flag in `config.rs`.

---

## 3. Invariants Section in CLAUDE.md

**Problem:** Several classes of bugs recurred because the same invariant was violated in different code paths. These invariants are documented across individual fix docs but not in the central context file that the AI reads.

Recurring violations found in docs:

| Invariant | Violated in | Docs |
|-----------|------------|------|
| Spinlocks must be held with IRQs disabled | `talc_alloc` (alloc protected, dealloc not) | [FAR_0x5_AND_HEAP_CORRUPTION_FIX](../docs/FAR_0x5_AND_HEAP_CORRUPTION_FIX.md) |
| Never use current TTBR0 for new threads | `spawn()` copied current TTBR0 | [TTBR0_AND_THREADING_FIXES](../docs/TTBR0_AND_THREADING_FIXES.md) |
| Never clean up state another thread may be using | Cleanup freed INITIALIZING slots | [INITIALIZING_RACE_CONDITION_FIX](../docs/INITIALIZING_RACE_CONDITION_FIX.md) |
| Never call blocking ops with preemption disabled | `yield_now()` inside `with_socket_handle` | [SENDTO_PREEMPTION_FIX](../docs/SENDTO_PREEMPTION_FIX.md) |
| Single source of truth for thread state | Duplicate state in THREAD_STATES and slots | [TTBR0_AND_THREADING_FIXES](../docs/TTBR0_AND_THREADING_FIXES.md) |
| Mask ASID when using TTBR0 as physical address | Raw TTBR0 used as phys addr | [UNIFIED_PROCESS_ABI_IMPLEMENTATION_ISSUES](../docs/UNIFIED_PROCESS_ABI_IMPLEMENTATION_ISSUES.md) |
| Check TTBR0 before reading ProcessInfo at 0x1000 | `read_current_pid()` read under boot tables | [FAR_0x5_AND_HEAP_CORRUPTION_FIX](../docs/FAR_0x5_AND_HEAP_CORRUPTION_FIX.md) |
| Permission faults (DFSC=0x0C) are not translation faults | Treated as demand-paging candidates | [BUN_MEMORY_STUDY](../docs/BUN_MEMORY_STUDY.md) |
| Re-enable preemption during sync I/O in async contexts | Async tasks did sync I/O with preemption disabled | [NETWORKING_DEADLOCK_INVESTIGATION](../docs/NETWORKING_DEADLOCK_INVESTIGATION.md) |

**Proposal:** Add an `## Invariants` section to `CLAUDE.md` containing these rules. When a new bug is fixed and the root cause is an invariant violation, add it to the list. This costs nothing and prevents the AI from generating code that repeats past mistakes.

**Effort:** Trivial — copy the table above into `CLAUDE.md` and maintain it.

---

## 4. Debug Assertions for Critical Invariants

**Problem:** The invariants above were discovered through crashes. Many could be caught at the point of violation rather than at the point of symptom (which is often far removed).

**Current state:** The codebase has ~165 assertions (mix of `assert!`, `debug_assert!`, canary checks, magic value checks), mostly in the allocator and threading. There are none for the most commonly violated invariants.

**Proposal:** Add `debug_assert!`-style checks gated on a `DEBUG_INVARIANTS` flag in `config.rs`. These compile to nothing in release unless the flag is set. Target the highest-value checks first:

```rust
// In every spinlock acquire:
debug_invariant!(!irqs_enabled(), "spinlock acquired with IRQs enabled");

// In spawn():
debug_invariant!(
    is_boot_ttbr0(read_ttbr0()),
    "spawn() called with non-boot TTBR0"
);

// In demand paging handler:
debug_invariant!(
    dfsc != 0x0C && dfsc != 0x0D && dfsc != 0x0E && dfsc != 0x0F,
    "permission fault treated as translation fault"
);

// In read_current_pid():
debug_invariant!(
    !is_boot_ttbr0(read_ttbr0()),
    "read_current_pid() called with boot TTBR0"
);
```

**Effort:** Medium — define the macro, add checks at ~10 call sites. The checks themselves are cheap (register reads, flag tests).

---

## 5. Investigation Doc Structure

**Problem:** Investigation docs (e.g. [HEAP_CORRUPTION_INVESTIGATION](../docs/HEAP_CORRUPTION_INVESTIGATION.md), [NETWORKING_DEADLOCK_INVESTIGATION](../docs/NETWORKING_DEADLOCK_INVESTIGATION.md)) preserve every hypothesis, dead end, and intermediate attempt. This is valuable as a log but expensive to re-read — both for humans and for AI context windows. When these docs are fed as context for a new session, the AI spends tokens on wrong guesses from past sessions.

**Proposal:** Add a `## Resolution` section at the top of each investigation doc with:

```markdown
## Resolution

**Status:** Fixed (date)
**Root cause:** One sentence.
**Fix:** One sentence + file references.
**Invariant added:** (if applicable)
```

The investigation body stays intact below for posterity. The resolution section gives future readers (and AI) the answer in 4 lines instead of requiring them to read 400 lines to find it.

Retroactively add resolutions to the ~15 investigation docs. Going forward, start every new investigation doc with an empty resolution template and fill it in when the bug is fixed.

**Effort:** Small — a few hours of retroactive work, seconds per new doc.

---

## 6. Disk Image Hygiene

**Problem:** The repo root contains manual disk image snapshots: `disk.img.backup`, `disk.img_feb12`, `disk.img_feb25_broken`, `disk.img_jan31`. These are untracked, unnamed, and accumulate.

**Proposal:**

1. Add `disk.img*` to `.gitignore` if not already there.
2. Add a `scripts/snapshot_disk.sh` and `scripts/restore_disk.sh`:
   ```bash
   # snapshot_disk.sh
   #!/bin/bash
   NAME="${1:-$(date +%Y%m%d_%H%M%S)}"
   cp disk.img "snapshots/disk_${NAME}.img"
   echo "Saved snapshots/disk_${NAME}.img"
   ```
3. Store snapshots in a `snapshots/` directory (gitignored).
4. Delete the four stale images from the repo root.

**Effort:** Trivial.

---

## Summary

| # | Change | Impact | Effort |
|---|--------|--------|--------|
| 1 | Smoke test script | Catches regressions before they compound | Small |
| 2 | Syscall survey mode | Batch-discover missing syscalls in 1 run | Small |
| 3 | Invariants in CLAUDE.md | Prevents AI from repeating past mistakes | Trivial |
| 4 | Debug assertions | Catches invariant violations at the source | Medium |
| 5 | Investigation doc resolutions | Faster context loading for humans and AI | Small |
| 6 | Disk image scripts | Cleaner repo, reproducible snapshots | Trivial |

Recommendations 1-3 can be done in an afternoon and have immediate payoff. Recommendation 4 is an ongoing investment. Recommendations 5-6 are housekeeping.
