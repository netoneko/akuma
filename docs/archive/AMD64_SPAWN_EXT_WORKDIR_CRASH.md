# `herd`'s `workdir`, and the `SPAWN_EXT` call that kills the machine

**Status: OPEN.** The `workdir` key is implemented in `userspace/herd` and is
**not usable** — every service that sets it takes the kernel down. Found
2026-09-20 on the Ryzen Firecracker guest while giving `llama-server` a working
directory. Nothing here is a regression: `SPAWN_EXT` has always had a `cwd`
field; this is the first unboxed caller to use it.

## Why a working directory was needed at all

`herd` is `init`, so its working directory is `/`, and a spawned service
inherits whatever the kernel gives it. `llama.cpp` scans for its ggml backend
libraries **relative to the working directory**. Started from `/` on the
bare-metal box, that walk descends into `/src` — an entire source tree.

Measured, `[PSTATS]` for the stuck process:

```
[PSTATS] PID 19 (/bin/llama-server) 30.81s: 5176 syscalls ... nr4=4480(1801ms) nr217=140(16ms) getdents64=140 openat=1
[PSTATS] PID 19 (/bin/llama-server) 60.86s: 5176 syscalls ...   (identical — it stopped issuing syscalls entirely)
```

4480 `stat` calls, 140 `getdents64`, and the model opened **zero** times. No
listener, one second of CPU, and a process that looks exactly like a model
loading slowly. `litter/yard_akuma.sh` did `cd /tmp` before its `nohup`, which
is the only reason the same binary and model came up in five seconds there.

## What was built

`ServiceConfig.workdir`, parsed from `workdir =` (or `working_dir =`) in a unit
file, passed to the child.

**A `chdir` in herd around a plain `spawn` does not work**, and the failure is
silent — worth stating because it is the obvious first implementation. The child
does **not** inherit the caller's working directory: the kernel assigns it the
one in `SpawnOptions`, defaulting to `/`. Verified with a `pwd` oneshot service,
which printed `/` with the chdir in place — the same answer a no-op produces.

So the mechanism has to be `SPAWN_EXT` (315), whose `SpawnOptions` carries
`cwd_ptr`/`cwd_len`. `herd` already uses that call for **boxed** services
(`spawn_in_box`, always with a real `box_id` and `cwd = "/"`).

## The crash

Any service with a `workdir` — i.e. any unboxed `SPAWN_EXT` with `box_id = 0` —
kills the guest:

```
[herd] Starting service: pwdprobe
[herd2026-09-20T15:16:26 [anonymous-instance:fc_vcpu 0] Unexpected exit reason on vcpu run: Shutdown
Error: RunWithoutApiError(Shutdown(GenericError))
```

A vcpu `Shutdown` is a **triple fault**. There is no panic line and no
`[Fault]`: the console dies mid-print, in the middle of herd's own `[herd] `
prefix.

**The failure moves with the input, which is the interesting part:**

| unit | what happened |
|---|---|
| `workdir = /tmp` | died **at** the `pwdprobe` spawn |
| `workdir = /` | `pwdprobe` **started** (pid 58), then died at the **next** spawn (`sshd`) |
| no `workdir` (same unit otherwise) | boots fine, service runs, oneshot completes |

Same binary, same unit, same everything else. A fault that lands one spawn later
when an argument changes is the shape of **state being corrupted and consumed
afterwards**, not of a bad pointer being dereferenced on the spot — but that is
an inference from two data points, not a diagnosis.

## What has been ruled out

* **Not `parse_argv_array` on a null pointer.** It guards `ptr == 0` at
  `crates/akuma-syscalls-glue/src/proc.rs:1565`.
* **Not the path value.** `/` crashes as surely as `/tmp`, just later.
* **Not the `oneshot` path.** The identical unit without `workdir` runs to
  completion and herd logs `Oneshot service pwdprobe completed`.
* **Not herd's config parsing.** The unit parses; herd prints
  `Starting service: pwdprobe` and `Started pwdprobe (pid= 58)`.

## What to look at next

The one structural difference between this call and the boxed calls that have
always worked is **`box_id = 0`** on a `SPAWN_EXT` that also supplies a `cwd`.
In the handler, `box_id == 0` means "inherit the caller's box" and deliberately
skips the access check; the value is then passed straight to
`akuma_exec::process::spawn_process_with_channel_ext(..., cwd_ref, o.box_id,
false)`. Whether that function can take `box_id = 0` **together with** an
explicit cwd is the first thing to establish — every existing caller supplies
either a real box id (boxed services, `box run`) or no cwd at all.

A probe belongs in `userspace/amd64/`, calling `SPAWN_EXT` directly with the
four combinations of `{box_id 0, box_id N} × {cwd None, cwd Some}`, so the
crashing cell is identified without herd in the picture. Do it on the
Firecracker guest, not the bare-metal box: the image is a file on the host, so a
dead guest costs a `debugfs` write with the VM stopped rather than somebody
walking to a machine.

**A userspace argument must not be able to triple-fault the kernel**, whatever
the argument is. That is the bug, independent of whether herd's use of the call
is correct.

## Workaround in place

`llama` is launched through a script instead:

```
command = /bin/sh
args = /root/llama-start.sh          # "cd /tmp" then "exec /bin/llama-server ..."
```

`sh -c "cd /tmp && exec ..."` does **not** work here: herd splits `args` on
whitespace with no quoting, so a command string cannot survive as one argument.
A script file is the shape that fits what herd can express today.

## Background

* `userspace/meow/litter/herd_akuma.sh` — the litter's service definitions,
  including the launcher above and the reasoning for `-t 1`.
* [`HERD_PLUS_BOX.md`](HERD_PLUS_BOX.md) — the `SpawnOptions` layout and why the
  kernel and userspace each keep their own copy of it.
* [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) appendix —
  why services belong under herd at all on this target (nothing reaps an orphan,
  so an agent started from an ssh session becomes an unkillable-looking zombie).
