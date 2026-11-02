# Infinity on Taoxichuan: GLM strips AWS from an agent runtime and builds it, under Akuma's hypervisor

2026-11-02. An unattended run: goose with `glm-5.3-flash`, inside a Taoxichuan guest, on the ryzen booted into
Akuma on bare metal. The model is given https://github.com/hydro-project/infinity, a Rust agent runtime, and has to:

1. clone and analyze it;
2. cut out every AWS dependency;
3. build what is left;
4. prove the result talks to `glm-5.3-flash`: a PONG reply, plus `uname -a` run through infinity's own tool path.

The order is fixed: **cut first, then build**. A clean clone pulls about 700 crates. Fewer crates is the point of
the experiment, and so is the build time saved.

Status: **running**. Results below are filled in as they land.

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
4 GiB.

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

And three that are open:

- **Eager mmap.** Taoxichuan allocates and zeroes every page at `mmap` time and copies file pages per
  process, so each `rustc` holds a private copy of the 311 MB `librustc_driver`. At `-j3` on 3 GiB, a
  rustc's thread-stack `mmap` got `ENOMEM`, which rustc reports as an ICE (`failed to spawn work thread`).
  The amd64 kernel moved to the akuma-mmap region table and lazy, shared file pages (fpcache) in
  2026-09; Taoxichuan has neither.
- **Stale process rows.** After a multi-threaded abort, and even after `SIGKILL`, rows stayed in `ps`
  (state R, `VmSize` 0, fds 0/1 open, `kill -0` succeeding) for pids cargo had already reaped.
- **sshd as init miscounts.** It reaps with `wait4(-1)`, but counts every reaped orphan as a finished
  session (`live_sessions`). herd's `sweep_reaped` is the reaper to share.

`e2fsck` after the first restart also found 42,815 bad dirents (the dirent bug above). After the hard stop
it found nothing structural: 336 leaked inodes (unlinked files still open when the guest died; ext2 has
no orphan list) and the 15,254 zero `dtime`s.

## Results

_Pending: the build is past every kernel problem and on GLM's own code; the PONG and `uname` checks
follow it._

## Background

- `docs/archive/TAOXICHUAN_FIXED_POINT.md`: the kernel used here.
- `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`, "post cleanup verification": the previous GLM
  run on this box (Shanghai).
- `scripts/kvm/taoxichuan/fp_metal.py`: how guests are booted and polled on the box.
