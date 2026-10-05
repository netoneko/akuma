# The kernel work the wgpu stack needed (and still needs)

**Date:** 2026-10-05. **Scope:** everything on the kernel side that running
`akuma-cli-wgpu` (the framebuffer demo and its wgpu custom backend) and then
the **rio** terminal on the trashcan panel depended on — what was built, what
userspace had to work around, and what is still open, in priority order.
Userspace stays plain Linux (`x86_64-unknown-linux-musl`, no libakuma): every
item below is a Linux ABI the programs already expect.

Related: `AKUMA_AMD64_RIO_FBDEV_BUILD.md` (building and running rio, the
session-by-session findings), `AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md`,
the demo repo's README (`netoneko/akuma-cli-wgpu`, "Things this kernel taught
us"), the plan `docs/runbooks/amd64-fbdev-wgpu-demo.md` (branch
`cats/meow/fbdev-wgpu-plan` of `netoneko/akuma-litter`; copied into the demo
repo as `docs/fbdev-wgpu-plan.md`).

## 1. Done: a Linux fbdev device (`/dev/fb0`)

The whole stack draws through one device: the demo directly, rio through the
wgpu backend's `Surface` (which presents whole rows into the mapping). Built
in six slices, all in this repo:

| slice | what | where |
|---|---|---|
| S1 | framebuffer geometry kept in a static at boot (multiboot2 tag; GRUB picks the mode, 3840×2160×32 here) | `amd64/src/multiboot2.rs` |
| S2 | a write-combining memory type for user page tables (`MemAttr::WriteCombine`); ~2.9–3.0 GB/s for whole-row copies, ~71 MB/s scattered | `crates/akuma-mmu` |
| S3 | `/dev/fb0` (char 29:0), single owner: a second open is `EBUSY` | `amd64/src/fbdev.rs`, `crates/akuma-fbdev` (`open_decision`) |
| S4 | `FBIOGET_VSCREENINFO`, `FBIOPUT_VSCREENINFO` (identical request OK, mode change `EINVAL`), `FBIOGET_FSCREENINFO` (`id = "akuma-fb"`), `FBIOPAN_DISPLAY` no-op | `crates/akuma-fbdev` (`Geometry::var/fix/put_accepted`) |
| S5 | device-backed `mmap` of the pixels | `crates/akuma-fbdev` (`mmap_check`) |
| S6 | the screen goes back to the console (fbcon) on close, exit or panic; console muted while a program owns it | `crates/akuma-fbcon` |

Verified from userspace: `probes/fbprobe.c` in the demo repo (ioctl dumps
byte-identical to the plan), the demo at 46–50 fps 4K software / ~35 fps
through wgpu, rio rendering the terminal.

Userspace consequence worth knowing: because the device is single-open, rio's
window platform opens it once and hands the fd to the wgpu backend through
`AKUMA_FB_FD`.

## 2. Open, blocking real terminal use

### 2.1 Ptys — `/dev/ptmx` + `/dev/pts/N` (highest value) — **DONE 2026-10-05**

Implemented on both kernels: `crates/akuma-pty` + `akuma-syscalls-glue/src/pty.rs`;
full record `AKUMA_AMD64_PTY.md`. The text below is the original problem statement.


There are none (`ls /dev` = `fb0 null random tty urandom zero`; an `ssh -tt`
session is "not a tty" either). Every terminal emulator's `openpty`/`forkpty`
fails; rio silently ran a dead terminal until this was found. rio now falls
back to a **userspace pipe pty** (shell on pipes, a relay thread doing a
cooked line discipline) — typing and commands work, but `isatty` is false in
the shell: no job control, no line editing, and **full-screen programs (vi,
top, less) and `ssh` from inside rio do not work.**

The full spec (call order musl's `openpty` uses, ioctls, readiness, signals)
is in `AKUMA_AMD64_RIO_FBDEV_BUILD.md`, "Kernel spec: Linux ptys". The line
discipline already exists (`crates/akuma-terminal::TerminalState`); what is
missing is the device nodes, master/slave fd types, `TIOCGPTN`/`TIOCSPTLCK`/
`TIOCSCTTY`/`TIOCSWINSZ`(+SIGWINCH), and epoll readiness on the master.

### 2.2 Alt on the USB keyboard — **DONE 2026-10-05**

Alt+key = `ESC` + key in `akuma_usb::hid` (xHCI keyboard; 3 new tests in
`crates/akuma-usb/tests/navigation_keys.rs`) and in the PS/2 driver
(`amd64/src/kbd.rs`, `ALT_FOLLOW`). Not yet seen on the panel (§6).


`crates/akuma-usb/src/hid.rs` `emit_key` consults only Shift and Ctrl, so
**Alt is dropped**: Alt+D arrives as `d`. Linux consoles send `ESC` before the
key while Alt is held ("meta sends escape"). Fix: when `MOD_LALT|MOD_RALT` is
set and the key produced a byte, emit `0x1b` first. rio's window platform
already decodes `ESC`+key as Alt+key (that is how it works over ssh today);
on the console users press Esc, then the key.

Super/GUI cannot be expressed in a tty byte stream at all; it would need
evdev (`/dev/input/event*`), which is out of scope.

### 2.3 Shell / process lifecycle

**2026-10-05 findings** (`userspace/forktest/c_stress/lifeprobe.c`, QEMU and
the trashcan's old kernel): every close-on-exec creation path is honoured
across fork+exec — `socketpair(SOCK_CLOEXEC)`, `socket(AF_UNIX, SOCK_CLOEXEC)`,
`pipe2`, `open`, `F_DUPFD_CLOEXEC`, `dup3`, `F_SETFD`, `posix_openpt` — so
rio's leaked fds 4/5 did not come from the kernel's CLOEXEC handling (their
origin is unexplained; rio's fd sweep stays). SIGCHLD reached a handler 5/5.
Orphans, including those of a SIGKILLed parent, are reparented to 1 and reaped
by a reaping init (herd's sweep does it on the box). **Fixed:** an exited,
unreaped process reported `State: R (running)` in `/proc` on amd64 (the exit
epilogue never set `ProcessState::Zombie`), which makes a zombie whose parent
never waited look exactly like "a shell hung in its own exit"; it reports `Z`
now. The exit hang and the `/proc` readdir wedge were not reproduced.


* **`SOCK_CLOEXEC` not honoured**: socketpair fds leak into exec'd children
  (rio's internal sockets appeared as fds 4/5 in every shell). With two panes
  this kept EOF from propagating, so exiting a shell never closed its pane.
  rio now closes fds 3..1023 in the child as a workaround. (`pipe2(O_CLOEXEC)`
  looked fine.)
* **A shell can hang in its own exit**: `sh -i` with stdin at EOF printed its
  exit newline, last completed syscall `close(0)`, then `State: R` forever,
  never a zombie; the parent never got SIGCHLD. Seen once in four runs.
* **SIGCHLD not reliably delivered**: rio's reactor waits for SIGCHLD
  (signal-hook self-pipe); in at least one run a shell was reaped without the
  reactor ever waking. rio now treats EOF on its end as "shell gone".
* **Orphans are never reaped**: a child whose parent is killed stays a zombie
  (`PPid` = the dead parent), nothing reparents it to init.
* **`/proc` readdir wedges**: walking `/proc/[0-9]*` hangs while a process is
  in one of the stuck states above (observed repeatedly; `ls /proc` lists
  thousands of numeric entries, many not live processes).

### 2.4 Filesystem

* **Concurrent `O_APPEND` writers lose writes** (reproducible: 298 of 300
  lines from two `>>` loops) and **cross-file data corruption** after a
  truncate (seen once). Details and hypotheses:
  `AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md`.

## 3. Open, worked around in userspace

* **FIXED 2026-10-05:** console `VMIN=0` reads return 0 at once (`VTIME=0`) or after `VTIME` tenths (glue's `Stdin`/`DevTty` arm; QEMU: 0 ms and 304 ms). Glue's `TCGETS`/`TCSETS` also wrote `c_cc` one byte early (byte 16, `c_line`) and now match Linux and amd64's own arm.
  Original: **Console read ignores `VMIN=0`/`VTIME`** (blocks forever); `O_NONBLOCK`
  is honoured, so rio's window platform keeps the console fd non-blocking and
  polls in 33 ms slices. A process blocked in a console read was seen to
  wedge `/proc` too.
* **One terminal state per console session**: interactive children share the
  console shell's termios (`spawn_inherits_terminal`). A raw-mode program
  killed with SIGKILL leaves the console raw — no NL→CRLF for everyone after
  it. (Linux shells re-sane the tty at each prompt; the console shell could do
  the same.) `stty sane` repairs it; rio heals a raw state it finds at start.
* **Signal delivery that returns to userspace** (observed 2026-10-03 with the
  demo, after `AKUMA_AMD64_SIGNAL_DELIVERY.md`; current status not re-checked):
  masked signals still EINTR the in-flight syscall; after a handler returns,
  the process died with SEGV_MAPERR at mapped addresses (address space not
  intact across delivery); `sigpending`/`sigtimedwait`/`signalfd` ENOSYS;
  `setitimer` fires once; the ucontext layout differs from musl's (RIP read
  as 0 at musl's offsets). The demo's SIGINT/SIGTERM handler `_exit(0)`s
  immediately; std `Instant::now` aborts on EINTR, so the demo has its own
  EINTR-retrying clock.

## 4. Performance characteristics that shaped the backend

Not bugs, but the software GPU is designed around them (measured with the
demo's `thread-probe` and `AKUMA_PROF`):

* **Thread wake-up latency ≈ a scheduler tick** (~4.3 ms when the CPU is
  busy; idle CPUs are evidently not sent an IPI). The backend's worker pool
  spins for 50 ms before parking. `sched_yield` made it worse.
  `sched_setaffinity` returns -1.
* **Thread creation ≈ 1.5 ms**, so `std::thread::scope` per parallel phase is
  too expensive; workers are created once.
* **First-touch page faults cost several µs each** (a 2.3 MB `Vec` filled by a
  worker: 3–5 ms), so per-draw temporaries are recycled.
* **Memory write bandwidth ~5–10 GB/s**: a 4K clear is 3–6 ms regardless of
  thread count. The WC framebuffer takes ~11 ms for a full 33 MB 4K present.

## 5. Priority, for whoever picks this up

1. Ptys (§2.1) — unlocks full-screen programs and ssh inside rio, and retires
   the userspace fallback.
2. `O_APPEND` atomicity and the ext2 corruption (§2.4) — data loss.
3. Alt → ESC prefix (§2.2) — a few lines, big usability win on the panel.
4. `SOCK_CLOEXEC`, SIGCHLD delivery, exit hang, orphan reaping, `/proc` wedge
   (§2.3).
5. Console `VMIN`/`VTIME`, signal return path (§3).

## 6. 2026-10-05 session: what landed and what was verified

| item | change | host tests | QEMU (amd64) | trashcan |
|---|---|---|---|---|
| ptys | `akuma-pty`, glue `pty.rs`, `PtyMaster`/`PtySlave`, `Process::ctty`, amd64 pgid/sid arms | 41 + readiness | `ptyprobe` 31/31 at SMP=1 and SMP=4 | `ptyprobe` **31/31** on kernel `8bece079` (old kernel: `posix_openpt` ENOENT). rio itself not yet run |
| `O_APPEND` atomic | `Filesystem::append`, ext2 `write_locked`, glue `sys_write` | race test fails 3/3 on old path, passes | — | old kernel: 273/271/264 of 300 lines; **new kernel: 300/300 ×3** |
| ext2 stress | two host tests incl. `e2fsck -fn` oracle | pass | — | **corruption reproduced on old kernel** (corruption doc §9), cause open |
| Alt → ESC | `akuma_usb::hid`, `amd64/src/kbd.rs` | 3 new | — | not verified at the panel |
| zombie state | amd64 exit sets `ProcessState::Zombie` | — | `lifeprobe` 21/21 | `lifeprobe` all passed (console check skipped over ssh) |
| console VMIN/VTIME + glue termios offset | glue `fs.rs`/`term.rs` | — | `lifeprobe` | n/a over ssh |
| fbcon handback on failed open | code read: `EBUSY` returns before any state change; `release` is CAS on owner | — | — | not verified |

Remaining userspace workarounds in rio: the pipe-pty fallback becomes dead code
once `openpty` succeeds on the box (keep it until verified there); the fd
3..1023 sweep (cause not found in the kernel); "EOF on its end = shell gone"
(still sensible); O_NONBLOCK + 33 ms slices on the console (still works; the
kernel now honours `VMIN=0` too, returning 0, which rio already treats as
"none").

Deployed 2026-10-05 as `/boot/akuma-amd64` (md5 `19258767…`, verified after a
cold boot); the previous kernel is `/boot/akuma-amd64.prev` (`9cc33e81…`). Not
done: `/dev/ptmx` and `/dev/pts` do not appear in `ls /dev` (they are opened by
name only), rio has not been run against the new kernel (the
`/tmp/rio-expect.sh` checks in the handoff remain), and Alt has not been tried
at the panel.

**rio, 2026-10-05, on kernel `8bece079`** (`expect` over `ssh -tt`): rio took
the real pty path — no `create_context failed`, no pipe-pty fallback in
`/tmp/akuma-fb.log`; inside rio `tty` = `/dev/pts/0`, `stty size` = `63 239`,
`vi` started full screen; `SIGTERM` to rio hung up its shell. Not checked:
the scripted `vi` quit (ESC and `:wq` in one burst; a scripting issue, not
re-run), `ssh late.sh` inside rio, Alt at the panel. The `/dev/ptmx` +
`/dev/pts` listing is in the tree and passes `ptyprobe` 38/38 in QEMU, but is
**not installed on the box**: the install's renames were lost across
`reboot -f` (corruption doc §11).
