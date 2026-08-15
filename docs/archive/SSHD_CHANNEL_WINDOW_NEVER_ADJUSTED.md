# SSH stdin stops at exactly 1 MiB: the channel window was never adjusted

**Date:** 2026-08-13
**Status:** FIXED (`userspace/sshd/src/protocol.rs`).
**Found by:** trying to verify the `write_stdin` backpressure fix — Phase 0 item 5
of [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md) §8.5.

Two bugs sat on top of each other here, and the outer one had been hiding the
inner one since the userspace sshd was written. This doc is about the outer one:
sshd advertised a 1 MiB inbound channel window at channel-open and **never sent
`SSH_MSG_CHANNEL_WINDOW_ADJUST`**, so no SSH session could ever carry more than
1 MiB of stdin.

## Symptom

```bash
python3 - <<'EOF'
import subprocess
data = b'x' * (4 * 1024 * 1024)
subprocess.run(['ssh','-o','StrictHostKeyChecking=no','-p','2222',
                'root@localhost','cat > /tmp/big.txt'], input=data, timeout=600)
EOF
```

```
Timeout, server localhost not responding.
```

Then, from a fresh session:

```
$ wc -c /tmp/big.txt
1048576 /tmp/big.txt
```

**Exactly 1048576 bytes** — 0x100000 — arrive, the child writes them out, and
the session then hangs until the client's own timeout kills it. No error, no
log line, no kernel involvement. Small transfers were always fine, which is why
this survived: every interactive session and every `ssh host cmd` in the test
suite moves far less than a megabyte of stdin.

## Root cause

RFC 4254 §5.2 makes channel data flow-controlled. The receiver advertises an
initial window in `SSH_MSG_CHANNEL_OPEN_CONFIRMATION`; the sender may transmit
that many bytes and **no more** until the receiver replenishes it with
`SSH_MSG_CHANNEL_WINDOW_ADJUST`. sshd did the first half:

```rust
// protocol.rs, SSH_MSG_CHANNEL_OPEN handler
let mut reply = vec![SSH_MSG_CHANNEL_OPEN_CONFIRMATION];
write_u32(&mut reply, sender);
write_u32(&mut reply, 0);
write_u32(&mut reply, 0x100000);   // initial window: 1 MiB
write_u32(&mut reply, 0x4000);     // max packet size
```

and never the second. `SSH_MSG_CHANNEL_WINDOW_ADJUST` (byte 93) appeared nowhere
in the file — not as a constant, not as a send site. `grep -n window` over
`protocol.rs` matched only the unrelated `"window-change"` terminal-resize
request.

So the client sent precisely the advertised 1 MiB and then waited, correctly and
forever, for an adjust that no code path could produce. OpenSSH's client has no
timeout of its own for this; what eventually fires is the TCP/keepalive layer,
which is why the failure reads as "server not responding" rather than anything
about flow control.

## What this hid

The kernel's `ProcessChannel::write_stdin` used **drop-oldest** on overflow: at
`MAX_BUFFER_SIZE` (1 MiB, `crates/akuma-exec/src/process/channel.rs`) it drained
the front of the buffer to make room for new bytes, silently deleting the middle
of a byte-faithful input stream. That is the stdin twin of the stdout truncation
bug in `userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md`, whose fix
(`write_bounded` + `check_set_writer`) reached only the stdout copy —
the copy-paste outcome catalogued in
[`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md) §6.

§6 left the reachability question open: *"whether it is live-triggerable depends
on how much unread stdin a caller can queue past `MAX_BUFFER_SIZE`."*

**The answer is: not through sshd, and only because of this bug.** The SSH
window is `0x100000` and `MAX_BUFFER_SIZE` is `1024 * 1024`. They are the same
number. The missing window adjust capped total inbound stdin at exactly the
size of the buffer that would have overflowed, so the drop-oldest path could be
reached at its boundary but never driven past it. Two independent defects whose
limits coincided, each making the other invisible:

- the missing adjust made large stdin hang, so nobody tested large stdin;
- the drop-oldest buffer meant that if anyone *had* fixed the adjust alone, the
  result would have been silent corruption instead of a hang.

Fixing the window without fixing the buffer would have converted a visible hang
into invisible data loss. They had to land together.

## Fix

Both halves, 2026-08-13.

**Kernel** (`crates/akuma-exec/src/process/channel.rs`,
`process/mod.rs`, `src/vfs/proc.rs`, `src/syscall/fs.rs`): `write_stdin` returns
the number of bytes accepted and never drops buffered data —
the stdin counterpart of `write_bounded`. The count is carried out through
`write_to_process_stdin` → `ProcFilesystem::write_at` → `sys_write`, so a full
buffer becomes a short write rather than a lie.

Deliberately **no** `check_set_writer` equivalent: see "Why not block" below.
`sys_write`'s `File` arm turns a 0-byte accept into `EAGAIN` (or the bytes
already written), because falling through would leave `total_written < count`
and spin the chunk loop forever inside the kernel. On-disk files never produce
`Ok(0)` for a non-empty write — they report `NoSpace` as an error — so
`/proc/<pid>/fd/0` is the only source of that case.

**Userspace** (`userspace/sshd/src/protocol.rs`):

- `send_window_adjust()` emits `SSH_MSG_CHANNEL_WINDOW_ADJUST`.
- `bridge_process` grants the window back **as the child consumes bytes**, not
  as they arrive, batched at 64 KiB against the 1 MiB initial window. Crediting
  on arrival would restore the hang-free behaviour while re-authorising the
  overrun; crediting on consumption makes the SSH window carry the child's
  backpressure all the way to the client.
- `stdin_fd` joins `stdout_fd` and the socket in the non-blocking set, and gains
  a `stdin_pending` residue queue that survives across loop iterations.
- Client EOF is deferred through `stdin_eof_pending` until the residue drains,
  so `close_child_stdin` cannot land ahead of the input it follows.

### Why not block

The obvious fix — block the writer in the kernel until the child drains, like
the stdout path's `check_set_writer` + `schedule_blocking` — is wrong here, and
`bridge_process`'s own comment says why:

> CRITICAL: make BOTH ends non-blocking before the bridge loop. […] Without
> this, the loop parks in `read_fd(stdout_fd)` […] and never reaches the
> keystroke forwarding below, so the shell never receives input: a deadlock
> (bridge waits on stdout, shell waits on stdin).

`bridge_process` is a **single loop** that both drains the child's stdout and
forwards client input to its stdin. Parking it in the stdin write parks the only
thing that can create the space it is waiting for — the exact deadlock that
comment was written to prevent, arrived at from the other direction. Short write
plus a userspace retry is the deadlock-free shape, and it is what a Unix pipe
does anyway.

## Verify

```bash
cargo build --release && scripts/populate_disk.sh
MEMORY=2048 cargo run --release > boot.log 2>&1 &
until grep -aqE "sshd started|Started sshd" boot.log; do sleep 2; done
```

Then, from the host — note this is a **self-checking** test, so a truncation or
a reordering fails it rather than merely looking short:

```python
import subprocess, hashlib
data = bytes(bytearray(b'%08x' % i for i in range(1 << 20)))[:8 * 1024 * 1024]
r = subprocess.run(['ssh','-o','StrictHostKeyChecking=no','-p','2222',
                    'root@localhost','sha256sum'],
                   input=data, capture_output=True, timeout=1200)
print(r.stdout.decode().split()[0], hashlib.sha256(data).hexdigest())
```

Both hashes must match. `sha256sum` is CPU-bound and reads far slower than the
network delivers, so it drives the backpressure path rather than skating over
it; `cat > /tmp/big.txt` followed by an in-VM `sha256sum` is the disk-backed
variant.

Result 2026-08-13: 4 MiB via `cat` and 8 MiB via `sha256sum` both byte-exact,
where before the fix the 4 MiB case stopped dead at 1048576 bytes.

## Notes for next time

- **`grep` here is `ugrep`.** Searching a QEMU log without `-a`/`--text`
  silently matches nothing — QEMU emits a control byte that makes the log look
  binary. This applies to every command in this doc.
- The coincidence that `0x100000 == MAX_BUFFER_SIZE` is worth distrusting in
  general: when a limit is hit at *exactly* a round number, check whether two
  different subsystems picked the same constant before concluding which one is
  responsible. Reading only `wc -c` here would have pointed straight at the
  kernel buffer, which was the wrong half.
- sshd advertises `0x4000` as its max packet size and that half was fine —
  clients respect it, and it is unrelated to the window.

## Background

- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  §6 (the unfixed stdin twin) and §8.5 Phase 0 item 5 (the work item this came
  out of).
- `userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md` — the stdout
  half of the same drop-oldest bug, fixed earlier; the reason `write_bounded`
  exists at all.
- `userspace/sshd/docs/PROCESS_PER_SESSION.md` — why each session is its own
  process, which is what makes a per-session residue queue cheap.
- `crates/akuma-exec/src/process/channel.rs` — `write_stdin`, `write_bounded`,
  `check_set_writer`, `MAX_BUFFER_SIZE`.
- `src/syscall/fs.rs` — `sys_write`'s chunk loop; the `File` arm's `Ok(0)`
  handling and the `Stdout`/`Stderr` arm's blocking backpressure loop, for
  contrast.
