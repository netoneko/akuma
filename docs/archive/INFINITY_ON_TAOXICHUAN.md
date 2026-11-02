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
| guest | `herd vm-run`: 4 vCPUs (`smp=4`), 3072 MiB, `outbound=1`, `forward=2225:22`, resolver `10.0.2.3` |
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
- Files the guest creates show an mtime of `Jan 1 1970`, although `date` in the guest is correct
  (`epoch=` on the command line).

## Results

_Pending._

## Background

- `docs/archive/TAOXICHUAN_FIXED_POINT.md`: the kernel used here.
- `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`, "post cleanup verification": the previous GLM
  run on this box (Shanghai).
- `scripts/kvm/taoxichuan/fp_metal.py`: how guests are booted and polled on the box.
