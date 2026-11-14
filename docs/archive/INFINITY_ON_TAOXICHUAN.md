# Infinity on Taoxichuan: GLM strips AWS from an agent runtime and builds it, under Akuma's hypervisor

2026-11-02. An unattended run: goose with `glm-5.3-flash`, inside a Taoxichuan guest, on the ryzen booted into
Akuma on bare metal. The model is given https://github.com/hydro-project/infinity, a Rust agent runtime, and has to:

1. clone and analyze it;
2. cut out every AWS dependency;
3. build what is left;
4. prove the result talks to `glm-5.3-flash`: a PONG reply, plus `uname -a` run through infinity's own tool path.

The order is fixed: **cut first, then build**. A clean clone pulls about 700 crates. Fewer crates is the point of
the experiment, and so is the build time saved.

Status: **done** 2026-11-03. GLM's port builds and passes PONG and `uname`; the push waits on credentials.

## Setup

| | |
|---|---|
| host | ryzen, Akuma bare metal (KVM-herd GRUB entry, `kvmmem=4096 kvmcores=4-7`), 192.168.1.159 |
| guest kernel | Taoxichuan B15 fixed point, `/root/tx/fp/k1` = `k2` (md5 `3eed0e62...`), reports `0.1.0-85abf06-taoxichuan` |
| guest | `herd vm-run`: 4 vCPUs (`smp=4`), 3072 MiB at launch and 4096 MiB from the first restart, `outbound=1`, `forward=2225:22`, resolver `10.0.2.3` |
| disk | copy of `/root/tx/rs.goose.orig` (Rust nightly 1.101 for `x86_64-unknown-linux-musl` + goose), grown to 4,290,000,000 bytes, about 1.7 GB free |
| agent | goose `--with-builtin developer`, provider openai → `https://api.z.ai/api/coding/paas/v4`, `glm-5.3-flash`, thinking effort low, `GOOSE_MODE=auto` |
| brief | `/root/inf/prompt.md` in the guest (copy on the box: `/root/infinity/prompt.md`) |
| commits | local only, branch `no-aws` in `/src/infinity`, author `tama`; the supervisor pulls them out |
| infinity | `1abc6fcf` (main HEAD at launch) |

The guest has no cmake, no clang and no OpenSSL development files. The brief states this up front, because
`aws-lc-sys` needs the first two and reqwest's `default-tls` (`native-tls` → `openssl-sys`) needs the third. Cutting
them is part of the job. The brief said "no OpenSSL headers or libraries", which was half wrong. OpenSSL's runtime
library is in the image (Python's `ssl` loads 3.5.9). Only the headers and the unversioned `libssl.so` link are
absent, and those are what `openssl-sys` needs.

The image is the box's own root copied into ext2 (`/root/tx/mkimg.sh`: `/bin /lib /sbin /etc`,
`/usr/{bin,lib,libexec,share,include}`, `/usr/local/rust`, then `debugfs -w -f`). So it carries the Akuma dev
environment's whole Alpine userland: git, curl, gcc 15.2, perl, make, pkg-config and python3. goose was added on top.

### Python works, as a side effect of tokio

Taoxichuan was built to run tokio programs, and nobody aimed it at Python. Seven minutes into the run, GLM wanted
to read rig-core's and reqwest's feature tables before cutting anything. It reached for
`python3 -c "import urllib.request; urllib.request.urlretrieve('https://static.crates.io/crates/...')"` and pulled
`rig-core-0.31.0.crate` (2.3 MB), `reqwest-0.13.5.crate` and `rustls-platform-verifier-0.5.3.crate` into `/tmp`
over HTTPS. Checked afterwards in the same guest:

```
$ python3 -c "import sys,ssl,json,sqlite3,hashlib,threading,subprocess; print(sys.version.split()[0], ssl.OPENSSL_VERSION); print(subprocess.run(['uname','-r'],capture_output=True,text=True).stdout.strip())"
3.14.8 OpenSSL 3.5.9 29 Sep 2026
0.1.0-85abf06-taoxichuan
```

That is Alpine's CPython, an unmodified musl binary, running on the syscall surface that was built for tokio, rustc and
cargo. No kernel change was made for it.

### The disk is capped at about 4 GiB

The first attempt grew the image copy to 16 GiB, and `truncate` failed with `I/O error`. The cause:
`akuma-ext2` maps blocks through direct, singly-indirect and doubly-indirect pointers only. A logical block
past 12 + 1024 + 1024² (4 KiB blocks), i.e. 4,299,210,752 bytes, returns `FsError::NotSupported`. The relevant
code is `crates/akuma-ext2/src/ext2.rs`, the block-map lookup and allocation, which end in
`Err(FsError::NotSupported)`.

So a single file on Akuma's own root filesystem, and therefore a guest disk image, cannot exceed about 4 GiB.
4 GiB is enough for this run once the crate graph is trimmed. The brief asks for `CARGO_PROFILE_DEV_DEBUG=0`
and for building only the packages the sanity check needs.

Open: triple-indirect support in `akuma-ext2`. It is needed before a guest disk on this box can be larger than
4 GiB. *(2026-11-03: written, along with the 64-bit file size that the cap also hid, in
[`EXT2_TRIPLE_INDIRECT_AND_LARGE_FILES.md`](EXT2_TRIPLE_INDIRECT_AND_LARGE_FILES.md). It reaches this box with the next
host kernel, at a reboot.)*

### Launch notes

- `setsid /root/inf/go.sh &` over ssh did not start. `setsid sh /root/inf/go.sh </dev/null >/dev/null 2>&1 &`
  (fp_metal's form) did.
- Files the guest creates showed an mtime of `Jan 1 1970`, although `date` in the guest was correct
  (`epoch=` on the command line). Taoxichuan mounted ext2 with a constant-0 clock; fixed in `a0ffa4d4`
  (below).

## What happened

Times are the guest's (UTC, 2026-11-02).

| when | what |
|---|---|
| 20:01 | launch (`go.sh`): clone, analysis, `docs/NO_AWS.md` (`3ec98ed`) |
| 20:15 | `becf10e` removes the AWS crates, their deps and the CDK deployment |
| 20:25 | `13a8526` switches reqwest from OpenSSL to rustls + ring: `Cargo.lock` goes from **664 to 399 packages** |
| 20:26 | `154daae` trims the workspace to the closure the sanity check needs, plus a new `infinity-mcp-shell-tools` crate. Branch total: 156 files, +2,453 / −32,623 lines |
| ~20:30 | first build at `-j3`: a rustc ICE, `failed to spawn work thread: EAGAIN` (out of memory, below) |
| 20:44–20:56 | `-j2` on 3 GiB gets to about 64 crates; the supervisor stops it to restart the guest at 4 GiB |
| 21:26 | the 4 GiB guest will not boot, then boots seeing only 3 GiB: two hypervisor bugs, fixed on the spot |
| 21:38 | resume (`go2.sh`). Every dependency builds; `infinity-agent-cli` fails with `exit status: 1` and no message |
| ~21:50 | GLM bisects the failure to an `E2BIG` at 448 argv entries (its line has 570) and writes a `RUSTC_WRAPPER` that moves the `-L` flags into an `@file`. The Python version cannot exec (no `#!` support); the C version is exec'd with the same 571 arguments |
| ~22:03 | the supervisor's spawn probe wedges the guest kernel (below). Hard stop, `e2fsck`, new guest kernel |
| 22:38 | resume (`go3.sh`) on the fixed kernel. The build passes `infinity-agent-cli` and stops at an ordinary type error in `infinity-provider-rig` |

## Bugs the run found

Each is a kernel or hypervisor defect, not GLM's, and each is fixed:

| bug | symptom in the run | fix |
|---|---|---|
| PVH `start_info` above 4 GiB (EBX is 32 bits) | 4 GiB guest: `no hvm_start_info magic` | `77d9c9ae`, [`KVM_GUEST_4GIB_PVH_HANDOFF.md`](KVM_GUEST_4GIB_PVH_HANDOFF.md) |
| `memmap_entries` always 1 without `add_acpi` | the 4 GiB guest saw 3 GiB | same |
| ext2 dirent type written without `INCOMPAT_FILETYPE`, and read as "file" when 0 | `e2fsck` cleared 42,815 entries; git then saw every directory as untracked | `923e27c3`, [`EXT2_DIRENT_TYPE_WITHOUT_FILETYPE.md`](EXT2_DIRENT_TYPE_WITHOUT_FILETYPE.md) |
| `execve` capped at 448 argv entries | `infinity-agent-cli`'s 570-argument `rustc` line: `E2BIG` | `a0ffa4d4`, [`TAOXICHUAN_EXEC_ARGV_LIMIT.md`](TAOXICHUAN_EXEC_ARGV_LIMIT.md) |
| AF_UNIX `read`/`write` bypassed the record queue | cargo saw `exit status: 1` instead of `E2BIG`, so never took its `@argfile` retry; the supervisor's probe wedged the guest | `caad5003`, [`UNIX_SOCKET_READ_WRITE_BYPASSED_RECORDS.md`](UNIX_SOCKET_READ_WRITE_BYPASSED_RECORDS.md) |
| no `#!` support in `execve` | GLM's Python wrapper: `Exec format error` | `a0ffa4d4`, [`TAOXICHUAN_EXEC_SHEBANG.md`](TAOXICHUAN_EXEC_SHEBANG.md) |
| ext2 clock was a constant 0 | 1970 mtimes; `e2fsck` after the hard stop set `dtime` on 15,254 deleted inodes | `a0ffa4d4` |

Fixed after the run:

| bug | what it did here | fix |
|---|---|---|
| eager `mmap`: every page allocated, zeroed and file-filled at `mmap` | at `-j3` on 3 GiB a `rustc` thread-stack `mmap` got `ENOMEM` (ICE: `failed to spawn work thread`) | [`TAOXICHUAN_LAZY_MMAP.md`](TAOXICHUAN_LAZY_MMAP.md): the same clean `-j3` build on 3 GiB now completes, 278 crates in 790 s |
| no file page shared between processes; `PROT_NONE` and `mmap`'s `PROT_READ` not enforced | each `rustc` held its own copy of every page of `librustc_driver` it touched | [`TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md`](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md) |
| sshd sessions had no `PATH` | gcc: `cannot execute 'cc1': posix_spawnp: No such file or directory` | [`SSHD_SESSION_WITHOUT_PATH.md`](SSHD_SESSION_WITHOUT_PATH.md) |

sshd as init had a miscount: it reaped with `wait4(-1)` and counted every reaped orphan as a finished session
(`live_sessions`). The guest runs herd as init instead now (`init=/bin/herd`, herd and sshd from the `linux-abi`
build, `/etc/herd/enabled/sshd.conf` alone). herd reaps orphans and starts sshd as pid 2. The host's own herd services
that were also enabled on the guest's disk copy are parked in `/etc/herd/disabled-guest/`.

Still open:

- **Stale process rows.** After a multi-threaded abort, and even after `SIGKILL`, rows stayed in `ps`
  (state R, `VmSize` 0, fds 0/1 open, `kill -0` succeeding) for pids cargo had already reaped.

`e2fsck` after the first restart also found 42,815 bad dirents (the dirent bug above). After the hard stop
it found nothing structural: 336 leaked inodes (unlinked files still open when the guest died; ext2 has
no orphan list) and the 15,254 zero `dtime`s.

## Results

GLM finished every step on branch `no-aws` of its clone (`/src/infinity` in the guest):

| commit | what |
|---|---|
| `becf10e` | remove the AWS crates, dependencies and CDK deployment |
| `13a8526` | reqwest: OpenSSL to rustls with the ring provider (the guest has no OpenSSL, cmake or clang) |
| `154daae` | trim the workspace to the closure the agent needs |
| `6f78b4d` | fix the compile errors in its own code that the fixed kernel's diagnostics showed |
| `c6d559d` | provider launcher, smoke test, shell-tool config |
| `f0aef22` | `RESULT.md`: the run report |

- **Size.** 664 to 399 crates in `Cargo.lock`; the workspace went from 21 members to 11.
- **Build.** The closure (daemon, the `infinity` CLI, `infinity-provider-openai`, `infinity-mcp-shell-tools`)
  built at `-j2`, about 18 minutes of build time over six attempts. Three of the attempts failed on kernel
  bugs (the table above).
- **PONG.** `infinity -H 'Reply with exactly the word PONG and nothing else.'` went daemon, provider
  process, `api.z.ai` over rustls/ring and back. The saved thread ends in
  `{"kind":"assistant","content":{"text":"PONG"}}`.
- **`uname -a` through the tool path.** The agent called `shell_invoke_tool` on the MCP shell server,
  which ran `/bin/sh -c 'uname -a'`, and replied with its output:
  `Linux taoxichuan 0.1.0-923e27c3-taoxichuan 923e27c3-release x86_64 GNU/Linux`.
- **Not pushed.** The guest holds no GitHub credentials. The `fork` remote
  (`https://github.com/netoneko/infinity.git`) is configured; the push is the supervisor's to make.
  *(2026-11-04: pushed; see "Update (2026-11-04)" below.)*

Two goose turns ended on supervisor action, not on GLM's: the smoke test blocked on `p.stderr.readline()`
from a provider that writes nothing to stderr, and one turn died on a z.ai `Stream decode error`.

## Update (2026-11-04): pushed, built for both architectures, shipped

**Pushed.** `no-aws` (`3ec98ed`..`f0aef22`, six commits on upstream `1abc6fc`) is on
`https://github.com/netoneko/infinity` as branch `no-aws`. The ryzen was in Pop, so the guest was not running:
the Akuma partition and the guest's `disk.img` were mounted read-only from Pop, the commits were taken out as a
`git bundle` (38 KB; the guest's clone is shallow, and the fork's `main` is already `1abc6fc`), and the branch
was pushed from the Mac as tama. Both mounts were removed afterwards.

**Built on the Mac, release, static musl** (`cargo build --locked --release -p infinity-mcp-shell-tools
-p infinity-provider-rig -p infinity-daemon -p infinity-agent-cli`, the musl cross gcc as CC and linker):
`x86_64-unknown-linux-musl` in 38 s, `aarch64-unknown-linux-musl` in 53 s, from a cold `target/`. The guest's
build was debug and took about 18 minutes over six attempts.

**amd64: shipped to the ryzen, not yet run on Taoxichuan.** `/root/infinity-no-aws/` on Pop holds
`bin/` (the four binaries), `cfg/` (`run-provider.sh`, `providers.json`, `rap.json`, the repo's
`smoke-provider.py`), `README.md` (where each file goes on a guest disk and the three checks) and
`SHA256SUMS` (verified on Pop). `infinity --help` runs there on Pop's Linux. Running it on Taoxichuan is left
for the next agent.

### aarch64: Infinity faulted at start (fixed 2026-11-05)

Staged on a scratch clone of `disk.img` (the same files, the aarch64 build) and booted on aarch64
Taoxichuan (QEMU virt TCG, `smp=4`, 2 GiB, the B21 A6 kernel), `infinity --help` exited 139 with
`[Fault] EL0 ec=0x24 elr=0x00000000006adc58 sp=0x00006ffffffefd70 far=0x00006ffffffefd70 esr=0x92000047 pte=0x0`.
goose 1.52.0 ran fine on the same kernel.

- **Cause.** `elr` is in `tokio::Runtime::block_on::<infinity::main::{closure}>`, whose frame is the whole
  `async main` future (0xb980 bytes, 46 KB). Its stack probe (`sub sp, sp, #0x1000; str xzr, [sp]` in a loop)
  writes each page going down, and the first write past the 64 KiB every image starts with
  (`USER_STACK_PAGES`) hit an unmapped page. amd64 grows the stack on that fault (`grow_user_stack`, up to
  8 MiB); the aarch64 half had no growth at all, so the task was killed before `main`. Any `#[tokio::main]`
  program with a large main future dies the same way. tokioprobe and goose passed only because their main
  frames fit.
- **Fix.** `grow_user_stack` moved into `akuma-tokio-kernel-core::loader`, generic over `Loader`. It uses only
  what core already has: `Mm::with_task`, the layout's `growth_below`/`extend_down`, `FreshPage`, and
  `UserTree::map_page`. amd64 calls it where it called its own copy, and aarch64's EL0 data abort calls it on a
  translation fault. One zeroed frame per page grown, no heap.
- **Verified.** A probe (`stackprobe`: a 256 KiB frame first touched by a user store, by `read` into it, and
  by `write` from it, each in a fork child) passes all three on aarch64. `infinity --help` runs. Infinity's
  PONG went daemon, provider, z.ai and back (`[Text] P`, `[Text] ONG`), and a tool turn called
  `shell_invoke_tool` -> `run_command {"command":"uname -m && nproc"}` (status 200) and answered `aarch64`,
  `4`. 0 kernel faults.
- **Not changed.** Growth stays from user faults only, on both machines. A syscall that is the *first* to touch
  a new stack page gets `EFAULT`, where Linux grows the stack. It needs a raw syscall from a leaf frame (a libc
  call touches the lower stack first), so it is latent; the probe above shows it does not arise through libc.

### On the main Akuma kernel (aarch64, 2026-11-05)

The same aarch64 build on the main kernel (`cargo build --release`, smp-shared, `SMP=4`, HVF, 2 GiB, the
scratch disk):

- **The daemon needs `INFINITY_WS_PORT`.** Its WebSocket server binds `127.0.0.1:8080` by default, and the main
  image's herd runs `/bin/httpd` there, so the daemon exits with `Address in use (os error 98)`, which is the
  correct answer. With `INFINITY_WS_PORT=8090`: PONG, and tool turns that returned
  `turn-1\naarch64\n0.00 0.00 0.00 16/16 264` through `run_command`.
- **Load.** 13 sessions on one daemon, six of them started within a second (each with its provider process, an MCP
  proxy RAP server and, when the model delegated, `spawn_thread` children): no kernel faults, and ssh stayed
  up. The failures in that run were z.ai's `429 Too Many Requests` (code 1302) on a key other agents share, and
  glm-5.3-flash choosing `spawn_thread` over calling the tool directly.
- **Harness trap.** `infinity -H` returns as soon as the session exists, and the daemon logs
  `Session <id> is idle but a client is still connected` at creation. Neither marks the end of a turn.
- Getting there also found a **boot-suite failure** on the main kernel, unrelated to Infinity:
  `docs/archive/SIGNAL_FATAL_DEFAULT_LINUX.md`, "Correction (2026-11-05)".

## Background

- `docs/archive/TAOXICHUAN_FIXED_POINT.md`: the kernel used here.
- `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`, "post cleanup verification": the previous GLM
  run on this box (Shanghai).
- `scripts/kvm/taoxichuan/fp_metal.py`: how guests are booted and polled on the box.
