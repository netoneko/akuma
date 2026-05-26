# Signal Hell — Failure Summary (trim0.log)

Captured from a boot run. All failures below need to be fixed before acceptance testing is meaningful.

---

## 1. Thread-group kill / exit_group (6 failures)

These all point to threads in a group not observing kill/exit signals sent to the group leader or via `exit_group`.

```
kill_thread_group_terminates_before_cleanup
  s1=0 s2=0 s3=0 — expected TERMINATED=3
  Sibling threads are not being terminated when the group is killed.

exit_group_kills_siblings_before_close_all
  s1=0 s2=0 leader=0
  exit_group is not propagating termination to siblings before fd cleanup.

kill_thread_group_clears_thread_id
  sib_exists=false sib_state=0 leader_tid=Some(Some(128))
  Leader TID not cleared after group kill; sibling state incorrect.

zombie_process_unregistered_after_return_to_kernel
  reg=true exited=false dropped=true gone=true
  Process stays registered (reg=true) even after returning to kernel; exit flag not set.

sys_kill_sets_interrupted_flag
  not interrupted
  Sending a signal to a sleeping thread via sys_kill does not set the interrupted flag.

goroutine_kill_does_not_kill_leader
  alive=false name=false !exited=false sib_gone=false
  Killing a goroutine-style thread kills the leader when it should not; or leader state is wrong.
```

---

## 2. Pending signal bitmask (4 failures) — FIXED 2026-05-26

**Status: RESOLVED.** All four now PASS (confirmed on a real boot run, trim2.log).

```
pending_signal_bitmask_multiple
  first=15 taken=None second=15
  Signal 15 is pending but take() returns None.

pending_signal_take_clears_one
  None None None None
  take() returns None for all four queued signals.

pending_signal_mask_blocks
  taken=Some(2)
  A masked signal (2/SIGINT) is being delivered despite the mask.

pend_signal_or_semantics
  has_15=true taken=None has_23=false
  Signal 15 shows as pending but cannot be taken; signal 23 not recorded at all.
```

**The original root-cause hypothesis (split/non-atomic bitmask, off-by-one
numbering) was WRONG.** The kernel implementation in
`crates/akuma-exec/src/threading/mod.rs` (`pend_signal_for_thread`,
`peek_pending_signal`, `take_pending_signal`) is correct. This was a **test
bug**, not a kernel regression.

**Actual root cause:** `take_pending_signal(mask)` treats `mask` as the set of
*blocked* signals — `deliverable = pending & (!mask | force_bits)`. So:
- `0u64`  = block nothing = "take any pending signal"
- `!0u64` = block *everything* = nothing deliverable except SIGKILL/SIGSTOP

The four failing tests in `src/process_tests.rs` called
`take_pending_signal(!0u64)` while *intending* "take any signal." Every take
returned `None`, and the un-taken bits then leaked into the next test — which
is why `mask_blocks` reported the nonsensical `taken=Some(2)` (a leftover
SIGINT pended by the preceding `take_clears_one` test).

Three independent proofs the impl is right and the tests were wrong:
1. `src/syscall/signal.rs:227` documents *"take_pending_signal takes a mask of
   BLOCKED signals"* and passes `!wait_mask`.
2. `src/tests.rs:8285` — a sibling test that already PASSES — uses
   `take_pending_signal(0u64)` with the same primitives.
3. `test_sigkill_bypasses_mask` deliberately passes `!0u64` to verify SIGKILL
   survives a full mask, confirming `!0u64` means "block all."

**Why the doc said these "were passing at some point":** commit `230ddfa`
("attempt to fix signals") migrated the single-slot `AtomicU32` to the
`AtomicU64` bitmask and, in the same commit, *replaced* the old
`test_pending_signal_is_single_slot` (which passed) with these new tests —
already calling `!0u64`. So these specific tests never passed post-migration;
the passing test they replaced is gone.

**Fix:** changed `!0u64` → `0u64` in the four tests (`bitmask_multiple`,
`take_clears_one`, `mask_blocks` cleanup, `or_semantics`) in
`src/process_tests.rs`. `test_sigkill_bypasses_mask` left untouched.

**Note on `current_thread_id()`:** a red herring for these failures. It is a
one-line alias that already calls the identical `get_current_thread_register()`
(`tpidrro_el0`) used internally by `take_pending_signal`, so pend and take
always hit the same slot. Deprecating it would fix nothing here.

**Implication for Cluster 1:** the original suggestion was that the
thread-group kill tests would "self-resolve once signals deliver correctly."
Since the signal primitives were never actually broken, they will NOT
self-resolve — confirmed by trim2.log, where all 6 still fail. Treat Cluster 1
as an independent bug.

---

## 3. STP instruction decoder — `stp_xzr_misroute_decode` FIXED 2026-05-26

**Status of `stp_xzr_misroute_decode`: RESOLVED** (test-only fix). The original
framing below ("decoder computing wrong offsets / rejecting valid encodings")
was WRONG. `decode_stp_xzr_xzr` in `src/exceptions.rs:719` is correct per the
ARM64 STP signed-offset encoding (imm7 in bits[21:15], Rt in bits[4:0], proper
sign-extension). This was a **test bug**: 5 of the 7 hand-assembled instruction
words in `src/process_tests.rs` did not encode the instruction their label
claimed. The 2 well-formed cases (`[x0]`, `[x0,#0x10]`) always passed, which
already proved the decoder logic.

```
stp_xzr_misroute_decode (original failure log)
  stp xzr,xzr,[x0,#0x70]  instr=0xa90e7c1f  got=Some((0,224))  want=(0,112)
  stp xzr,xzr,[x3,#0x20]  instr=0xa9027c63  got=None           want=(3,32)
  stp xzr,xzr,[x0,#-0x8]  instr=0xa97f7c1f  got=None           want=(0,-8)
  stp xzr,xzr,[x0,#-0x10] instr=0xa97e7c1f  got=None           want=(0,-16)
  stp xzr,xzr,[x0,#-0x200] instr=0xa9407c1f got=None           want=(0,-512)
```

What each malformed word actually was, and the corrected constant:

| label          | old (wrong) word | defect in the word                  | corrected word |
|----------------|------------------|-------------------------------------|----------------|
| `[x0,#0x70]`   | `0xa90e7c1f`     | imm7=28 (→224), should be 14         | `0xa9077c1f`   |
| `[x3,#0x20]`   | `0xa9027c63`     | Rt=x3 in bits[4:0], should be xzr    | `0xa9027c7f`   |
| `[x0,#-0x8]`   | `0xa97f7c1f`     | imm7 placed in bits[22:16] not [21:15] | `0xa93ffc1f` |
| `[x0,#-0x10]`  | `0xa97e7c1f`     | same bit-position error              | `0xa93f7c1f`   |
| `[x0,#-0x200]` | `0xa9407c1f`     | same bit-position error              | `0xa9207c1f`   |

The "offset doubled (224 = 0x70*2)" symptom was a coincidence of the wrong word,
not a scale bug; the decoder never doubles anything. Fixed by replacing the 5
constants in `src/process_tests.rs`. `cargo check` clean.

### `stp_xzr_ec15_handler_fires` — STILL FAILING (separate, real issue)

NOT a decoder bug and NOT fixed. This is a runtime/QEMU concern: QEMU's TCG is
generating EC=0x25 (data abort) for `stp xzr,xzr` on a PROT_NONE page instead of
EC=0x15, so the EC=0x15 emulation path is never exercised. The handler may be
registered under the wrong EC, or the instruction is simply handled by the
EC=0x25 demand-pager before the EC=0x15 path is reached. Needs its own
investigation — left untouched.

```
stp_xzr_ec15_handler_fires
  EC=0x15 STP handler never fired.
  QEMU is generating EC=0x25 for this instruction class instead of EC=0x15.
```

---

## 4. Thread safety

```
fake_thread_ids_safe FAILED: system threads corrupted
  Assigning fake TIDs to user threads is corrupting the system thread table.
  Likely an index bounds issue or the TID namespace for user vs system threads overlaps.
```

---

## 5. Minor / lower priority

```
FS errno mapping
  PermissionDenied → EPERM: got -13, expected -1
  fs_error_to_errno_mapping is returning the raw errno value (-13) instead of -1.
  (Standard Linux syscall convention: return -errno, but test may expect the positive errno
  to be negated at the syscall boundary, not inside the FS layer.)

procfs stdout
  procfs stdout missing expected content (got 0 bytes)
  A procfs read returned empty. Low priority unless something depends on /proc for signals.
```

---

## Suggested attack order

1. ~~**Pending signal bitmask**~~ — DONE (2026-05-26, test-only fix; see Cluster 2). The "underlies most of the kill tests" premise was false; the primitives were never broken.
2. **Thread-group kill / exit_group** — independent bug, does NOT self-resolve (still failing in trim2.log after the signal fix). Needs its own investigation.
3. **fake_thread_ids_safe** — TID namespace overlap, isolated fix.
4. ~~**STP decoder**~~ — `stp_xzr_misroute_decode` DONE (2026-05-26, test-only fix; the decoder was correct, the test words were malformed). `stp_xzr_ec15_handler_fires` remains: real EC 0x25-vs-0x15 runtime issue, separate from the decoder.
5. **FS errno / procfs** — clean up after the above.
