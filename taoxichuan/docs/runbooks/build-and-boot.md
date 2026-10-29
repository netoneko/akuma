# Build and boot taoxichuan as the vm-dev guest

The kernel is x86_64-only and is **not** in the workspace `default-members`.
The pinned build command is the same shape every kernel in this experiment
uses (`kbuild -j 4 --features no-tests` on the dev box, where `kbuild` wraps
cargo with the experiment's target dir); from the repo root with a default
target dir:

```bash
cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests
```

Output: `target/x86_64-unknown-none/release/taoxichuan` (the dev box pins
`/root/ktarget/...`). The linker script comes from `taoxichuan/build.rs`
(`rustc-link-arg-bins`, scoped to this package's binary); the x86_64 target
flags (`relocation-model=static`, `code-model=kernel`) live in the repo's
`.cargo/config.toml`.

## Boot (dev box, kvm0)

One idempotent boot of the vm-dev guest, from the dev box:

```bash
scripts/kvm/goose/boot_dev.sh /root/ktarget/x86_64-unknown-none/release/taoxichuan
```

The script stops vm-dev and vm-serve-mc (kvm0 is exclusive), writes both herd
confs from the tracked template (`scripts/kvm/goose/vm-dev.stage7.conf`),
disables vm-dev for the image rewrite (an enabled-but-stopped service restarts
on herd's reload tick mid-copy — the 856 MiB race,
`docs/archive/KVM_GUEST_DISK_856_MIB_RACE.md`), copies a fresh root from the
`.orig`, re-asserts the DNS prerequisites with `devroot-ensure`, installs the
kernel to `/boot/akuma-guest-dev` (**never** `/boot/akuma-amd64*` — that is
the host kernel), re-enables, starts, and waits for a real ssh round trip.

It prints exactly one line:

```
BOOT OK disk=<img> outbound=1 ready_in=<s>
BOOT FAIL <reason>
```

Detail log: `/root/dev-cycle/taoxichuan-s7/boot_dev.log`; guest console:
`/var/log/herd/vm-dev.log`.

## Verify

- `BOOT OK` and an ssh round trip to the guest.
- The console shows the banner, `ext2 root mounted on /dev/vda`, and
  `entering ring 3`; **zero** `[Fault]`/`PANIC` lines (`grep -a` — the log
  contains control bytes).
- `guest disk:` names the 1 GiB image, not a partial copy (the race above).
- A boot that stalls before the banner with no `[Fault]` is the known rare
  stall — record it against `../gaps.md` G2, do not assume your change caused
  it without an A/B.

## Background

`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (RESULT-D ops
findings), `docs/archive/KVM_GUEST_DISK_856_MIB_RACE.md`.
