# Taoxichuan: goose starts (epoll works) but cannot open its session database - every fd kept its own file size (fixed 2026-11-01)

**Status: fixed 2026-11-01** (§ Fix). goose + GLM now answers a prompt and runs a shell tool call on Taoxichuan (TCG). Found and
fixed on the local QEMU/TCG rig; the ryzen run is the cleanup plan's step 1d.

## Symptom

goose 1.52.0 (`goose-x86_64-unknown-linux-musl`, release asset sha256 `fdc86653...79f9f`, the same 149,764,512-byte static-pie binary the
ryzen uses) on Taoxichuan, configured for GLM (`glm-5.3-flash` through z.ai's OpenAI-compatible endpoint, the settings of
`scripts/kvm/taoxichuan_goose_image.sh`):

```
# goose --version
 1.52.0                       (exit 0; on 2026-10-30 this panicked: epoll_create1 ENOSYS)
# goose run --no-session -t "Reply with exactly the word PONG ..."
  error: Could not create session: error returned from database: (code: 522) disk I/O error
```

It fails the same way on a fresh `~/.local/share/goose`, every time, before any network I/O. The database is left with a 4,096-byte
`sessions.db`, a 32 KiB `-shm` and a 90,672-byte `-wal` (22 frames), so the schema migration ran for a while before the error.
No `[Fault]`/`PANIC` on the console. The `[err]` lines show nothing relevant: the failure is a wrong *answer*, not an errno.

## Cause

`522` is `SQLITE_IOERR_SHORT_READ`. SQLite ignores a short read of the main database file, but **not a short read of a WAL frame**
(`readDbPage` only forgives it in the non-WAL branch). goose uses sqlx, which opens several connections, so there are several open
descriptions of the same `-wal` file, and one connection reads frames that another one just appended.

Taoxichuan's `OpenFile::File { inode, offset, size }` (`taoxichuan/src/main.rs`, `enum OpenFile`) records the file size **once, at
`open`**, and only a write through *that* fd updates it (`write_file_arm_inner`, `writev_file_arm_inner`, the `PWRITE64` arm, `ftruncate`).
Every reader clamps to that copy:

- `PREAD64` arm: `want = len.min(size - off)`, so a read past this fd's stale size returns short (here: 0).
- `read_fd_arm`'s `Snap::File` branch: `have = size - cursor`, the same clamp.
- `fstat_fd`: `st_size` is the fd's copy, so a second connection also sees the wrong WAL length.

`clone_desc` copies the stale size into dup'd and forked fds too.

### Probe (Linux prints the full file on both lines)

```c
/* twofd.c - x86_64-linux-musl-gcc -O1 -static -o twofd twofd.c */
int a = open("/tmp/twofd", O_RDWR | O_CREAT, 0644);
int b = open("/tmp/twofd", O_RDWR);          /* a 2nd open description, like a 2nd SQLite connection */
pwrite(a, "0123456789", 10, 0);              /* grow through a */
fstat(b, &st); n = pread(b, buf, 10, 0);     /* read through b */
pwrite(b, "ABCDEFGHIJKLMNOPQRST", 20, 10);   /* grow through b */
fstat(a, &st); n = pread(a, buf, 30, 0);     /* read through a */
```

Taoxichuan:

```
fstat(b).size=0 pread(b)=0 []
fstat(a).size=10 pread(a)=10 [0123456789]
```

Linux: `size=10 pread=10`, then `size=30 pread=30`.

(A busybox-only check, `exec 9</tmp/x; echo hello >> /tmp/x; cat <&9`, printed `hello` and looked like a pass. It is not a test of this:
use the probe.)

## Why amd64 does not have it

amd64's `sys_pread64` (`amd64/src/fd.rs`) goes through `akuma_syscalls_glue::fs::sys_pread64`, which asks the filesystem, not an
fd-local copy. This is a Taoxichuan glue bug. It belongs to the file-description model that
`TAOXICHUAN_CRATE_EXTRACTION_PLAN.md` § X7 proposes moving into the shared crates, which is not started.

## Fix

`OpenFile::File` no longer has a `size` field. Its length is the inode's: a new `inode_size(inode)` (ext2 `metadata_by_inode`) is asked
by `read`, `pread64`, `fstat`, `lseek(SEEK_END)` and file-backed `mmap`. The writes (`write`, `writev`, `pwrite64`) and `ftruncate`
stopped maintaining a copy. Every place that wrote the copy was a place it could go stale, so removing the field closes the class
rather than one path. The cost is one inode read per file `read`/`pread`, under ext2's read lock.

Verified on TCG, the same image and boot:

| check | before | after |
|---|---|---|
| two-fd probe | `fstat(b).size=0 pread(b)=0` | `fstat(b).size=10 pread(b)=10`, `fstat(a).size=30 pread(a)=30` (Linux's answer) |
| `goose run --no-session -t "...PONG..."` | `(code: 522) disk I/O error` | `PONG`, about 8 s |
| `goose run --with-builtin developer` asking for `uname -a; echo hello > f; cat f` | - | goose ran it through its shell tool and reported both lines; the file exists afterwards |
| battery: quick execs, F1, PK1, fs, `apk add git`, `git --version` | baseline | identical to the recorded baseline (including apk's pre-existing `9 errors; 15 MiB in 19 packages`) |
| O_APPEND: `printf a > f; printf b >> f; printf c >> f` then `cat`, `tail -c 2`, `dd skip=1 count=1` | - | `abc`, `bc`, `b` |

No `[Fault]`/`PANIC` lines on the console.

## The next wall was the rig: the guest clock read 1970

With the database fixed, goose said `Network error: Could not connect to api.z.ai` and retried, although the console showed every TCP
connect to `:443` established in about 250 ms. goose hides the cause. `nettest-reqwest` (tokio + hyper + reqwest + rustls, the same stack)
names it: `invalid peer certificate: certificate not valid yet: verification time 12 (UNIX)`. microvm has no RTC, and Taoxichuan's
wall clock comes from an `epoch=<unix seconds>` cmdline token, which the ryzen rigs (`taoxichuan_pop.sh`, `boot_dev.sh`) pass and the
TCG rig did not. `scripts/kvm/taoxichuan/tcg_rig.py` now passes it. The busybox `wget` failure (`ssl_client: SSL_connect` / `No error
information`) was the same certificate check reported with less detail.

## Side finding: a session's shell clobbered sshd's fd 4

Over ssh, `exec 4</tmp/x` in the session's `/bin/sh -c` made the session's sshd fail `recvfrom(4)` with `EBADF` and tear the session
down (`[SSHD] Session pid 29: socket read failed ... Bad file descriptor`; console also shows `bridge_process: couldn't open stdin for
pid 29` before it). The same redirect one shell level deeper (`sh -c "exec 9<..."`) did not affect sshd. So the forked child's fd
changes reached the parent sshd's fd 4. Cause not investigated. It is not the goose blocker, but it is a fork/fd-table isolation
problem.

## Rig (local, no ryzen)

`scripts/kvm/taoxichuan/tcg_rig.py` (`build`, `boot [fresh]`, `run 'CMD'`, `stop`), written for this. It boots through a qcow2 overlay,
so the base image is never written by the guest, and it passes `epoch=`.

- Kernel: `cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests,trace-err` at `c362f165`, then with the
  fix. An uncommitted, trace-only `[netlap]` instrument from another session was in `taoxichuan/src/net.rs` and does not affect this.
- Image: the fork-sshd TCG image (`img/fork.img` of the 2026-10-30 supervisor rig), plus `/usr/local/bin/goose`,
  `/usr/local/bin/nettest-reqwest` (`ARCH=x86_64 userspace/nettest/rust/build-musl.sh reqwest`),
  `/root/.config/goose/{config,secrets}.yaml` and `/etc/goose.env` written with `debugfs -w` (write `secrets.yaml` from `~/.akuma/z.ai/token`
  with `umask 077`, then delete the host-side copy). **Never `debugfs -w` an image while the guest has it mounted.** This session did
  that once, before the overlay rig existed, and needed `e2fsck -fy` afterwards. `/usr/local/bin` is not on the ssh session's `PATH`;
  use full paths.
- Ready in 6–20 s. `goose --version` returns in under a second.

## Background

- [`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md), entry "2026-10-30 (late): goose + GLM on
  Taoxichuan - blocked: no epoll" (the previous wall), and the 2026-11-01 entry that points here.
- [`TAOXICHUAN_TOKIO_MULTI_EPOLL_FINDING.md`](TAOXICHUAN_TOKIO_MULTI_EPOLL_FINDING.md): the eventfd edge re-arm that let tokio's
  multi-thread runtime make progress.
- [`TAOXICHUAN_CRATE_EXTRACTION_PLAN.md`](TAOXICHUAN_CRATE_EXTRACTION_PLAN.md) § X7: the fd-model extraction that would remove this class.
