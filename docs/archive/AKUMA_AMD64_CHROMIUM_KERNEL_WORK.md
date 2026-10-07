# Chromium on amd64 Akuma: the kernel work (2026-10-07..08)

`userspace/kami` runs headless Chromium and blits its screencast onto
`/dev/fb0`. It ran first on the trashcan's Ubuntu, in an Alpine container
(Alpine's `chromium` 142, musl). An `strace -f` of one page load there gave the
list of kernel features Chromium leans on (`userspace/kami/README.md` § "What
Chromium asks of the kernel"). This records what that list found missing or
wrong in the amd64 kernel, what was fixed, and what Chromium itself hit when it
first ran on Akuma under Firecracker.

The gate for the fixed half is `userspace/forktest/c_stress/chromeprobe.c`.
Every expectation in it comes from running it on Linux first. On Linux it
passes **16/16**, and on the amd64 kernel under Firecracker **16/16** since
2026-10-08.

## Fix 1: amd64 `sendmsg`/`recvmsg` on a unix fd answered `ENOTSOCK`

`amd64/src/usermode.rs` sent every `sendmsg`/`recvmsg` to `crate::sock`, which
only knows `FileDescriptor::Socket` (AF_INET). A unix fd got `ENOTSOCK`, so
plain unix `sendmsg` had never worked on this target, with or without
ancillary data. Socket, bind, connect and the rest already went "AF_UNIX
first" to glue. These two arms were missed. They now do the same.

## Fix 2: `SCM_RIGHTS` (fd passing) did not exist

Chromium passed 628 descriptors over `SOCK_SEQPACKET`/`SOCK_STREAM` pairs
loading one page. The pure AF_UNIX table had reserved the shape
(`Record::anc_fds`, a channel-teardown hook with a `debug_assert!` that no fds
were in flight), but nothing carried descriptors:

- `sendmsg` ignored `msg_control`.
- `recvmsg` always reported `msg_controllen = 0`.

Now:

- **Wire format.** `akuma_net_unix::scm` holds the LP64 `cmsghdr` parse and
  encode, and Linux's `scm_detach_fds` rule for a short receive buffer. It is
  host-tested.
- **In flight.** Glue's `IN_FLIGHT` token table holds one `clone_fd_refs`
  reference per descriptor in transit. An `InFlight` guard releases them on
  every send path that does not commit.
- **Receive.** `recvmsg` installs the descriptors and honours `MSG_CTRUNC`
  and `MSG_CMSG_CLOEXEC`.
- **Teardown.** Closing a channel that still holds unread descriptors closes
  them, where it used to assert.
- **Stream stop rule.** A `SOCK_STREAM` read stops after the first message
  carrying descriptors (`UnixTable::stream_read_limit`), as Linux's
  `unix_stream_read_generic` does.

Reference: `docs/reference/subsystems/syscalls/net.md` § "SCM_RIGHTS".

## Fix 3: a shared-writable page of an unlinked file was lost when its writer exited

The amd64 write-back (`flush_shared_write`) went through the path recorded at
`mmap`. Once that path stopped naming the inode, it skipped the flush, on the
grounds that "an unlinked file's contents die with its last mapping, as on
Linux". That is not Linux: the contents live as long as *any* fd or mapping
references the inode. The failing sequence:

1. Process A maps an unlinked file and does not touch the page.
2. Process B maps the same file through a passed fd, writes the page, and
   exits.
3. B's exit dropped the shared page (no other present mapper).
4. The flush was skipped, because the file has no name.
5. A's later fault refilled the page from a file that never got B's bytes.

On Linux A sees the write. Chromium unlinks every shared-memory file right
after creating it, so this was on its path.

**Fix:** a by-inode write path (`Filesystem::write_at_by_inode`; ext2
implements it, `SubdirFs` forwards it, `OverlayFs` sends it to the upper layer
only), and the flush uses it whenever the path no longer names the inode. That
also keeps the 2026-10-03 guarantee that an unlink-and-recreate never writes
into the *new* file. Host tests are in `akuma-ext2`; the `shmvar` variants c
and d now match Linux.

## Fix 4: `ftruncate`/`fallocate` on an unlinked fd answered `ENOENT`

Both went by `KernelFile::path`. Chromium creates its shared-memory file,
unlinks it, and only then sizes it. So the file stayed at 0 bytes, and every
later write-back of the mapping fell past EOF and was dropped. They now go by
the fd's inode once the path no longer names it
(`akuma_vfs_glue::{truncate,fallocate}_open_file`, backed by ext2's
`truncate_by_inode`/`fallocate_by_inode`). While the path still names the
inode they keep the path route, so the path-keyed mapping notifications are
unchanged. Gate: `chromeprobe`'s `shm_unlinked_writer_exits`.

## Fix 5: amd64 `setsockopt` on a unix fd answered `ENOTSOCK`

The same routing hole as Fix 1, found by Chromium on Akuma. crashpad's
`setsockopt(SO_PASSCRED)` failed, and the browser `CHECK`-crashed. Glue had
always accepted `SOL_SOCKET` options on unix sockets (`dispatch_setsockopt`),
so AArch64 was unaffected; amd64 now routes there. `SO_PASSCRED` is accepted,
but no `SCM_CREDENTIALS` are generated yet.

## Fix 6: `execve` recorded the literal path as the image name

`/proc/<pid>/exe` reports `image.name`, which `execve` set to the path it was
given. A process started with `execve("/proc/self/exe", …)` (Chromium starts
every child that way) could then read `/proc/self/exe` back as itself, and
Chromium finds its own files next to that path. The name is now resolved
through symlinks before the swap, while `/proc/self` still names the caller.
**Not shown to have changed Chromium's behaviour**: the snapshot and
crashpad errors below persisted after it.

## Fix 7: `hpbox.deploy()` reported success on a reset that did not land

`deploy()` resets the box to the newest local commit found on *any* remote.
A commit pushed only to the private `litter` remote is chosen even though the
box cannot fetch it. The `git reset` failed inside a pipeline that hid its
status, the box stayed 210 commits back, and the patch was applied onto that
tree. Two Firecracker runs measured a kernel nobody had written: it made
writable `MAP_SHARED` look entirely broken, because the old tree predated the
shared-page table. `deploy()` now refuses when the box did not land on the
intended commit. The runbook (`amd64-bare-metal-loop.md`) shows how to carry
commits over with `git bundle`.

## Still open: what Chromium hit on Akuma

Chromium was run under Firecracker on an ext2 image built from the same
Alpine container, with `init=/bin/busybox initargs=sh,<script>`. It starts and
gets well into browser startup, then:

- **`execve` reads the whole executable into the kernel heap.** Chromium is
  250 MB, and it re-execs itself for the zygote and utility processes. On a
  512 MiB heap the second copy does not fit (`[ALLOC FAIL]
  requested=249690856`); the exec waits about 20 s and fails `EIO`. A guest
  with ≥ 8 GiB gets the 1 GiB heap and gets past it. The fix is to stream the
  loader: read headers, then each segment straight into its pages. See
  `proposals/AMD64_FD_WHOLE_FILE_HEAP.md`.
- **crashpad: `posix_spawn chrome_crashpad_handler: ENOENT`.** The file is
  there. musl's `posix_spawn` uses a `CLONE_VM|CLONE_VFORK` child that
  `execve`s. It is not yet known whether this is the path or the vfork-exec.
- **`Failed to create socket directory`** (ProcessSingleton): `mkdtemp` under
  `/tmp`, not yet probed.
- **Zygote children: `Error loading V8 startup snapshot file`.** They get the
  snapshot as an fd from the browser. Not seen with `--no-zygote`, which
  stops earlier at the ProcessSingleton error.
- **`int3` from ring 3 arrives as SIGSEGV** (`#GP err=0x1a`: IDT vector 3's
  gate is not DPL 3). Chromium's `CHECK` failures therefore look like
  segfaults. Linux delivers SIGTRAP.
- **`init=` does not follow symlinks.** `init=/bin/sh` on an image where it
  is a fast symlink to busybox read the link target's bytes as block numbers
  (`read_sectors: sector 14819201400`, which is "/bin" read as a block
  number). ext2's `read_at` should refuse a symlink inode.
- **Syscalls missing from the x86_64 table** (ENOSYS, non-fatal so far): 40
  `sendfile`, 141 `setpriority`, 239 `get_mempolicy`, 297
  `rt_tgsigqueueinfo`, 444 `landlock_create_ruleset`.

Background: `userspace/kami/README.md` (the Ubuntu measurements and the
`strace` summary), `docs/reference/subsystems/syscalls/net.md`,
`docs/reference/subsystems/amd64-shared-write-mmap.md`.
