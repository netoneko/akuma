# What is refused outside box 0, and what should be (audit, 2026-11-18)

Written after `docs/archive/BWRAP_ON_BOXES.md`: a sandbox (a private mount namespace made by `clone(CLONE_NEWNS)`) is a
box whose processes are **uid 0**, so "boxed" now describes an untrusted root, not only an OCI container's service. This
audit lists every call the tree refuses (or filters) for a process outside box 0, then the calls that are not refused
and probably should be. It was done by reading the code at `c54f5fa2`; each candidate says whether I confirmed it by
reading or only suspect it.

Follow-up: items 1–3, 5, 7 and 11 were addressed. Kernel log reads, clears and
unread size require host root on amd64. The hostname, domain name and shared
smoltcp privileged ports require host root on both architectures. AF_UNIX
abstract names are scoped to the caller's box. Pathname sockets are resolved
through the caller's VFS view and keyed by mount ID and pinned inode.
Process-targeted limit, priority and scheduling calls apply box visibility.
`/proc/mounts` skips the global table when the namespace has its own root.

Loopback service isolation and capability-aware DAC remain open (items 4 and
6). The accepted uid/gid map writes in item 8 remain a compatibility stub.

## 1. What is gated today

Two shapes. **Host root only** (`akuma_exec::process::host_root()`: box 0 *and* uid 0, the `HostRoot` token); and
**box rule** (`box_access`/`box_reaches`: a boxed process reaches its own box and descendants, box 0 reaches all).

| call / object | rule | where |
|---|---|---|
| `mount`, `umount2`, FAT `mount` | host root. **One exception, new:** the builder of its own private namespace may mount into *that namespace* (`NsCreator`), until the final `pivot_root` | `container.rs` `mount_permit`, `private_ns.rs`; `vfat.rs:107` |
| `MOUNT_IN_NS` | host root | `container.rs:421` |
| `reboot`, `system_off`/`reset` | host root | `reboot.rs:11` (glue), `reboot.rs:222` (amd64) |
| `clock_settime`, `settimeofday`, `adjtimex` stepping | host root | `akuma-syscalls-time:374,422`, amd64 `dispatch.rs:131` |
| open `/dev/kvm0`, `/dev/kvm`, `/dev/wifi0`, `/dev/power`, `/dev/net/tun` | host root; a boxed caller gets no node at all | amd64 `chardev.rs:41-48` |
| open `/dev/fb0`, `/dev/input/mouse0` | box 0 only | amd64 `chardev.rs:41` |
| raw block devices `/dev/vdX` | host root (boxes get no synthetic `/dev`) | `fs.rs:2404-2416`, `dev_probe::in_box` |
| connect to the cloud metadata IP `169.254.169.254` | host root | `net.rs:251` (`egress_allowed`) |
| `REGISTER_BOX`, `KILL_BOX`, `SPAWN_EXT(box_id)`, `reattach` | box rule (`can_register_box`, `can_kill_box`, `can_access_box`) | `container.rs`, `box_access` |
| `kill`/`tkill`/`pidfd_send_signal` | box rule (`check_signal_pid`) | `signal.rs`, `pidfd.rs:91` |
| `pidfd_open` | children of the caller only | `pidfd.rs:46` |
| `/proc/boxes`, `/proc/cores`, `/proc/dac` | box 0 only; other `/proc/<pid>` entries are filtered by the viewer's box | `akuma-procfs` `Global::exists`, `visible()` |
| `clone(CLONE_NEWNS)` (new) | box 0 only, uid 0 or `CLONE_NEWUSER`; a boxed caller is `EPERM` (no nested namespaces) | `private_ns.rs::clone_namespace` |
| `pivot_root` (new) | the builder of a private namespace only | `private_ns.rs::pivot_root` |
| devices shown by `ls /dev` | none inside a box (`null`, `zero` stat only) | `dev_probe` |
| SysV message queues | keyed `(box_id, msqid)` | `akuma-syscalls-ipc` |

## 2. Candidates: not refused today, probably should be

Priority is for a sandbox whose processes are uid 0 and **share the host network stack** (`--unshare-net` is not a
namespace, `docs/archive/BWRAP_ON_BOXES.md` § not enforced).

### High

1. **`syslog(2)` / `dmesg` has no box check (amd64).** `sys_syslog` (`usermode.rs:805`) decodes the action and
   answers for any caller: a boxed uid 0 can **read the whole kernel log** and **`Clear` it** (`klog_clear`). The log
   holds boot traces and driver dumps. Linux has `dmesg_restrict` and wants `CAP_SYSLOG` to clear. *Confirmed by reading.*
   Add: reads and clears are the host root's; `SIZE_BUFFER`/`CONSOLE_*` no-ops stay open. (The AArch64 kernel has no
   `syslog` row in glue's table; recheck when it gets one.)
2. **AF_UNIX names are one namespace for every box.** `UnixTable.names` is keyed by the bound name alone
   (`akuma-net-unix/src/lib.rs:719`). Abstract names (`@foo`) have no filesystem to scope them, so a sandboxed process
   can `connect` to a host daemon's abstract socket (dbus-style); and a path name is the *string the binder passed*,
   so a box binding `/run/x.sock` collides with, or reaches, the host's `/run/x.sock` even though the two files are
   different. *The table keying is confirmed by reading; whether `connect` resolves a path through the caller's
   namespace before the lookup I did not trace.* Add: key names by `(box id, name)` for abstract names, and make path
   names resolve to a filesystem identity (mount id + inode) as the file does.
3. **Privileged ports are gated by uid only.** `sys_bind` refuses ports below 1024 to non-root (`net.rs:257`); a box's
   uid 0 passes, and a box on the shared stack can take a port the host's service has not bound (or race it at boot).
   *Confirmed by reading.* Add: ports below 1024 need the host root while a box shares the host stack.
4. **Loopback services are reachable.** herd's control port (`127.0.0.1:7117`) and every host service on loopback accept
   a box's connection; the only barrier is a token file (`/var/run/herd/control.token`, 0600) that a uid 0 sandbox with a
   read-only bind of `/` **can read**. With network allowed, a sandbox can `herd stop`/`start`. *The token mode and
   the shared stack are confirmed; the exploit path was not run.* Add: either a per-box network stack with no route to
   the host's loopback, or run sandboxed commands as a non-zero uid (codex's own model), or keep herd's token outside
   the paths a sandbox is given.

### Medium

5. **`sethostname`/`setdomainname` change the global name.** Gated by uid 0 only (`proc.rs:323`), stored in one
   static (`NODENAME`). A boxed root renames the machine for everyone. *Confirmed.* Add: host root only, or a per-box
   UTS field (`--unshare-uts` is accepted and ignored today).
6. **A uid-0 sandbox keeps uid 0's file overrides.** `capset` to empty is remembered so `capget` reports nothing
   (`Creds::caps_dropped`), but DAC still treats uid 0 as the superuser: inside a *writable* bind it may `chmod`/`chown`
   anything, and it reads every 0600 file it can see (finding 4). *Confirmed by design: there is no capability model
   finer than "uid 0 or not".* Add: make `dac` consult `has_capabilities()` instead of `is_root()` for the overrides,
   so a dropped uid 0 is an ordinary owner.
7. **`prlimit64`, `setpriority`, `sched_set*` are accepted no-ops for any pid.** Harmless today (nothing changes) but
   `prlimit64(pid, ...)` ignores `pid`, so it answers for processes in other boxes. *Confirmed.* Add the box rule to
   the `pid` argument so it cannot be used as a cross-box existence probe.
8. **`/proc/self/{uid_map,gid_map,setgroups}`** swallow writes (new). Nothing changes, but a program that asked for a
   remap believes it got one. *Confirmed; intentional.* Listed so the gap is not forgotten.

### Low / watch

9. `chroot` is uid 0 only and acts on the caller's own view (`sys_chroot` resolves inside the caller's namespace), so
   it cannot leave a box; fine, but note it in any future "no chroot in a sandbox" policy.
10. `io_uring_*` answer `ENOSYS` and `process_vm_readv`, `ptrace`, `unshare`, `setns`, `bpf`, `keyctl`,
    `perf_event_open`, `swapon`, `init_module` have no dispatch row. They are refused for everyone today; if one is
    implemented later it needs the box rule from day one (the `pidfd_send_signal` comment is the model).
11. `/proc/<pid>/mountinfo` and `fdinfo` apply the box rule like `status`; the global `/proc/mounts` still lists the
    global table for a boxed viewer (`render_mounts` global pass), which a pivoted namespace should not show.

## 3. Method

* `grep -rn "host_root()\|caller_is_host_root\|box_id != 0\|box_id == 0\|in_box\|caller_box_and_pid().0"` over
  `crates/akuma-{syscalls-glue,kernel-amd64,syscalls-time,exec,vfs-glue}` gave section 1.
* For each syscall a sandbox could abuse (`syslog`, `sethostname`, `chroot`, `prlimit64`, `process_vm_readv`,
  `pidfd_*`, `bind`, AF_UNIX names), the arm in `akuma-syscalls-glue/src/lib.rs` and its handler were read.
* Not done: a runtime probe of any candidate; fuzzing the box rule; the AArch64-only paths.

## 4. Suggested order

1 (`syslog`), 5 (`sethostname`), 7 are one-line host-root checks. 2 and 3 are small and local. 4 and 6 are design
changes (per-box network, DAC on capabilities) and want their own plan.
