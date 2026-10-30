# Taoxichuan: bugs found 2026-10-30 (unsafe reduction, `-j4` hunt, memory baselines)

Found while cutting `unsafe` in `taoxichuan/src` ([plan status](TAOXICHUAN_UNSAFE_REDUCTION_PLAN_GLM.md)), hunting the
`-j4` wedge ([CoW fork doc](TAOXICHUAN_COW_FORK.md)) and measuring whether the guest can compile its own kernel in 3 GiB.
Rigs: Mac TCG (battery, `cow_leak_gate.py`), ryzen/KVM via `scripts/kvm/taoxichuan_pop.sh` (Pop OS host, guest at
`192.168.1.126:2224`). Status of each entry is stated; nothing here is committed yet.

## 1. `execve` answered EFAULT for large argument blocks — FIXED (build now gets past it)

- **Symptom.** `cargo build -p taoxichuan` inside the guest (3 GiB, `-j4`) died with `Bad address (os error 14)`:
  `[execve] s14 path=/usr/local/rust/bin/rustc` then `[err] s14 nr=59 -14`. Free memory was ~1.1 GiB at that moment, so not memory.
- **Cause: three limits in one function.** `read_spawn_args` (`taoxichuan/src/child.rs`, shared by `spawn`, the real-fork `execve`
  and `exec_replace`) capped `argv` and `envp` at 128 entries each, and each string at 4096 bytes (`STR_MAX`). Exceeding any of them
  returned `None`, which the caller maps to `EFAULT`. Linux answers `E2BIG` for an oversize block, never `EFAULT`. There was also no
  total-size check, so an oversize block reached `ring3::build_stack`'s slice arithmetic and its `MAX_STRS = 512` assert.
- **What cargo actually passes.** Measured on Linux with a `RUSTC_WRAPPER` that logs each call's shape for this same build: the
  heaviest `rustc` call has **344 arguments**, one **5192-byte argument**, ~19.6 KB of arguments, 84 environment variables (Linux host
  environment; the guest's is smaller). The first fix attempt (128 → 256 entries, found by an isolated test) moved the failure from
  ~3m14 to ~4m00 guest time and did not finish the job: the 344-argument and 5192-byte cases were still refused. Reading the cargo
  invocation first would have found all three at once.
- **Reproduced in isolation.** In the guest, `env V0=x … V99=x /bin/true` ran and the same with 200 variables printed
  `env: Bad address`.
- **Fix.** `ARGV_MAX` 448, `ENV_MAX` 256, string cap 32 KiB (read into a `Vec`, not a 4 KiB stack array), `argv + envp <= 512` and a
  total-bytes guard both answering `E2BIG`; the loader's argument block grew from 8 to 12 pages (`ARG_BLOCK_PAGES` in `main.rs`, the stack
  is 16 pages).
- **Why the `-j4` serde/serde_json/fastrand builds never hit it.** Their crates have few dependencies; the argument and environment
  size of a `rustc` call grows with the dependency list, so only a larger crate crosses the caps.
- **Verified:** the isolated 200-variable case failed before the fix. With the final fix the in-guest build executes the `rustc` calls that
  died before and runs on to the failure in section 7. A 400-variable test exceeds the new 256 cap by design and still answers EFAULT
  rather than `E2BIG` (the pointer-array reader maps "too many" to `None`); cosmetic, not changed.

## 2. `-j8` crates.io build fails — UNEXPLAINED

Same serde + serde_json + fastrand project, fresh boot each time.

| Guest RAM | Kernel | Result |
|---|---|---|
| 3072 MiB | working tree (unsafe-reduced) | dies at ~45 s: `mmap` ENOMEM, "memory allocation of N bytes failed", free pages hit 0, then `[Fault] ring3 PageFault … code=0x7` and a #GP |
| 3072 MiB | committed `9f1e333f` | same |
| 8192 MiB | working tree | `rustc` (itoa) dies: `[Fault] ring3 GeneralProtection rip=0x3004274b rsp=0x00006fffffffc8a0` (in the dynamic linker, rsp at the stack top) |

- At 3 GiB it is memory exhaustion: eight rustc jobs need more than 3 GiB, and when a CoW break (`cow_resolve`'s `Copy` arm) cannot get a
  frame it answers `Fault`, so the task is killed with `code=0x7`. That is the designed out-of-frames path, not a CoW logic error.
  A kernel that kills with SIGSEGV where Linux would run its OOM killer is a policy gap, not investigated further.
- The 8 GiB #GP is **not** explained. It is not OOM and not the `execve` cap of section 1 (different symptom). The committed kernel was
  only run at 3 GiB, so whether the 8 GiB crash predates the unsafe-reduction work is **not established**.
- The pre-CoW control (`4428104c`) could not run the script: it lacks the `O_APPEND` fix of `3839b9a7`, so `printf >> Cargo.toml`
  destroys the manifest and the run fails in 4 s. Next step: the same control with a pre-written `Cargo.toml`, at 8 GiB.
- `-j4` is unaffected: 5 of 5 fresh-boot runs at 3 GiB passed on the unsafe-reduced kernel, ~129 s wall each, free pages back to
  baseline and `cowrefs=0` after each, no `BorrowCell` overlap panic.

## 3. The guest wall clock runs about 1.5x fast — OPEN, not investigated

The identical build is "3m 09s" by the guest's `cargo`/`time` and 128-130 s by the host's `date`. Every guest-side duration in the
docs (including the 3m03-3m52 `-j4` figures in the CoW doc) is guest time. The TSC rate is calibrated against the PIT
(`net.rs`); the likely source is the PIT window under KVM. Compare guest figures with each other, not with a host stopwatch.

## 4. The guest sshd corrupts binary stdin — OPEN

Piping a gzip tarball into `ssh -p 2224 root@… 'tar xz'` fails with `gzip: invalid magic`; base64 text piped to `base64 -d | md5sum`
also returned the empty-input digest. `[SSHD] bridge_process: couldn't open stdin for pid N` appears on the console for commands that
get stdin. Commands whose payload rides in the command string work (all the test scripts use that). Workaround used: the guest
clones the repo from GitHub over HTTPS (public repo, 9 s sparse clone). Cause not investigated (sshd's stdin bridge, `userspace/sshd`).

## 5. Smaller findings

- `git` in the guest prints `fatal: setsid failed: Function not implemented` after `git sparse-checkout set`; the clone and checkout
  still succeed. `setsid` (syscall 112) is unserved (`[nosys]` list).
- `TRAMP-MISMATCH tid=N THREAD_PID_MAP=… but table scan found …` is printed repeatedly during the big builds. It says it uses the map's
  value and the builds pass; left as is.
- The `-j4` wedge of the first 3 GiB attempt and one local TCG run (CoW doc) has still not recurred in 5 + 5 clean runs.
- `scripts/kvm/taoxichuan_pop.sh` defaulted to 8192 MiB, which hides leaks; it now defaults to 3072.

## 6. Memory baselines: building the taoxichuan kernel itself at 3 GiB

`cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests -j4`, committed tree `9f1e333f`, cold target dir.

| Where | Limit | Result |
|---|---|---|
| Linux (ryzen Pop OS host, nightly-2026-09-19) | cgroup `MemoryMax=3G`, `MemorySwapMax=0` | 10 s, **peak 447 MiB** (`memory.peak`, includes page cache), 0 OOM kills |
| Taoxichuan guest on KVM (nightly-2026-09-29, musl host toolchain) | 3072 MiB RAM | **does not finish.** Attempt 1: `execve` EFAULT (section 1). Attempt 2 (128 → 256 caps only): same, ~4m00 guest. Attempt 3 (final fix): kernel panic, process table full (section 7) |
| **Akuma amd64** (full kernel, `e5e23d61`, Firecracker/KVM on the ryzen, 4 vCPU, 3072 MiB) | 3072 MiB RAM | **PASS**, `kbuild -c -j4 -p taoxichuan --features no-tests`, 35 s guest clock (~24 s real), **peak 1,356 MiB used** (MemTotal 3,144,704 kB, lowest MemFree 1,756,584 kB, sampled every 2 s from `/proc/meminfo`), idle 531 MiB used before the build, so the build added ~821 MiB |

So the memory comparison for the same compile is: **Linux 447 MiB peak (cgroup, page cache included) vs Akuma 1,356 MiB peak
(821 MiB above its own idle)**: Akuma needs ~3.0x Linux's peak, ~1.8x if only the build's increment over idle is compared. Neither is close to
3 GiB, so 3 GiB is not what stops the taoxichuan guest below. Caveats: the Akuma figure is a 2 s sample (a short spike can be missed, so it is a
floor); Linux's `memory.peak` counts page cache charged to the cgroup; different toolchains (gnu 09-19 on Linux, musl 09-29 on Akuma and the
taoxichuan guest). The Akuma run used the committed tree `9f1e333f` with a freshly `cargo vendor`ed `vendor/` (the rig's old one lacked
`unicode-width`); source was copied into the image from the host's GitHub clone, not rsynced from the laptop.

The taoxichuan guest has no `/proc/meminfo`. Its idle is the same order: 654,000 free pages of 786,432 at 3072 MiB, i.e. ~517 MiB used
before any work. Both kernels sitting at ~0.5 GiB idle suggests a fixed kernel reservation (heap) rather than a per-process cost; **not
verified**. That idle floor is the first thing to account for when asking why Akuma's peak is higher than Linux's.

So there is **no taoxichuan-guest baseline yet**: it cannot finish the build (section 7), and with no `/proc/meminfo` its peak would have to be
read from the reap lines' `free A -> B`. Free memory was ~1.1-2.4 GiB at each of its failures, so memory is not what stops it.

## 7. Kernel panic: "Process table full (256 slots, 0 reclaimable RETIRED)" — OPEN

- **Symptom.** In the in-guest taoxichuan build (3 GiB, `-j4`), right after `[execve] s18 path=/usr/local/rust/bin/rustc`:
  `[PANIC] crates/akuma-exec/src/process/table.rs:73 Process table full (256 slots, 0 reclaimable RETIRED)`; ssh then reports
  "server not responding". pids had reached ~345 (`THREAD_PID_MAP=345`).
- **What it is.** `register_process` has `MAX_PROCESSES = 256` slots and panics when none is free. The known-open item is written in
  that function and in `docs/reference/subsystems/memory.md` ("OPEN: a full process table still panics the kernel"). Zero RETIRED
  entries means the 256 are live or unreaped, not waiting for the collector.
- **Why this build hits it.** This kernel registers a real `Process` per thread (`bind_child_task`, the clone fold), and cargo plus four
  concurrent `rustc` (each with several codegen/jobserver threads) is the first workload here with that many live at once. The
  serde/serde_json/fastrand `-j4` builds stay below 256.
- **Not yet established:** how many of the 256 are genuinely live versus leaked (a thread or zombie whose `Process` is never unregistered
  would also fill the table over a long build). Count live processes against `reap_process`/`unregister_process` calls in a run before
  raising the constant, which is a shared `akuma-exec` value that also sizes the AArch64 kernel.
- **Next steps.** (1) Log table occupancy at each reap on the `trace-err` build. (2) If occupancy is leaked, fix the leak. (3) If it is
  real, make `register_process` fallible so fork/clone answer `EAGAIN`, as the in-source comment already asks.
