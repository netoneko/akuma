# Run the gates

A change to this kernel, or to a crate it composes, is gated on the Mac's QEMU TCG rig first and on the ryzen
second. Every gate boots a fresh guest; none scores a silent probe as a pass.

## 1. The TCG rig (Mac, no KVM)

`scripts/kvm/taoxichuan/tcg_rig.py` (its docstring has every option). It boots `TX_DISK` through a qcow2
overlay, so the image is never written; `boot fresh` discards the guest's writes.

```bash
R=scripts/kvm/taoxichuan/tcg_rig.py
export TX_DISK=<the battery image> TX_KEY=<its ssh key>
CON=${TMPDIR:-/tmp}/tx-tcg/console.log
python3 $R build                                  # no-tests,trace-err: the leak gate reads trace-err's reap lines
python3 $R boot fresh && python3 $R battery goose  # regression battery + the GLM prompt
python3 -I scripts/kvm/goose/cow_leak_gate.py --console $CON --key $TX_KEY --port 2244 --cycles 20
python3 $R stop
TX_SMP=4 python3 $R boot fresh && python3 $R run /usr/local/bin/tokioprobe 300; python3 $R stop
TX_DISK=<the toolchain image> python3 $R boot fresh && python3 $R battery toolchain; python3 $R stop
```

| gate | passes when |
|---|---|
| `battery` | every line matches `BATTERY` in the script (quick, F1, PK1, fs, append, dup2-sock, pipeline, apk, git) and `console faults=0`; `goose` adds a GLM round trip over TLS |
| `cow_leak_gate.py` | the idle machine's free-page count after the rounds equals the baseline exactly (`delta=0`) and `cow_ref_count() == 0` |
| `tokioprobe` at `smp=1` and `smp=4` | exits 0 with `RESULT PASS` (twice at `smp=4`) |
| `battery toolchain` | `rustc` and a `cargo build` with a build script succeed with no `LD_LIBRARY_PATH` |
| signal and process probes (on the image, `/usr/local/bin`) | `pidfdprobe`, `sigsusprobe`, `madvshared`, `decommitprobe`, `killtree 8` and `sh -c "sleep 1 & sleep 2 & wait"` pass at `smp=1` and `smp=4`; `sigprobe` provokes its two pinned faults and nothing else |

Stage files onto an image with `debugfs -w` only while the rig is stopped.

## 2. The ryzen

- **Fixed point**: `scripts/kvm/taoxichuan/fp_metal.py K0 SHA` builds the kernel at `SHA` inside a guest under
  Akuma's own hypervisor, boots the result and builds again; it passes when K1 == K2 byte for byte
  (`docs/archive/TAOXICHUAN_FIXED_POINT.md`). Stop herd's `vm-dev` first.
  When the ryzen runs Pop instead, `scripts/kvm/taoxichuan/fp_pop.py K0 SHA` runs the same two generations under
  Linux KVM.
- **Census**: `scripts/kvm/taoxichuan/census_pop.sh` on Pop, then `census_classify.py OUT UNISTD_64_H`, exits 1 on
  a call this kernel would not serve ([`../reference/syscalls.md`](../reference/syscalls.md)).

The other ryzen drivers are listed in `scripts/kvm/taoxichuan/README.md`.

## Reading the console

- `grep -a` is mandatory (the console log contains control bytes; plain grep reads it as binary and prints
  nothing).
- `[nosys] nr=N` is printed once per unserved number per boot: a checklist line, not a failure. `nr=40`
  (`sendfile`) is expected ([`../gaps.md`](../gaps.md) G12).
- `[Fault] ring3 …` ended a task (it gets SIGSEGV, `rc=139`); anything without `ring3` stopped the kernel.

## Verify

- The battery prints `RESULT PASS` and exit 0; `cow_leak_gate.py` prints `delta=0 PASS`.
- `grep -a '\[Fault\]' $CON` shows only the faults a probe provokes on purpose (`sigprobe`'s pinned pair).
- For a FAIL, the first `[nosys]` or `[Fault]` line after the boot's banner is where to start; cross-reference
  [`../reference/syscalls.md`](../reference/syscalls.md).

## Background

`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md` (the gates each step ran),
`docs/archive/TAOXICHUAN_FIXED_POINT.md`, `docs/archive/TAOXICHUAN_COW_FORK.md` (the leak gate),
`docs/archive/TAOXICHUAN_M4_SLICE3_PH_RESULT.md` (the stage gates before the rigs).
