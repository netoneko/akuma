# Taoxichuan on ryzen Linux KVM - the harness (2026-10-31)

Drivers for booting the Taoxichuan kernel under QEMU microvm + KVM on the ryzen while it runs Pop!_OS, and gating it. They reach the ryzen as
root through `scripts/utils/hpbox.py`; the guest is `scripts/kvm/taoxichuan_pop.sh` (copied to the ryzen as `/root/tx-pop.sh`, patched there for
`TX_INIT`, `TX_QEMU_EXTRA`). The kernel is built on the Mac: `cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests,trace-err`.

| script | what |
|---|---|
| `rzput.py LOCAL REMOTE` / `-c CMD` | put a file on the ryzen (atomic, `+x`) / run a command there |
| `gate2.py KERNEL SMP BOOTS [TRIALS]` | boot BOOTS times, run a fork/pipe probe TRIALS times each; prints pass counts and fault-line counts |
| `exp_cargo.py KERNEL SMP POLLS [INIT]` | boot on the p3 toolchain root, check out the tree in the guest, start `cargo build`, poll for `/tmp/tgt` |
| `bis_run.sh SHA INIT` | `exp_cargo` on `kb-SHA` (a kernel you built from SHA) and count `rustc` execs in the console |
| `fp_run.py KERNEL SMP` | the fixed-point build (`fp.sh`) - see `docs/archive/TAOXICHUAN_FIXED_POINT.md` |
| `probe_run.py` / `probes_on_guest.py` | boot, fetch `userspace/forktest/c_stress` probes over the user-net (`10.0.2.2:8099`), run them |
| `pollhup.c`, `outputprobe.rs` | probes written for the cargo stall (`docs/archive/TAOXICHUAN_CARGO_STALL_REGRESSION.md`) |

Traps (each cost time): the guest's `sh -c "...$?..."` is expanded by the OUTER shell - use plain commands; ssh stdin into the guest is lossy - send
scripts on the command line; musl probes must be built in an Alpine container (`docker run alpine:3.20`), a glibc-static binary segfaults for no kernel reason;
use `wget`, not `curl`, in the guest; never `git stash` with pathspecs here (it can silently stash nothing and `pop` an older entry).
