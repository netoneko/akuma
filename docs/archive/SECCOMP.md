# Seccomp filters for Codex on Akuma

Codex installs a classic BPF syscall filter in its
`--apply-seccomp-then-exec` stage. Akuma previously returned `EINVAL` from the
seccomp prctl operations and accepted `PR_SET_NO_NEW_PRIVS` without storing it.
Both native Akuma kernels now enforce the installed policy at syscall entry.

## Source provenance

The instruction layout, opcode values, and 16-word scratch memory come from
NetBSD `sys/net/bpf.h` CVS revision 1.82, Git
`57b869a1b1e93e77e35bfee34f52e94075019aac`. The Regents' notice, three
redistribution conditions, and disclaimer are in `akuma-seccomp/src/bpf.rs` and
in the repository's binary redistribution materials. The validator,
interpreter, and integration are original Akuma Rust under BSD-2-Clause.
[NetBSD header at that revision](https://github.com/NetBSD/src/blob/57b869a1b1e93e77e35bfee34f52e94075019aac/sys/net/bpf.h)
[Third-party notices](../../THIRD_PARTY_NOTICES.md)

The Linux wire ABI and action behavior were implemented from public
documentation, the architecture ABI tables, and black-box Linux observations.
No Linux implementation source or headers were consulted for the seccomp
definitions. The architecture table test checks 277 on AArch64 and 317 on
amd64. An independent query of the installed libseccomp architecture tables
with `seccomp_syscall_resolve_name_arch` returned the same numbers.
[Linux seccomp filter documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html)
[seccomp(2)](https://man7.org/linux/man-pages/man2/seccomp.2.html)
[PR_SET_NO_NEW_PRIVS(2const)](https://man7.org/linux/man-pages/man2/PR_SET_NO_NEW_PRIVS.2const.html)

Black-box probes enumerated the accepted classic BPF opcode bytes and checked
unaligned and out-of-range absolute loads, packet-load rejection, invalid
shifts, and the scratch verifier's handling of branch joins and unreachable
code. Runtime probes confirmed wrapping variable shifts, zero-divisor
termination, a 4095 errno cap, and `TRACE` returning `ENOSYS`. An amd64 SIGSYS
handler observed `SYS_SECCOMP`, the action data, the original syscall number
in the saved RAX, and a call address equal to the saved return IP. With
`KILL_THREAD` on a group leader, a sibling ran and the group exited normally.
[Linux action probe](../../userspace/seccomp_probe/linux_actions.c)
[Guest probe](../../userspace/seccomp_probe/guest.c)

## Allocation and entry paths

The copied 64-bit `sock_fprog` is 16 bytes. Installation checks the declared
length and validates the complete instruction range before creating the
instruction vector. A filter owns a single fallible vector reservation of at
most 32 KiB and one fallible reference-counted chain node. The instruction
copy uses a 256-byte stack buffer with `akuma-pod`. Scratch dataflow uses an
8 KiB stack array only for programs that read scratch memory. Evaluation
uses 16 scratch words on the stack. A 32768-instruction path limit, charging
four instructions per predecessor, bounds storage and traversal. The maximum
number of one-instruction nodes on a path is 6554. The chain drops its
unshared tail iteratively. Fork, clone, and spawn share the existing chain
by reference and syscall evaluation needs no allocation.

`akuma-syscalls` maps the winning action to an entry decision. AArch64
uses ELR_EL1 and checks the decision before its separate signal-return path.
amd64 uses the return RIP saved by the raw syscall stub and applies the
decision before ABI decoding or legacy folding. Signal delivery and task
exit use the architecture's existing lifecycle paths.

## Verification

- **Host classic BPF and ABI tests.** 112 tests passed across
  `akuma-seccomp`, `akuma-syscalls`, `akuma-syscalls-linux`, and
  `akuma-syscalls-abi` on `x86_64-unknown-linux-gnu`. The seccomp crate also
  passed Clippy with warnings denied. The integration test includes the
  exact 114-instruction blob from installed Codex CLI 0.169.0. Its SHA-256
  is `24cafc6ac11a8f90a4be0e4643f22e5cd9a1c3ddf51faa1d22455ac14387c8b0`.
  The installed syntax is `codex sandbox -- /bin/true`. Host
  `strace -f -e seccomp,prctl` confirmed installation and showed the program
  pointer. The syscall-boundary debugger read the instruction bytes and
  a repeat capture matched the fixture. [Capture script](../../userspace/seccomp_probe/capture_codex.py)
- **amd64 QEMU TCG.** The `--no-default-features --features headless` build
  booted via `amd64/run.sh`, passed 793 self-tests with zero failures, and
  answered Python-driven SSH. The guest probe verified `EACCES` without
  `NO_NEW_PRIVS`, sticky state, invalid-opcode rejection, `GET_ACTION_AVAIL`,
  `EPERM` for `socket` with `read` allowed, per-thread clone inheritance,
  fork and exec inheritance, newest-filter errno data, and a denied
  `rt_sigreturn`. It also verified caught SIGSYS, `TRACE`, both kill actions,
  and a sibling surviving a leader's `KILL_THREAD`.
- **Codex inner stage.** The installed Codex binary ran as uid 65534,
  equivalent to the capability check it expects at entry. The guest
  `strace_nr=157,317,41,59` trace showed syscall 317, operation 1, flags 0
  returning 0. The command executed under the resulting filter, printed
  `Codex filter active after exec`, and verified the AF_INET socket denial,
  the successful read, seccomp mode 2, and sticky `NO_NEW_PRIVS`.
- **Full Codex command with real bubblewrap.** Bubblewrap 0.11.0 ran in
  the same guest. `codex sandbox -- /bin/seccomp_probe --codex-child`
  exited 1 before the inner stage. On a fresh guest with `strace_err` enabled, bubblewrap
  hit `ENOENT` opening `/proc/sys/kernel/overflowuid` and Codex hit
  `ENOENT` opening `/proc/self/mountinfo` while building the command.
  A rerun on a reused image also hit Codex's app-server socket directory
  ownership and mode check.
  [Codex setup and troubleshooting](../runbooks/debug-codex-on-akuma.md)
  [amd64 syscall tracing](../runbooks/trace-failing-syscalls-amd64.md)
- **AArch64 build and QEMU gate.** `cargo check`, Clippy, and a release build
  passed for `aarch64-unknown-none`. QEMU ran the memory, filesystem,
  threading, process, and network boot suites and answered Python-driven
  SSH with `aarch64`. The runner used the pinned toolchain's LLVM objcopy
  because the host GNU objcopy could not read the AArch64 ELF.
- **Unsafe budget.** The budget check passed without modification.

## Reproduce the amd64 guest checks

Build `userspace/seccomp_probe/guest.c` with a Linux libc, for example
`cc -O2 -static -pthread userspace/seccomp_probe/guest.c -o seccomp_probe`.
Install the probe at `/bin/seccomp_probe` and stage Codex and the real
bubblewrap at `/usr/bin/codex` and `/usr/bin/bwrap`, including the loader
and libraries required by bubblewrap. Boot with
`NO_DEFAULT_FEATURES=1 FEATURES=headless INIT=/bin/herd`,
`KCMDLINE="strace_err strace_nr=157,317,41,59,272,165"`, and
`sh amd64/run.sh -accel tcg`. Wait for SSH before running the probe and
the two Codex checks.

```sh
/bin/seccomp_probe
cd /tmp
profile='{"type":"managed","file_system":{"type":"restricted","entries":[{"path":{"type":"special","value":{"kind":"root"}},"access":"read"}]},"network":"restricted"}'
HOME=/tmp /bin/seccomp_probe --exec-unprivileged /usr/bin/codex codex-linux-sandbox \
    --sandbox-policy-cwd /tmp --command-cwd /tmp --permission-profile "$profile" \
    --apply-seccomp-then-exec -- /bin/seccomp_probe --codex-child
PATH=/usr/bin:/bin /usr/bin/codex sandbox -- /bin/seccomp_probe --codex-child
```

## Background

[Seccomp syscall reference](../reference/subsystems/syscalls/seccomp.md)
[Taoxichuan syscall middleware example](../../taoxichuan/docs/reference/syscalls.md)
