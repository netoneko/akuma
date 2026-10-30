# Taoxichuan: copy-on-write fork, and a gate that proves nothing leaks (2026-10-30)

## Why
`fork_share_memory` copied the parent's whole user half eagerly. A forked `rustc` copied a 312 MB mapped
`librustc_driver.so`, which is why `cargo build -j4` needed an 8 GiB guest (a 3 GB one ran out of memory).
`taoxichuan/docs/reference/memory.md` said CoW was impossible because "this kernel has no page-fault handler".
glm's planning session (`TAOXICHUAN_COW_FORK_PLAN_GLM.md`, goose-glm on the Mac) found that premise already false in the code:
`fault_trap` resumes after `grow_user_stack`. The plan's premise table, slices and risks were the starting point; the
user's open questions were answered by default (both fork paths, exact-zero leak bound, prints under `trace-err`,
marker bit local to `main.rs`).

## What changed (`taoxichuan/src/main.rs`)
- `clone_user_tree` shares leaf frames (`cow_ref_inc`), demotes writable leaves to RO + PTE bit 9 in both tables, `invlpg`s the
  parent's; `MAP_SHARED` writable spans (`in_shared_span`) are still copied. A failed clone frees its partial tree (`free_subtree`).
- `cow_resolve` is the single break path (`akuma_cow::decide`), called from `fault_trap`'s new present+write+user arm,
  `copy_to_user` (`cow_prepare_write`: the kernel's physmap writes never fault) and `mprotect` (`cow_privatize`).
- `arch_prctl(ARCH_GET_FS)` wrote user memory by hand through the physmap; it goes through `copy_to_user` now.
- The `[reap]`/`[spawn-reap]` lines (`trace-err`) carry `cowrefs=` and `cow=(shared, copied, taken, retried)`.

## Verification (local QEMU TCG, `--features no-tests,trace-err`)
- Isolation: `sh -c 'x=parent; (x=child; echo in-sub=$x); echo after=$x'` -> `in-sub=child`, `after=parent`.
- `scripts/kvm/goose/cow_leak_gate.py --cycles 20` (60 fork/exec/exit rounds): `base=654000 end=654000 delta=0 PASS`,
  `cowrefs=0`, shared=15520 copied=320 taken=320.
- Battery (`pl.img`): quick execs, F1, PK1, fs, `apk add git` (same "9 errors" as before CoW: unserved `mount` 165/166), `git --version`
  identical to the pre-CoW output. `rs.img`: `cargo build; cargo run; cargo build --release` in one boot; after each, free pages back to
  654000 and `cowrefs=0` (223948 frames shared instead of copied by the third build).
- clippy clean with and without `trace-err`; production build compiles; `cargo test -p akuma-cow` 8/8.

## Still open
- Not measured on the ryzen/KVM: the `-j4` crates.io build in a 3 GB guest (the point of the change); `taoxichuan_pop.sh` still defaults to 8 GiB.
- Kernel writes into a shared read-only **unmarked** page are not intercepted (no caller does that).
- vfork-style `clone(CLONE_VM)` and threads were not re-audited beyond the battery; they share a root and never run the share loop.
- `COW_REFCOUNTS` is a `BTreeMap`: a fork of a 100k-page image allocates ~100k heap nodes until the frames are released.
