# Can Akuma drop `spawn` and the other private syscalls? (2026-11-10)

**Status: research and plan. Nothing here is implemented.** Written from a read of the tree at
`89bd5f73` (branch `akuma-multiarch-attempt-0`); no kernel was built or booted, no harness was run.
Every "exists" below means "the code is there and a doc or test says it works", not "I ran it".
The list of things I could not verify is at the end.

## Question

The owner asked whether Akuma's own `spawn` syscall, and the other non-Linux syscalls, can go, leaving
only Linux syscalls, to simplify both kernels and our own userspace. Vendored third-party code cannot
be rewritten; our own binaries can.

## Short answer

Yes for 21 of the 25 private entries, and the tree has already run the experiment once:
Taoxichuan serves no private syscall at all (`TAOXICHUAN_CLEANUP_PLAN.md`, "No private syscall") and
runs sshd, herd, goose and `cargo` on `fork`+`execve`. Two things are not a rewrite and need kernel
design: **entering a box** (`spawn_ext`) and **taking over another process's terminal** (`reattach`).
The four box-management calls (`register_box`, `kill_box`, `set_box_stack`, `mount_in_ns`) have no
Linux spelling because Linux has no equivalent object; keep them, or move them to a control file.

The most important finding is that **sshd already is on the Linux path**: since `811456e5`
(2026-11-05) `userspace/sshd/Cargo.toml:88` turns on `libakuma/linux-abi` unconditionally, which swaps
`spawn_full` for `pipe2`+`fork`+`dup3`+`execve` (`userspace/libakuma/src/lib.rs:2234`). The cost of that
change is a silent downgrade of the pty path (see section 3, case 1), which this plan repairs rather
than creates.

## 1. Inventory

### 1.1 The private numbers

Numbers are asm-generic (AArch64); on amd64 each is `AKUMA_PRIVATE_BASE (0x1000) + n`
(`userspace/libakuma/src/lib.rs:138`, `amd64/src/usermode/dispatch.rs:59`). Defined in
`crates/akuma-syscalls-linux/src/nr.rs:85-149` and `:285`; mirrored in `libakuma` twice
(`lib.rs:180-330`, one module per architecture). 321 and 323 are reserved (removed framebuffer calls,
`nr.rs:111-123`), 304 and 306 are unused. **25 entries: 24 live on AArch64, 21 on amd64.**

`akuma-native` (`crates/akuma-syscalls-glue/Cargo.toml:89`, 21 `cfg` sites in `lib.rs`) already gates
the arms for all of them except the box family (gated by `sc-containers`) and `mount_in_ns`. It is in
glue's `default` and named explicitly by both kernels (`Cargo.toml:890`, `amd64/Cargo.toml:568`), so
it is always on today. Taoxichuan builds without it ("Off, the numbers fall to `ENOSYS`",
`docs/reference/subsystems/config-flags.md:109`).

| # | Number | Name | AArch64 handler (glue) | amd64 handler | Does |
|---|---|---|---|---|---|
| 1 | 300 | `RESOLVE_HOST` | `net.rs:1737` (`akuma-native`+`smoltcp`) | `dispatch.rs:340` (own copy, 255-byte cap) | name to IPv4 via the kernel resolver |
| 2 | 301 | `SPAWN` | `proc.rs:1784` | `usermode.rs:4137` (separate 259-line implementation) | create a process from a path: fresh `ProcessChannel`, returns `pid \| stdout_fd << 32`; flags `PTY=1`, `CONSOLE=2` (amd64 only) |
| 3 | 302 | `AKUMA_KILL` | `proc.rs:2006` (`sys_kill`) | `dispatch.rs:447` to the same `sys_kill` | identical handler to Linux `kill` (`lib.rs:924`), only the number differs |
| 4 | 303 | `WAITPID` | `proc.rs:2035` | `usermode.rs:4546` (117 lines) | non-blocking reap of a spawned child by pid |
| 5 | 305 | `TIME` | `time::sys_time` (`akuma-syscalls-time/src/lib.rs:610`) | **absent (`ENOSYS`)** | wall-clock microseconds |
| 6 | 307 | `SET_TERMINAL_ATTRIBUTES` | `term.rs:394` | `dispatch.rs:414,424` | raw/cooked bits on the caller's terminal or a pty fd |
| 7 | 308 | `GET_TERMINAL_ATTRIBUTES` | `term.rs:434` | `dispatch.rs:414` | read those bits (reads the console's, even for a pty, per `AKUMA_AMD64_PTY.md` §7) |
| 8 | 309 | `SET_CURSOR_POSITION` | `term.rs:455` | `dispatch.rs:414` | writes `ESC[r;cH` to the process channel |
| 9 | 310 | `HIDE_CURSOR` | `term.rs:466` | same | writes `ESC[?25l` |
| 10 | 311 | `SHOW_CURSOR` | `term.rs:474` | same | writes `ESC[?25h` |
| 11 | 312 | `CLEAR_SCREEN` | `term.rs:482` | same | writes `ESC[2J` |
| 12 | 313 | `POLL_INPUT_EVENT` | `term.rs:490` | `dispatch.rs:386` (glue's if a channel exists, else `fd::sys_poll_input_event`) | read stdin bytes with a timeout, `u64::MAX` blocks |
| 13 | 314 | `GET_CPU_STATS` | `term.rs:580` | `dispatch.rs:414` | per-thread `{tid,pid,box_id,cpu_us,state,last_core,name}` table |
| 14 | 315 | `SPAWN_EXT` | `proc.rs:1845` | `dispatch.rs:503` to glue | spawn with `SpawnOptions`: `box_id` (enter a box), cwd, env, stdin, credentials |
| 15 | 316 | `REGISTER_BOX` | `container.rs:3` (`sc-containers`) | `dispatch.rs:505` to glue | create/update a box registry entry and its mount namespace |
| 16 | 317 | `KILL_BOX` | `container.rs:57` | `dispatch.rs:507` | kill every member, drop the namespace |
| 17 | 318 | `REATTACH` | `container.rs:84` | `dispatch.rs:509` | re-point another process's output channel at the caller's (`box grab`, `box run`) |
| 18 | 319 | `UPTIME` | `time::sys_uptime` (`:613`) | `dispatch.rs:433` | microseconds since boot |
| 19 | 320 | `SET_TPIDR_EL0` | `proc.rs:83` | **absent** | write the TLS base (EL0 can do this itself) |
| 20 | 322 | `CONSOLE_NOTIFY` | **absent (reserved FB number)** | `dispatch.rs:462` (`console-notify` feature) | print a framed line on the machine console |
| 21 | 324 | `SET_BOX_STACK` | `proc.rs:1951` | `dispatch.rs:515` | mark a box as rump-networked |
| 22 | 325 | `MOUNT_IN_NS` | `container.rs:336` | `dispatch.rs:520` | box 0 composes a box's mount table from outside |
| 23 | 326 | `CLOSE_CHILD_STDIN` | `proc.rs:1985` | `usermode.rs:4522` to the same glue fn | EOF on a spawned child's stdin channel |
| 24 | 327 | `CORE_INIT` | `proc.rs:1514` (returns `ENOSYS` always) | **absent** | multikernel leftover, a permanent stub |
| 25 | 328 | `AKUMA_GET_VERSION` | `version.rs:120` | **absent** | returns the git sha; floor control for benchmarks |

Also mention the private numbers outside the handlers: `crates/akuma-syscalls/src/lib.rs:309-310,428-460`
(excursion classes for `WAITPID`/`UPTIME`/`AKUMA_GET_VERSION`) and `crates/akuma-bkl/src/policy.rs:228,276`
(BKL opt-out bitmap holds `RESOLVE_HOST` and `AKUMA_GET_VERSION`).

### 1.2 Callers

Owned userspace only: `userspace/{box,herd,sshd,httpd,paws,termtest,elftest,forkprobe,hello,stackstress,wall,scratch,hget,libakuma,libakuma-tls,klogd,powersaved,wifi}/src`
grepped by call name; C probes under `forktest/` and `ext2probe/` by number. `userspace/meow` is a
submodule that is **not checked out in this worktree** (empty directory), so its callers are unknown.
Kernels never invoke these numbers: the kernel's own spawns use in-kernel functions
(`spawn_process*`, below).

| Call | Callers (file:line) | Zero callers? |
|---|---|---|
| `RESOLVE_HOST` | `libakuma/src/net.rs:371,379` (`lookup_host`, `resolve`), which reaches `libakuma-tls/src/http.rs:204`, `scratch/src/http.rs:113`; `sshd/src/client/protocol.rs:1094` | no |
| `SPAWN` (via `spawn`, `spawn_pty`, `spawn_console`, `spawn_with_stdin`, `spawn_with_env`, all one `spawn_full`, `lib.rs:2173`) | **sshd** `protocol.rs:512` (`spawn_pty`), `:545` (`spawn_pty`), `:547` (`spawn_with_env`); **herd** `main.rs:1033` (`spawn` of the VM helper), `:1222` (`spawn`, local core path), `:1369` (`spawn_console`), `:1374` (`spawn_with_env`), `:1379` (`spawn`); **httpd** `main.rs:504,507` (`spawn_with_env`, CGI, the two branches of one `if`); **paws** `lib.rs:332` (`spawn_with_stdin`), `:638,661,674` (`spawn`); **elftest** `main.rs:87` | no |
| `AKUMA_KILL` (`kill_signal`) | sshd `protocol.rs:805`; herd `main.rs:1558`; paws `lib.rs:706`; box `main.rs:420` (probe) | no |
| `WAITPID` (`waitpid`, `waitpid_status`) | sshd `protocol.rs:901`; herd `main.rs:1560,1926`; httpd `main.rs:549,615`; paws `lib.rs:644,679,709`; box `run.rs:316`, `build.rs:537`, `main.rs:239,308,413`; elftest `main.rs:91,163`; termtest `main.rs:193`; forkprobe `main.rs:494` | no |
| `TIME` | httpd `main.rs:207,431,491,582`; scratch `commit.rs:108` (assuming it is libakuma's `time`) | no |
| `SET/GET_TERMINAL_ATTRIBUTES` | sshd client `client/protocol.rs:485,490`; termtest `main.rs:46,60,132,238` | no (meow unknown) |
| `SET_CURSOR_POSITION`, `HIDE/SHOW_CURSOR`, `CLEAR_SCREEN` | termtest `main.rs:68-124`; paws `lib.rs:235` (`clear_screen`) | no (meow unknown) |
| `POLL_INPUT_EVENT` | paws `main.rs:39`, `lib.rs:704`; termtest `main.rs:96,111,230` | no (meow unknown) |
| `GET_CPU_STATS` | box `main.rs:130` (`thread_stats`, used by `box ps`, `box grab`, `box show`) | no |
| `SPAWN_EXT` | box `main.rs:219`, `:293`, `run.rs:287`, `build.rs:530`; herd `main.rs:980` (in `spawn_in_box`, three callers `:1274,1329,1348`), `:1405` (the `workdir` service) | no |
| `REGISTER_BOX`, `KILL_BOX`, `MOUNT_IN_NS`, `SET_BOX_STACK` | box `run.rs`, `build.rs`, `main.rs`; herd `main.rs:991,1011,1316-1350` (43 lines across the two) | no |
| `REATTACH` | box `main.rs:232,303,410`, `run.rs:310`, `build.rs:533`; paws `lib.rs:641` | no |
| `UPTIME` | herd (`main.rs:588,596,1076,1090,1429,1554,1789`), sshd `protocol.rs:169`, httpd `stats.rs`, box `run.rs:149`, `commit.rs:128`, `build.rs:211`, scratch, hello, stackstress, paws | no |
| `CLOSE_CHILD_STDIN` | sshd `protocol.rs:1065` (the stdin pump's EOF) | no |
| `CONSOLE_NOTIFY` | wall `main.rs:64` | no (amd64 only; `ENOSYS` on AArch64) |
| `SET_TPIDR_EL0` | **none** (only the constants in `libakuma`) | **yes** |
| `CORE_INIT` | herd `main.rs:898-925` issues it by raw number; it always fails | effectively yes |
| `AKUMA_GET_VERSION` | `userspace/ext2probe/c/read_syscall_cost.c:318` (a benchmark floor arm) | only a benchmark |

Corrections to the census the task started from: **termtest does not call `spawn_pty`**, it uses
`fork` and `waitpid_status`; **paws** has four spawn sites, not one; **httpd** has two source sites,
not three. `kami` uses `std::process::Command` (Linux calls) and is not a caller. The C probes in
`forktest/c_stress/` use the musl ABI.

Kernel-internal spawns that stay no matter what: `crates/akuma-kernel-glue/src/lib.rs:1807`
(`spawn_process_with_channel` for herd), `:1833` (`spawn_process` for sshd on `extreme-size`),
`rump_proxy.rs:1449,1507` (rump servers); amd64 builds init through `spawn_process_task`
(`amd64/src/usermode.rs:2581,4787`). Linux's kernel also execs init itself; these are the equivalent.
Only two non-test callers reach the *syscall-shaped* `spawn_process_with_channel_ext` /
`spawn_process_with_creds`: glue `proc.rs:1814` and `:1922`.

## 2. Linux replacement for each

"Kernels have it" cites where I looked. Both kernels dispatch glue's arms unless stated.

| Private call | Linux sequence | Kernels already have it? | What is missing |
|---|---|---|---|
| `SPAWN`, no flags | `pipe2(O_CLOEXEC)` x2, `clone`/`fork`, in child `dup3`, `close`, `execve`; parent `wait4` | `pipe2` `glue lib.rs:771`, `dup3 :770`, `execve :823`, `clone :820`, `wait4 :912` all present. AArch64 `CLONE_VM\|CLONE_VFORK` is a real shared-page-table vfork (`docs/reference/subsystems/syscalls/proc.md`, `VFORK_FASTPATH_ENABLED`, `akuma-config/src/lib.rs:525`). amd64 serves `CLONE_VM\|CLONE_VFORK` as a **CoW fork with the parent not suspended** (`dispatch.rs:1066-1103`). The shape already exists in `libakuma` as the `linux-abi` `spawn_full` (`lib.rs:2234`), which `sshd` uses and Taoxichuan runs | A vfork-style helper in `libakuma`: child on its own stack, asm-only until `execve`. Today `spawn_full` calls `fork()` (`clone(SIGCHLD)`, a full CoW fork, `lib.rs:2425`) |
| `SPAWN` env | `execve(path, argv, envp)` | yes | native `SPAWN` with an empty env fell back to `DEFAULT_ENV`; `execve` gives an empty environment. Callers must always pass a composed env (herd already does for console and boxed services) |
| `SPAWN` stdin bytes | write the bytes to the stdin pipe, then close it | yes; implemented in `linux-abi` `spawn_full` (`lib.rs:2282-2294`) | writing before the child runs blocks if the bytes exceed the pipe capacity while the child is blocked writing its stdout: a deadlock for large CGI bodies (httpd) and for paws's captured pipe. Needs a poll loop or writer thread |
| `SPAWN` cwd | `chdir` in the child | `chdir` yes (`glue lib.rs:857`) | native `SPAWN` ignored the caller's cwd (herd's comment at `main.rs:1389-1397`); Linux inherits it, a behaviour change to check per caller |
| `SPAWN_FLAG_PTY` | `open("/dev/ptmx")`, `TIOCSPTLCK(0)`, `TIOCGPTN`, `fork`; child `setsid`, `open("/dev/pts/N")`, `TIOCSCTTY`, `dup3` to 0/1/2, `execve`; window size by `TIOCSWINSZ` on the master | **Yes, on both kernels.** `crates/akuma-syscalls-glue/src/pty.rs` (64 slots, `TIOCSPTLCK/GTPN/GPTPEER/SCTTY/NOTTY/GWINSZ/SWINSZ/GPGRP/SPGRP/GSID`, `ISIG` to the foreground group, hangup rules); `setsid` `proc.rs:139`; `ptmx` in `akuma-vfs/src/dev.rs:88`; `FdState::Pty` readiness for `poll`/`epoll`. Spec and 31/31 `ptyprobe` on amd64 QEMU at SMP=1 and 4 and on the metal: `docs/archive/AKUMA_AMD64_PTY.md` §6 | nothing in the kernel. AArch64 is "served by the same glue" per that doc, but **`ptyprobe` has not been recorded on AArch64** (unverified). Known gaps from §5 of that doc: `kill(-pgid)` unsupported by `sys_kill` (`proc.rs:2010`: `pid <= 1` is `EPERM`), no output flow control |
| `SPAWN_FLAG_CONSOLE` (amd64) | none: a child of the console process **inherits** the console channel | the amd64 shell already runs everything this way: `busybox sh` forks, children share the console channel (`usermode.rs:1505-1540`, `console_attached` at `:3060`) | herd as `init` must redirect service output itself (see section 3, case 2). The `EPERM` policy ("only a console-attached process may hand the console on", `usermode.rs:4151-4161`) disappears, because inheritance is the mechanism |
| `SPAWN_EXT` box entry | `fork`; child: an entry call for the box, `chdir`, `setgroups`/`setgid`/`setuid`, `execve` | creds: `setuid/setgid/setresuid/setgroups` `glue lib.rs:1089-1097`; `chroot` `:858`; `chdir`. **There is no `setns`, `unshare` or `pivot_root` in the tree** (`grep` finds only the constant `CLONE_NEWNS` in `akuma-syscalls-linux/src/proc.rs:120`) | the entry primitive. See 2.1 |
| `REGISTER_BOX`, `KILL_BOX`, `SET_BOX_STACK`, `MOUNT_IN_NS` | none: Linux has no registry of namespaces with a policy that "box 0 composes the mount table from outside" (`containers.md`, "Mount policy") | `/proc/boxes` already exposes the registry read-only (`akuma-vfs-glue/src/proc.rs:1092`); precedent for a control file instead of a syscall: `amd64/src/kvm/ctl.rs` (`/dev/kvm0` takes `Spec` key=value text, `akuma-vm-spec`) | see 2.2 |
| `REATTACH` | pty master ownership: the runner of the box holds the master and bridges it; `box grab` would need to obtain that fd | `SCM_RIGHTS` exists on AF_UNIX (`glue net.rs:1201-1264`, `unixsock.rs:261`); `containers.md` says amd64 has no AF_UNIX (that line may be stale, unverified) | `pidfd_getfd` is absent (`grep` finds no row). See 2.3 |
| `WAITPID` | `wait4(pid, &st, WNOHANG)` or blocking `wait4` | yes; `libakuma` already does this under `linux-abi` (`lib.rs:2567`) | nothing. The native one only matches `spawn` children (amd64 by `spawner_pid`, `containers.md`) |
| `AKUMA_KILL` | `kill(pid, sig)` | the same handler `proc::sys_kill` | only `libakuma`'s constants: Linux `kill` is 129 (AArch64) and 62 (x86_64) and is missing from `libakuma`'s tables |
| `CLOSE_CHILD_STDIN` | `close(write_end)` of the stdin pipe | yes | sshd's pump (`protocol.rs:833-870,1065`) writes `/proc/<pid>/fd/0` and calls the private EOF; the `linux-abi` branch already carries a `child_stdin` fd (`TAOXICHUAN_SSH_STDIN_PUMP.md`) |
| `UPTIME` | `clock_gettime(CLOCK_MONOTONIC)` | yes; `libakuma::uptime` already does this under `linux-abi` (`lib.rs:969`) | nothing |
| `TIME` | `clock_gettime(CLOCK_REALTIME)` | yes | nothing (and it is `ENOSYS` on amd64 today) |
| `SET_TERMINAL_ATTRIBUTES`, `GET_TERMINAL_ATTRIBUTES` | `ioctl(fd, TCGETS/TCSETS[W/F])` | yes on console/session terminals and ptys (`docs/reference/subsystems/tty.md`: "`TCGETS/TCSETS`" in glue `term.rs`, `Termios::to_wire`) | `PtyPair`'s raw-off restores default flags, not the program's earlier ones (pty doc §7), which `tcsetattr` save/restore would cure |
| `SET_CURSOR_POSITION`, `HIDE/SHOW_CURSOR`, `CLEAR_SCREEN` | `write(1, "\x1b[..")` | yes | nothing: the kernel arms literally write those escape strings to the channel (`term.rs:455-487`), so these are four wrappers of `write` |
| `POLL_INPUT_EVENT` | raw termios (`VMIN=0`) plus `ppoll`/`read` on fd 0 | `ppoll` yes; "readiness for a session stdin fd is decided in glue/poll from the channel" (`tty.md`) | the timeout semantics (`u64::MAX` blocks) become a `ppoll` timeout. A process with no channel gets `ENOMEM` from the private call today |
| `GET_CPU_STATS` | `/proc/<pid>/stat` and `/proc/boxes` | `/proc/<pid>/stat` (44 fields, `akuma-procfs`), `/proc/<pid>/status`, `/proc/boxes` exist. `akuma-procfs/src/tree.rs:60` carries `box_id` per process | whether `status` prints the box id and whether `stat` field 39 (`processor`) is filled: **unverified**. If not, add a `Box:` line to `/proc/<pid>/status`. `box` needs pid-to-box and name |
| `RESOLVE_HOST` | a stub resolver in userspace: read `/etc/resolv.conf`, UDP query | UDP sockets yes. `crates/akuma-dns` is `no_std`, `forbid(unsafe)`, effects-based (`build_query`, `parse_response`, `resolve_a`), exactly what `libakuma` could link | a nameserver source. The kernel does not write `/etc/resolv.conf` (no writer found under `crates/`, `src/`, `amd64/src/`); it is disk content (`overlays/`, box overlays). The in-kernel resolver stays regardless: the SNTP bootstrap uses it |
| `CONSOLE_NOTIFY` | `write` to `/dev/kmsg` or `/dev/console` | **neither node exists**: `akuma-vfs/src/dev.rs` has `null random tty ptmx fb0 ...`, and a test asserts `console` is absent (`dev.rs:326`) | one dev node whose write does `serial::puts` with the existing framing and 512-byte cap |
| `SET_TPIDR_EL0` | write `TPIDR_EL0` from EL0 | n/a | nothing; delete |
| `CORE_INIT` | none | n/a | nothing; delete, and herd's `core` key |
| `AKUMA_GET_VERSION` | `uname(2)` already carries the same sha | yes | the benchmark loses its "no-arm" floor; pick a Linux arm and re-baseline (`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`) |

### 2.1 Entering a box without `SPAWN_EXT`

What `SPAWN_EXT(box_id != 0)` does that a plain fork+exec does not: checks `can_access_box`
(`proc.rs:1869-1879`), makes the child take the box's `box_id` and mount namespace *before* the
binary is resolved (`spawn.rs:281-294,537-548`), gives it a fresh `TerminalState`
(`spawn.rs:213,482`), and for a rump box wires a sysproxy channel onto fd 3 when the path contains
`rump_server` (`proc.rs:1930-1936`, `hooks::attach_server`).

Options, in order of how much Linux they keep:

- **A. `setns(fd, CLONE_NEWNS|...)`** with an fd from `open("/proc/<pid>/ns/mnt")` or `/proc/boxes/<id>`.
  Runs in the forked child, so `execve` resolves the path inside the box with the ordinary process
  namespace and the per-thread `set_spawn_namespace` override becomes unnecessary. The access rule is
  the existing `box_access::can_access_box`. Needs: a procfs ns entry or a new fd kind, `setns` in the
  `akuma-syscalls-abi` table (x86_64 308, asm-generic 268) with an arm in glue. This is the only option
  that reads as Linux.
- **B. A write to `/proc/self/box`**, the pattern of `/proc/self/oom_score_adj`. Smaller than A
  (no fd kind) and still no syscall number.
- **C. Keep one private call** (`enter_box(id)`) and delete the other 24. Honest fallback if A and B
  cost more than they are worth; it still removes `SPAWN_EXT`'s 112-byte `SpawnOptions`, its size
  negotiation (`proc.rs:1836-1860`), `spawn_creds`, and all of `spawn_process_with_creds`'s box/pty/creds
  parameters.

I recommend A, with B as the budget version. Not decided here.

### 2.2 The management plane

`register_box`, `kill_box`, `mount_in_ns`, `set_box_stack` are four low-rate calls with two callers
(herd, box). Linux's equivalent is a container runtime doing `unshare`, `mount`, `pivot_root` inside
the new namespace, which contradicts the deliberate rule that a boxed process cannot mount and a
box's namespace is composed from box 0 (`containers.md`, "Mount policy: composed from outside, once").
They can keep their numbers and be documented as the only Akuma-private surface, or become a control
file (`/proc/boxes` accepting writes, or a `/dev/box` node) following `/dev/kvm0`. The second is
cleaner and not urgent.

### 2.3 `reattach`

Callers use it for two jobs. (i) Foreground `box run`/`open -i`/`use -i` and paws's "run this and show
its output" (`run.rs:310`, `paws lib.rs:641`): that exists only because a native spawn child gets a
private channel nobody reads. With a real pty (or inheritance), the child writes to the terminal and
the job vanishes. (ii) `box grab`, which takes over a *running* box's terminal and can force-detach the
previous holder (`syscalls/container.md`, "reattach"). That has no Linux call; `pidfd_getfd` (Linux 5.6)
is the nearest and is not implemented. Either implement it, or have the box's runner serve the master
fd over an AF_UNIX socket, or keep `reattach` as an exception.

## 3. Hard cases and risks

1. **sshd's pty path is currently downgraded.** `sshd` always builds `libakuma` with `linux-abi`
   (`Cargo.toml:88`), and `spawn_full`'s linux-abi body drops `flags` (`lib.rs:2234-2240`, `_flags`).
   `spawn_pty` therefore returns a plain pipe, and `set_terminal_size(res.stdout_fd, ...)`
   (`protocol.rs:518,526,436,453`) is a `TIOCSWINSZ` on a pipe. `TAOXICHUAN_SSH_STDIN_PUMP.md` records the
   same ("a `linux-abi` pty session gets a pipe, not a pty"). I read this; I did not boot it.
   Consequence: an interactive `ssh` login on the current tree most likely has no tty. The real-pty
   port in the plan fixes it. Consider that an argument for doing sshd first.
2. **Console attachment and logging in herd.** Native `spawn` gives each service a private channel
   that herd drains to `/var/log/herd/<name>.log`; `spawn_console` (amd64) hands the console instead.
   With fork, a child inherits herd's own fds, which on `init=/bin/herd` is the console. Services
   must be started with `open(log)`, `dup3(log, 1)`, `dup3(log, 2)`, stdin from `/dev/null` (herd's
   drain loop disappears). A console shell is the case where nothing is redirected. Risk: forked
   children share the parent's `TerminalState` (`spawn_inherits_terminal`, `spawn.rs:213`); the
   comment at `spawn.rs:446-470` records the bug this rule exists for (a multiplexing daemon's
   children aliasing one `input_waker`). Piped children whose fds 0/1/2 are pipes do not read the
   channel, but I did not verify that `fork_process` gives them a fresh terminal state.
3. **The 4 MB `extreme-size` profile.** The kernel starts `/bin/sshd` itself
   (`akuma-kernel-glue/src/lib.rs:1828-1840`, `config::AUTO_START_SSHD`, no herd) with `/bin/paws` as
   the shell. The profile builds `--no-default-features`, yet `akuma-native` and `exec` are forced on
   for glue (`Cargo.toml:890`), so every private arm is in that image; `sc-containers` is off, so the
   box arms are not. `pty.rs` is not feature-gated, so it is compiled in at 4 MB; a pair is ~28 KiB
   of heap (`AKUMA_AMD64_PTY.md` §3), about the same as the lazily allocated `LineDiscipline` a pty
   `TerminalState` already costs (`tty.md`). Fork is the real risk:
   `FPCACHE_UNDERSIZED_AT_LOW_RAM.md` measured `/bin/sh: can't fork: Out of memory` after ~9 sessions
   at 4.5 MB, and `build-extreme-size.md:27-49` builds sshd without `fork-sessions` for that reason.
   **sshd's own `libakuma` fork() for the shell has been the path since 2026-11-05**, so the 4 MB
   gate (`acceptance/05_meow_tcc_extreme_4mb.md`) is already exposed. I found no record of it being
   rerun after `811456e5`. A shared-page-table vfork child (AArch64 has it) avoids the CoW share of
   sshd's pages; `fork()` does not. Neither was measured here.
4. **amd64 has no real vfork.** `clone(CLONE_VM|CLONE_VFORK)` is a CoW fork with the parent running on
   (`dispatch.rs:1066-1103`). Correct, but each spawn pays a CoW share of the parent. Relevant to
   httpd's CGI-per-request and herd. Unmeasured.
5. **Boot-time spawns** stay as kernel functions. What goes away is their *syscall-shaped* variants
   (`spawn_process_with_channel_ext`'s `pty`/`box_id`, `spawn_process_with_creds`'s `creds`), which
   would then be called from tests only (`spawn.rs:231,252`).
6. **Boot self-test suites.** AArch64 `src/process_tests.rs` has syscall-level tests for the private
   numbers: `nr::SPAWN_EXT` at `:3360` (`test_spawn_ext_passes_env` `:3326`) and `:4457`; the box guard
   test `test_box_isolation_syscall_guards` (`:4346-4532`) calls `REGISTER_BOX`, `KILL_BOX`,
   `SET_BOX_STACK`, `MOUNT_IN_NS`; `AKUMA_GET_VERSION` `:20212,20263`. Spawn-property tests that use the
   in-kernel function (`test_spawned_child_not_a_tty` `:8379`, `..._pty_is_a_tty` `:8421`,
   `test_box_crossing_spawn_gets_fresh_terminal_state` `:8463`, `test_spawn_resolves_a_shebang_script`
   `:10724`, 31 references to `spawn_process*` in total) survive or die with the ext variants.
   amd64: `amd64/src/usermode/tests.rs:754-766` (`console_notify`), `:1562`, `:1792` (box family "exercised
   live"). Shared: `crates/akuma-syscalls/src/tests.rs:119-134,369,433-435`.
7. **Credentials.** `SPAWN_EXT`'s `uid/gid/groups` are applied atomically before the child's first
   instruction (`proc.rs:1721-1752`, `spawn.rs:535`). In fork+exec the child runs as the caller until
   its own `setgroups/setgid/setuid`; the kernel's rule (only lowering, permanent) makes this safe, but
   the sequence order matters and `box run` should keep `-u` failure as "refuse to start".
8. **Rump boxes.** `attach_server` triggers on `path.contains("rump_server") && box_is_rump(box_id)`
   inside `sys_spawn_ext` (`proc.rs:1930-1936`). In the new model the only place that sees "a process
   in a rump box is exec'ing a rump_server" is `execve`. The rump devbox is "deferred" per
   `CLAUDE.md`, but `rump` is in the default kernel features and `acceptance/11` exercises it.
9. **Version skew.** Private numbers are baked into binaries already on disk images, which is why
   321-323 stay reserved (`nr.rs:111-123`). A kernel that removes a number returns `ENOSYS` to an
   older `herd`/`box`/`sshd` (the Taoxichuan README records exactly this for `UPTIME`). Ship kernel
   and userspace together and never reuse a freed number.
10. **`wait4` vs the private `waitpid`.** The private one reaps a `spawn` child by pid without
    parenthood semantics; `box grab` already learned it cannot wait on a non-child and probes
    `kill(pid, 0)` (`container.md`). `box run` polls `waitpid` every 50 ms (`run.rs:316-319`); `wait4`
    would block.
11. **Third-party binaries.** `linux-compat.md:98-101` states musl binaries never call the private
    numbers, and a grep of the owned C probes agrees. busybox, apk, gcc, rustc, bun, redis are musl.
    `meow` is ours but its source is not in this worktree; `llama.cpp`, `tcc`, `rumpkernel` were not
    searched by instruction. The plan's step 0 (a kernel-side first-use log) answers this empirically.

## 4. Migration plan

Each step is shippable and leaves a green tree. "Verify" names harnesses that exist; none were run.

**Step 0: measure who calls what.** Add a once-per-number `safe_print!` of "private syscall N first
used by pid P (comm)" in the dispatch arms (both kernels), behind a build flag. Boot an image with
herd, box, sshd, paws, meow and the rest; the log is the real caller list, including `meow` and any
image binary I could not read. Deletes nothing. Verify: `scripts/vm_ready.py`, `scripts/ssh_harness.py`.

**Step 1: delete what has no caller or a one-line Linux twin.**
- `SET_TPIDR_EL0` (zero callers), `CORE_INIT` (always fails; remove herd's `core` key and
  `core_init`, `main.rs:898-925,1186-1212`), `AKUMA_GET_VERSION` (move `ext2probe`'s floor arm),
  `TIME` (httpd x4, scratch x1 to `clock_gettime(CLOCK_REALTIME)`), `UPTIME` and `AKUMA_KILL` (make
  `libakuma`'s `linux-abi` bodies unconditional; add Linux `kill` to its number tables).
- Deletes: glue `proc.rs:83-86,1508-1517`, `version.rs` syscall arm, `time` aliases (`akuma-syscalls-time
  lib.rs:610,613` if no other user), the `nr.rs` constants and `akuma-syscalls` / `akuma-bkl` list entries,
  amd64's 319 and 302 arms. About 40 lines in the kernels; libakuma loses 7 constants x 2 arches.
- Verify: `cargo test` in `akuma-syscalls`, `akuma-bkl`; AArch64 boot suite; `scripts/box_amd64_suite.py`;
  `herd start/stop` round trip.

**Step 2: the terminal family becomes `ioctl`/`write`/`ppoll`.**
- New `libakuma::term` built on `TCGETS/TCSETS`, `write` of the four escape strings, and `ppoll`+`read`.
  Port paws, termtest, sshd's client (and meow when its source is in reach).
- Deletes 307-312, 313, 314's twin work, glue `term.rs:383-577` (~195 lines), amd64 `dispatch.rs:386-433`
  (the 313/307-312/314 arms), `fd::sys_poll_input_event`'s private copy.
- Needs: confirm `TCSETS` raw mode on the console channel on both kernels (the pty one is verified by
  `ptyprobe`); add `Box:` to `/proc/<pid>/status` if absent and move `box`'s `thread_stats` onto `/proc`
  (deletes 314, `term.rs:580-619`).
- Verify: `termtest` by hand over ssh on both kernels, the meow TUI, `ptyprobe` unchanged.

**Step 3: userspace DNS.** `libakuma` links `akuma-dns` (`no_std`, `forbid(unsafe)`) over a UDP socket
and `/etc/resolv.conf`; `lookup_host`/`resolve` use it. Deletes 300: glue `net.rs:1737-1758`,
amd64 `dispatch.rs:340-361`, the `akuma-bkl` opt-out entry. Needs: a nameserver on every image (see 2).
Verify: `hget`, `libakuma-tls` fetch, sshd client to a named host, on an image with and without DHCP.

**Step 4: process creation, no box.** Order inside the step matters.
1. `libakuma`: vfork-style `spawn_full` (own stack, asm-only child) as the one implementation; stop
   having a native twin. Add `cwd` and explicit env.
2. **sshd first**: real pty via `ptmx` for `pty-req` sessions; bridge reads/writes the master; window
   size by `TIOCSWINSZ` on the master; shutdown by closing the master and `kill`. This removes
   `bridge_process`'s `/proc/<pid>/fd/0` write and `close_child_stdin` (`protocol.rs:833-870,1065`).
   Fixes section 3, case 1.
3. herd (services via fork, redirecting output to the log file; console services by inheritance; the
   VM helper), httpd (CGI, with a poll loop for stdin), paws, elftest.
4. Delete `SPAWN` (glue `proc.rs:1755-1826`, amd64 `usermode.rs:4099-4395`, ~295 lines each side),
   `WAITPID` (glue `proc.rs:2028-2053`, amd64 `usermode.rs:4546-4662`), `CLOSE_CHILD_STDIN`
   (glue `proc.rs:1970-2004`, amd64 `:4522`), `SPAWN_FLAG_CONSOLE`, their `nr.rs` rows and
   `libakuma`'s `spawn*`/`waitpid*` wrappers.
- Verify: `scripts/sshd_concurrency_test.py`, `scripts/sshd_crash_hunt.py`, `scripts/ssh_harness.py`,
  `forktest/c_stress/{spawnprobe,spawnalias,dynspawn,ptyprobe}.c` on both kernels,
  `acceptance/05` at 4.0 MB (the gate that matters), `acceptance/10`.
- Risk gate: do not delete the native path until `acceptance/05` is re-run with the vfork helper.

**Step 5: boxes.** Decide 2.1 and 2.3 first.
- Add the entry primitive and port `box` (4 sites) and herd (`spawn_in_box`, the `workdir` service)
  to fork; `setns`; `chdir`; `setgroups/setgid/setuid`; `execve`; wait.
- `box run`/`open -i`/`use -i`: box holds a pty master and bridges; `reattach` for the
  foreground case disappears. `box grab`: `pidfd_getfd`, or a socket from the runner, or keep `REATTACH`.
- Deletes `SPAWN_EXT` (glue `proc.rs:1651-1942`, ~290 lines: `SpawnOptions`, layout asserts,
  `spawn_creds`, the V1/V2 size negotiation), `REATTACH` if replaced (`container.rs:84-135`), the
  syscall-shaped parameters of `spawn_process_with_creds`, `FileDescriptor::ChildStdout` and its arms
  (44 sites in 14 files, `grep`), `spawn_inherits_terminal`'s box half, `boxlib::sys::SpawnOptions`
  (`userspace/box/src/sys.rs:21-51`), the amd64 `315`/`318` arms.
- Update `test_box_isolation_syscall_guards` and `test_spawn_ext_passes_env` to the new sequence;
  `scripts/box_amd64_suite.py` is the live gate (it lists the bugs boxes once had).

**Step 6: close the door.** Make `akuma-native` an off-by-default kernel feature; build both kernels
and Taoxichuan without it and run everything. Whatever fails is a remaining caller. When green, delete
the feature, `AKUMA_PRIVATE_BASE`, amd64's `akuma_private` stage (`dispatch.rs:316-525`) except the
kept box-plane arms, the dual-arch `libakuma::syscall` tables, and update
`docs/reference/abi/linux-compat.md:98-101` and `config-flags.md:109`. Optionally `CONSOLE_NOTIFY` goes
to a `/dev/kmsg` node here. Add a bug-fix-list row per `docs/runbooks/update-bug-fix-list.md`.

## 5. Recommendation

- **Remove 21 of the 25**: 300, 301, 302, 303, 305, 307-314, 315, 318, 319, 320, 322, 326, 327, 328.
  Steps 1-4 (17 of them) are userspace ports onto syscalls both kernels already serve, and Taoxichuan
  has shown the shape works. Steps 5 (box entry, `reattach`) need one small new Linux primitive
  (`setns`) and a decision about `box grab`.
- **Keep, for now, `REGISTER_BOX`, `KILL_BOX`, `SET_BOX_STACK`, `MOUNT_IN_NS`** as the only
  documented Akuma-private surface (the management plane has no Linux twin and two owned callers).
  Move them to a control file later if the number-free ABI is worth it.
- **Do sshd's pty first.** It is the only step that is also a repair.
- **Do not delete the native spawn until the 4 MB gate has run on a vfork-style `libakuma` spawn.**

Size, counted from the line ranges above (they include doc comments): the shared glue private handlers
are about 890 lines (`proc.rs` ~395, `container.rs` 195 of 409, `term.rs` ~237, `net.rs` 23, plus the
~45 dispatch arms in `lib.rs`; `version.rs`'s 132 lines are mostly this), amd64 adds about 650
(`dispatch.rs:316-525` 210, `usermode.rs` spawn 297 + waitpid 146). That is roughly **1,500 kernel
lines**, before the second-order deletions (`ChildStdout`, the spawn-parameter branches in
`akuma-exec`) which I did not count. If the four box-plane calls stay, their handlers (~195 lines in
`container.rs` plus `set_box_stack` 18) come off that number. For scale, Taoxichuan's own private
spawn path was a 759-line commit (`f821ddda`, 12,167 to 11,408). Userspace: `libakuma` loses its
spawn/waitpid/terminal/resolve wrappers (about 450 lines of `lib.rs`) and one of its two number tables;
herd loses its output-drain loop and `core`; sshd's `bridge_process` loses the `/proc/<pid>/fd/0` pump;
box loses `SpawnOptions` and the `reattach` poll loop.

## 6. What I could not verify

- **No build, boot or harness run.** Everything is from reading code and docs.
- `userspace/meow` is not checked out here; its use of spawn, terminal and DNS calls is unknown.
  `llama.cpp`, `tcc`, `rumpkernel` and other submodules were not searched.
- Whether `ptyprobe` passes on **AArch64**; the docs record amd64 QEMU and metal only.
- That sshd's `pty-req` sessions really have no tty on the current tree (section 3, case 1): inferred from
  `spawn_full`'s `_flags` and from the Taoxichuan doc.
- Fork/vfork cost at 4 MB and on amd64's CoW spawn; whether `acceptance/05` still passes since
  `811456e5`.
- Whether `fork_process` gives a forked child with pipe stdio a fresh `TerminalState` (section 3, case 2).
- Whether `/proc/<pid>/status` carries a box id and `stat` carries a processor field.
- Whether amd64 has AF_UNIX now (`containers.md:424` says no; `SCM_RIGHTS` code is in shared glue).
- Where `/etc/resolv.conf` gets its content on images, and whether the amd64 comment at
  `dispatch.rs:297-...` about it is current.
- Whether `scratch/src/commit.rs:108` calls libakuma's `time`.
- The line counts are range counts, not a `git diff --stat` of a prototype.

## Background

- `docs/reference/abi/linux-compat.md` (private-number paragraph, to update), `docs/reference/subsystems/config-flags.md`
  (`akuma-native`), `docs/reference/subsystems/syscalls/proc.md` (clone/vfork/execve),
  `docs/reference/subsystems/syscalls/container.md`, `docs/reference/subsystems/containers.md`,
  `docs/reference/subsystems/tty.md`, `docs/reference/build-profiles.md`, `docs/reference/userspace-layout.md`.
- `docs/archive/TAOXICHUAN_CLEANUP_PLAN.md` (steps 2e and "No private syscall"; B11),
  `docs/archive/TAOXICHUAN_SSH_STDIN_PUMP.md`, `docs/archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md`,
  `docs/archive/AKUMA_AMD64_PTY.md`, `docs/archive/FPCACHE_UNDERSIZED_AT_LOW_RAM.md`,
  `docs/archive/AKUMA_ARCH_CONVERGENCE_RESEARCH.md`, `docs/runbooks/build-extreme-size.md`,
  `acceptance/05_meow_tcc_extreme_4mb.md`.
