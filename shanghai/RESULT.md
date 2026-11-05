# Shanghai — result

Kernel built from `crates/akuma-tokio-kernel/README.md` per
`scripts/kvm/goose/shanghai-01-new-kernel.md`, on the ryzen booted into **Akuma**
bare metal (no QEMU, no Pop). Branch `shanghai`, cut from `origin/rust-org-il-experiment`
(`2b9c7abd`); one commit per passing gate, nothing pushed (supervisor fetches from this box).

Files: `shanghai/Cargo.toml`, `shanghai/build.rs`, `shanghai/src/main.rs`,
`shanghai/src/banner.txt` (78x20 limit respected: 19 lines, 75 columns max, plain ASCII),
plus the one `members` line in the root `Cargo.toml` and the `Cargo.lock` entry the build adds.
All images, keys, wrappers and logs live outside the tree under `/root/shanghai/`.

**`shanghai/src/main.rs` is 18 lines** (`wc -l shanghai/src/main.rs`).

Environment for every cargo command: `. /etc/akuma-dev.env`, builds with `-j4`.
Kernel: `cargo build -p shanghai --target x86_64-unknown-none --release` →
`target/x86_64-unknown-none/release/shanghai` (2.29 MB ELF, statically linked).

---

## Gate 1 — build: PASS

```
. /etc/akuma-dev.env && cargo build -p shanghai --target x86_64-unknown-none --release -j4
```

```
   Compiling shanghai v0.1.0 (/src/shanghai-wt/shanghai)
   Compiling akuma-runtime-tokio v0.1.0 (/src/shanghai-wt/crates/akuma-runtime-tokio)
    Finished `release` profile [optimized] target(s) in 32.54s
EXIT=0
```

Caveat, recorded for honesty: the *first*, cold build prints one warning, and it comes from the
`akuma-exec` dependency, not from shanghai:

```
warning: the feature `allocator_api` has been partially stabilized since 1.100.0 and is succeeded by the feature `allocator_ext`
warning: `akuma-exec` (lib) generated 1 warning
```

`shanghai` itself compiles warning-free (a rebuild that only recompiles shanghai prints no warnings
at all). The brief forbids touching any file outside `shanghai/` + the members line, so the
pre-existing `akuma-exec` note was left as is; it also appears building `-p osaka` on this branch.

## Gate 2 — minimal image: PASS

Per the README's "smallest disk" steps, with `E=/sbin` for mke2fs/e2fsck and `/usr/sbin/debugfs`
for debugfs (see deviations #2). sshd built per README:

```
(cd userspace && cargo build -p sshd --release --target x86_64-unknown-none --features libakuma/linux-abi -j4)   # EXIT=0
```

busybox 1.35.0 static musl downloaded from busybox.net (1,131,168 bytes). There is no `ssh-keygen`
on Akuma (see deviations #3): the login key is the box's own raw-seed ed25519
(`/root/.ssh/id_ed25519`, the key `scripts/kvm/taoxichuan/fp_metal.py` names as the default guest
key), copied to `/root/shanghai/w/key`; its public line was derived with a pure-Python ed25519
implementation and verified against the box's own `/etc/sshd/id_ed25519(.pub)` pair before use:

```
$ ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIfAjn7pW7HAAcdIS3mcvvOzME1EddOvCCdNUO2ZufEB shanghai
```

```
$ /sbin/mke2fs -q -F -t ext2 -b 4096 /root/shanghai/w/shanghai.img 64m
$ /usr/sbin/debugfs -w -f cmds shanghai.img        # mkdir /bin /etc /etc/sshd /tmp;
                                                   # write sshd/busybox/key.pub + sif 0100755;
                                                   # symlink /bin/sh /bin/busybox
$ /usr/sbin/debugfs -R "ls -l /bin" shanghai.img
     16  100755 (0)      0      0   127280  sshd
     17  120777 (0)      0      0      12  sh
     19  100755 (0)      0      0  1131168  busybox
$ /usr/sbin/debugfs -R "cat /etc/sshd/authorized_keys" shanghai.img
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIfAjn7pW7HAAcdIS3mcvvOzME1EddOvCCdNUO2ZufEB shanghai
$ /sbin/e2fsck -fn shanghai.img   ->  19/16384 files, fsck clean
```

All four required files present: `/bin/sshd`, `/bin/busybox`, `/bin/sh` (symlink), `/etc/sshd/authorized_keys`.

## Gate 3 — boot: PASS

`herd vm-run` spec (Akuma's own hypervisor; the README's metal spec as written):

```
type=kvm
kernel=/root/shanghai/w/shanghai
cmdline=init=/bin/sshd initargs=--port,22 smp=4 epoch=1793645322
ram_mib=3072
vcpus=4
disk=/root/shanghai/w/shanghai.img
disk_ram=0
outbound=1
forward=2246:22
```

Console (`grep -a` on the vm.log `herd vm-run` writes), captured on this boot:

```
Shanghai/amd64 -- long mode reached
  uart: present
  machine: 3071 MiB RAM, 1 memmap entries, 2 virtio-mmio slot(s)
...
[Block] Registered vda: 64 MB (131072 sectors)
  fs:   ext2 root on /dev/vda
[SmolNet] Found virtio-net at slot 1
  net: DHCP configured, 10.0.2.15
  smp: 4 cpus online
  smp: 4 of 4 requested


            |                                                      ___
            |                   |                                 /   \
           (O)                 /_\           ___________         /     \
            |                  | |          |  _______  |       /       \
         _____                /___\         |  /     \  |      /         \
        /     \               |   |         |   /   \   |     |           |
        \_____/              /_____\        |    /_\    |     |           |
           /|\               |     |        |           |     |___________|
          /   \             /_______\       |           |     |           |
         /     \            |       |       |           |     |           |
        /       \          /_________\      |           |     |___________|
       /         \         |         |      |___________|     |           |
      /           \        |         |      |           |     |           |
     /             \       |         |      |           |     |___________|
     |      |      |       |         |      |           |     |           |
     |      |      |       |         |      |           |     |           |
 __________________________________________________________________________
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

  Shanghai 0.1.0 (akuma-tokio-kernel, amd64)
Shanghai: init spawn -- init=/bin/sshd
Shanghai: entering ring 3 -- /bin/sshd
[init] pid 1 on pool slot 1
[SSHD] Listening on 0.0.0.0:22...
```

`grep -a -c -E "\[Fault\]|PANIC" vm.log` → **0**. (The banner prints just before init starts,
after the SMP lines, as `.banner()`'s docs say.)

## Gate 4 — inside: PASS

The Akuma ssh client takes no `-o` options (see deviations #6); the working command on this box:

```
$ /bin/ssh -i /root/shanghai/w/key -p 2246 root@127.0.0.1 'busybox uname -a'
Linux shanghai 0.1.0-2b9c7abd-shanghai 2b9c7abd-release x86_64 GNU/Linux
```

## Gate 5 — tokio: PASS (1 core and smp=4)

`ARCH=x86_64 ./build-musl.sh` does not run on this machine (deviations #7); its cargo step was
run verbatim after supplying the toolchain it names from the box's gentoo musl gcc
(`/opt/gentoo/root`, gcc 16) via wrappers:

```
export PATH=/root/shanghai/w/bin:$PATH
export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
export AR_x86_64_unknown_linux_musl=x86_64-linux-musl-ar
cd userspace/tokioprobe && cargo build --release --locked --target x86_64-unknown-linux-musl -j4
# -> bootstrap/bin/tokioprobe.x86_64  (2,286,568 bytes, ELF x86-64, static-pie, stripped)
```

Image updated per README's cmds2 (write /bin/tokioprobe 0100755, symlink /bin/uname,
`nameserver 10.0.2.3` as /etc/resolv.conf), then two boots:

smp=4 (`cmdline=... smp=4`, `vcpus=4`):

```
$ /bin/ssh -i key -p 2246 root@127.0.0.1 'TOKIOPROBE_TIMEOUT=300 /bin/tokioprobe'
RUNTIME PASS multi-thread workers=4
UNAME PASS Linux shanghai 0.1.0-2b9c7abd-shanghai 2b9c7abd-release x86_64 GNU/Linux (0.00s)
HTTPS PASS https://akuma.sh/ status=200 HTTP/1.1 bytes=20595 (0.35s)
  title: AKUMA // SYSTEM ONLINE
RESULT PASS
```

one core (`cmdline=... smp=1`, `vcpus=1`):

```
RUNTIME PASS multi-thread workers=1
UNAME PASS Linux shanghai 0.1.0-2b9c7abd-shanghai 2b9c7abd-release x86_64 GNU/Linux (0.00s)
HTTPS PASS https://akuma.sh/ status=200 HTTP/1.1 bytes=20595 (1.33s)
  title: AKUMA // SYSTEM ONLINE
RESULT PASS
```

## Gate 6 — goose: PASS

Image built by the procedure of `scripts/kvm/taoxichuan_goose_image.sh`, adapted to this machine
(deviations #9-#11; the rs.goose.orig fallback was NOT needed). Boot spec = gate 3's with
`disk=/root/shanghai/w/goose.img` (512M ext2, `fsck clean`), same Shanghai kernel. Inside:

```
$ /bin/ssh -i key -p 2246 root@127.0.0.1 'uname -a && . /etc/goose.env && echo ENV_OK && cat /etc/resolv.conf'
Linux shanghai 0.1.0-2b9c7abd-shanghai 2b9c7abd-release x86_64 GNU/Linux
ENV_OK
nameserver 10.0.2.3
```

```
$ /bin/ssh -i key -p 2246 root@127.0.0.1 \
    'nohup sh -c ". /etc/goose.env; goose run --with-builtin developer -t \"Reply with exactly one word: PONG\" > /tmp/goose.out 2>&1; echo RC=\$? >> /tmp/goose.out" >/dev/null 2>&1 & echo LAUNCHED'
LAUNCHED
# ~45 s later:
$ /bin/ssh -i key -p 2246 root@127.0.0.1 'tail /tmp/goose.out'

    __( O)>  ● new session · openai glm-5.3-flash
   \____)    20261102_1 · /
     L L     goose is ready
PONG
RC=0
```

`goose run` answers **PONG** — tokio (runtime, TLS, DNS, child processes) works end to end on
Shanghai, on this machine's hypervisor.

---

## Every place the README (and friends) were wrong, unclear or missing a step here

1. **Toolchain setup missing**: every cargo/rustc command needs `. /etc/akuma-dev.env` first
   (nightly rustc is a shim needing `LD_LIBRARY_PATH`; nothing on Akuma's sshd sessions sets it),
   and builds need `-j4`. The README's build section says neither.
2. **README "on Linux they are on `PATH`"** — not here: `mke2fs` is `/sbin/mke2fs`, `debugfs` is
   `/usr/sbin/debugfs` (not on the default PATH of this busybox userland). All e2fsprogs calls
   above use full paths.
3. **README step 3 `ssh-keygen -q -t ed25519 ...`** — no `ssh-keygen` exists on Akuma. Used the
   box's existing raw-seed ed25519 key (`/root/.ssh/id_ed25519`, the documented default guest
   key in `fp_metal.py`) and derived the `authorized_keys` line with a pure-Python ed25519
   (correctness proven by reproducing the box's own published `/etc/sshd/id_ed25519.pub` byte
   for byte). Note the Akuma `/bin/ssh` wants this 32-byte raw seed as `-i`, not an OpenSSH file.
4. **README boot section, QEMU line** — impossible here: there is no QEMU on this machine.
   The `herd vm-run` spec is what was used (and it worked exactly as printed, incl.
   `force-legacy` not applying under KVM/SVM and `disk_ram=0`).
5. **README "Run tokio on it"**: `scripts/kvm/taoxichuan/tcg_rig.py run /bin/tokioprobe 300` —
   the `300` is the rig's timeout argument, not a tokioprobe argument. Run bare as first command,
   `/bin/tokioprobe 300` makes reqwest build with URL `300` → `HTTPS FAIL send: builder error:
   relative URL without a base`. Correct invocation: `/bin/tokioprobe` (optionally
   `TOKIOPROBE_TIMEOUT=300`). The README never says what the trailing number of the rig line is.
6. **Boot instruction `ssh ... -o StrictHostKeyChecking=no root@127.0.0.1`** (also in the
   goose-image script header and fp_metal.py's guest() helper) — Akuma's `/bin/ssh` takes no
   `-o` options (it parses `-o` as a hostname). Its usage is
   `ssh [-p port] [-l user] [-i identity_file] [user@]host [command]`; the first connection
   answers its `yes/no` host-key prompt interactively ('yes' can be piped through stdin) and the
   key persists in `/root/.ssh/known_hosts` as `127.0.0.1:2246 ...`. When a rebuilt image
   changes the guest host key, the client refuses with
   `REMOTE HOST IDENTIFICATION HAS CHANGED` and names the fix (remove that line) — done per boot
   of a fresh image.
7. **`ARCH=x86_64 ./build-musl.sh`** — fails on this machine three ways: (a) no `rustup` binary
   (script asserts `rustup target list --installed`; the box has a standalone rustc); (b) no
   `x86_64-linux-musl-gcc` (script hard-requires musl-cross); (c) with offline cargo,
   `proto 0.11.16` was not in the local cache (needed one online fetch). Workaround: the box's
   gentoo musl gcc 16 (`/opt/gentoo/root/usr/...`) wrapped as `x86_64-linux-musl-gcc`
   (the wrapper drops rustc-nightly's lld-only `--threads=1` link arg and points `-B` and
   `LD_LIBRARY_PATH` at the gentoo binutils/libbfd), `x86_64-linux-musl-ar` likewise; then the
   script's `cargo build --release --locked --target x86_64-unknown-linux-musl` runs verbatim
   and ring builds fine. Output byte-count identical path as the script (`bootstrap/bin/tokioprobe.x86_64`).
8. **herd vm-run pid capture**: `setsid /bin/herd vm-run vm.spec ... & echo $!` records the
   setsid wrapper's pid, not the vm-run process (setsid forks); `$!` was already dead while the
   guest kept running, and the first "stop" missed it. What works: find the single
   `/bin/herd vm-run <spec>` process via ps, record that pid in `vm.pid`, stop by that pid.
9. **`taoxichuan_goose_image.sh` PATH assumption**: it calls `debugfs`/`mke2fs`/`e2fsck`
   unqualified and exits 127 here (see #2). With PATH fixed, its next line fails:
10. **`taoxichuan_goose_image.sh` reads the raw device**: `debugfs -c -R "dump ..." /dev/nvme0n1p3`
    cannot run on Akuma bare metal — `/dev` has no block device nodes at all and `/sys/block` is
    empty (Akuma exposes none). The same files (goose, busybox, sshd, `ld-musl-x86_64.so.1`,
    CA bundle, sshd keys) were taken from the live root with `cp` instead; the rest of the script
    (tree layout, applet symlinks via `busybox --list`, hosts/passwd/group, the GLM config.yaml,
    secrets from stdin, mke2fs -d 512M, e2fsck) is its procedure unchanged. Two more forced
    deltas: `/etc/resolv.conf` written as `nameserver 10.0.2.3` (the vm relay — the script's
    host dump would be wrong inside a guest), and `/etc/sshd/authorized_keys` = the host's lines
    plus this session's login key. The `rs.goose.orig` fallback was not needed.
11. **`mke2fs -d` itself fails on Akuma**: `set_inode_xattr: Function not implemented while
    listing attributes` — the kernel serves no xattr syscalls. Workaround: an `LD_PRELOAD` stub
    (musl-shared `.so` built with the same gentoo gcc) whose `listxattr/llistxattr/flistxattr`
    return 0 (no attributes) and whose setters are silent no-ops; `mke2fs -d` then populates
    normally and `e2fsck -fn` is clean.
12. **The goose image's sshd from the host does not run in the guest here**: the script dumps
    `/bin/sshd` from the machine's root, but on Akuma that binary is the Akuma-ABI build, not a
    Linux-ABI one — in the guest every spawn dies with `[nosys] nr=4397` and sshd falls back to
    its built-in emergency shell (`[SSH] '/bin/sh' failed to spawn ??? retrying with the
    built-in emergency shell /bin/paws`), where commands answer from builtins (`uname -a`
    prints `Akuma 0.1.0 Akuma-OS x86_64`, not the guest's). The script was written on the Pop
    box, where the host was Linux and `/bin/sshd` was the linux-abi build. Fix: `debugfs -w`
    the tree's linux-abi sshd (the gate-2 build) over the image's `/bin/sshd` and the static
    busybox over its dynamic one — after that the guest gives a real `Linux shanghai` shell.
13. **debugfs `-w` `write` cannot overwrite an existing file** ("Ext2 file already exists");
    `rm` the target first. Also `debugfs -w` on an image a guest has open is what corrupted-ish
    state to avoid — the README does warn about this ("Write to an image with debugfs -w only
    while no guest has it open"); it matters because the guest writes its own host key into the
    image at boot, and rebuilding the image (mke2fs + debugfs in one pass) is cleaner than
    patching a booted one.
14. **Spec detail, not README's fault but undocumented**: for a one-core boot, `smp` lives
    inside the `cmdline=` value; changing only `vcpus=` gives `smp: 1 of 4 requested` — both
    must change.

## Notes

- `/root/shanghai/` holds: `w/shanghai.img` (gate 2-5 image), `w/goose.img` (gate 6 image),
  `w/key`/`w/key.pub`, `w/busybox`, `w/bin/` (musl-gcc wrappers), `w/noxattr.{c,so}`,
  `vm.pid`, and the gate logs (`gate1-build.log`, `gate2-sshd.log`, `gate5-build.log`,
  `gate6-image.log`, `w/vm.log` of the last boot).
- The goose image at `/root/tx-side/goose.img` (the adapted script's own OUT path) is a copy of
  the built image; `/root/tx/rs.goose.orig` was never written.
- After each gate: killed only the guest recorded in `w/vm.pid`; `herd status` confirmed
  `vm-dev: exited` before each boot; the supervisor services were never touched.

## Middleware sample (B22, 2026-11-05)

`--features middleware` adds `src/middleware.rs` (one table, every tier once). `sh shanghai/mw_gate.sh` boots the default
kernel (control) and the middleware kernel under QEMU TCG with `mwprobe/mwprobe.rs` as init: control `CONTROL PASS`,
middleware `PASS` (26/26 with the no-dynamic-loading demo, `src/nodl.rs`: forked, exec'd and real dynamic children included, and a 4-thread autolock contention case; control 7/7), on amd64 and aarch64, at 4 cores (amd64 also at 1). An earlier revision of this line said 14/14 for a 13-case probe. See `docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md` § What was verified, and the guide `crates/akuma-tokio-kernel-core/docs/middleware.md`.
