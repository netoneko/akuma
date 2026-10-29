# PLAN-pn — Taoxichuan stages 9–11, session pn: the syscalls `rustc`/`cargo` need and the order to enable them

Plan for the supervisor to execute. Taoxichuan must go from "tcc compiles C" to
"`rustc hello.rs`, then `cargo build`, then the fixed point". This plan classifies
every needed syscall/behaviour into one route — **A** already a family crate under
`crates/akuma-syscalls-*` (call the pure decision, keep effects local), **B** lives in
`akuma-syscalls-glue`/`src/syscall/` but not yet a family crate (extract: pure part to a new
`akuma-syscalls-<family>` crate with host tests, effects stay at each caller), **C** only in
`amd64/src/` (extract into a crate first, then pull), **D** nowhere (write new, in a crate).
The standing rule holds: pull from a crate; extract from `amd64/src/` only what lives only
there; write new only when it exists nowhere. Planning only — no kernel code, no boots.
Branch `rust-org-il-experiment`, push `litter` only.

**Executive answer (grows as sections land).** The measured census stops at the first
`clone` (the ctrlc thread). Everything before it is slab-size + no-op-arm work:
`sigaltstack`, `membarrier`, `readlink`(89), a real `mprotect`, and the 1 GiB slab are
**INDEPENDENT** of session pm's process-slot switch and are the first order of work.
Everything at and after the first `clone` — thread `clone` groups, `futex`,
`set_robust_list`/`rseq`, `tkill`/`tgkill` + real signal dispositions, `getpriority`/
`sched_getaffinity`/`prlimit64`/`getrusage`, `close_range`, `socketpair`+`sendmsg`/
`recvmsg` — is **AFTER-PM** (needs `akuma-exec`'s process model, which pm is landing) and
is served by crates that already exist (`akuma-syscalls-sync`, `akuma-threading`,
`akuma-syscalls-glue`), extracted where the pure part still lives only in the glue or in
`amd64/src/`.

## 0. Premises (HOLDS / WRONG / UNCHECKED per row)

| # | premise | state | evidence |
|---|---|---|---|
| P1 | Taoxichuan serves only the `*at` forms (openat 257, mkdirat 258, unlinkat 263, renameat 264, symlinkat 266, readlinkat 267); legacy 83/82/87/84/88/89/90/92 have no arms | HOLDS | `taoxichuan/docs/reference/syscalls.md` table — no legacy arm rows |
| P2 | The mmap slab is a fixed 64 MiB (`SLAB`/`MMAP_SLAB` in `taoxichuan/src/main.rs`) and brk shares the constant; `rustc`'s 312 MB `librustc_driver` load ENOMEMs under it | UNCHECKED → read main.rs consts | supervisor's measured ENOMEM at 64 MiB, loads at 1 GiB |
| P3 | `clone` on Taoxichuan is the M2 fork arm (SIGCHLD\[|VM\|VFORK] only, else EINVAL); there are no threads (gettid=1) | HOLDS | syscalls.md row 56; measured EPERM→musl EAGAIN on ctrlc thread |
| P4 | futex has no arm at all (pure gap) | HOLDS | syscalls.md table + the ENOSYS tail list |
| P5 | Signals: `rt_sigaction`/`rt_sigprocmask` accepted no-ops, no delivery, no tkill/tgkill arms | HOLDS | syscalls.md rows 13/14 + gaps list |
| P6 | `mprotect` accepted-and-ignored → W^X never changes → `tcc -run` faults; a real mprotect with PROT_EXEC is needed | HOLDS | syscalls.md row 10; supervisor's run |
| P7 | mmap is anon + MAP_PRIVATE file fill only, no CoW, no writable MAP_SHARED file mapping (linker output) | HOLDS | syscalls.md row 9; archive's zero-byte `rust-lld` failure |
| P8 | `taoxichuan/docs/reference/rust-toolchain.md` scores the toolchain list low, omits threads, and its tables are an inventory not a measurement — headline needs correcting this session | HOLDS | brief; README says "7/10, AF_UNIX is the stage-9 blocker" |
| P9 | pm's switch to the standard process slots has landed far enough that `fork_process` (akuma-exec) is the fork path and init can be a registered Process; thread/futex rows depend on that landing | HOLDS (F1, PK1, P1–P5 green; PK2 open) | README status table; syscalls.md rows 57–59 |
| P10 | The `akuma-syscalls-*` family crates (sync/mem/time/poll/net/eventfd/ipc/abi/linux/log) exist with host tests and are pull-able pure decisions | HOLDS | `ls crates/` this session |
| P11 | `akuma-syscalls-glue` needs `akuma-exec`'s process model; folded glue arms (`to_aarch64()`) are the amd64 route | UNCHECKED → read glue + pm's switch-to-crates.md | brief; CLAUDE.md family paragraph |
| P12 | amd64 runs `rustc` and `cargo` today via `amd64/src/` glue over the same crates; the syscall inventory needed is documented in the archive | HOLDS | archive RUST_TOOLCHAIN*.md set exists |
