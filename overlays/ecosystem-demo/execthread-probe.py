#!/usr/bin/env python3
"""Probe threaded exec on the guest root and in a box; each case gets a fresh VM.

Uses the existing static musl execthread program. A marker alone is not enough:
we also require the guest to survive and inspect its serial output for faults.
Set ECO_KERNEL to the kernel under test and optionally pass --out for its disks.
"""
import argparse
import subprocess
import tempfile
import time
from pathlib import Path

import ecolib
import launch
import verify


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--out', type=Path, default=launch.WORK / 'out')
    ap.add_argument('--iterations', type=int, default=3)
    args = ap.parse_args()
    if args.iterations < 1:
        ap.error('--iterations must be positive')
    work = Path(tempfile.mkdtemp(prefix='eco-execthread-', dir=launch.WORK))
    binary = work / 'execthread'
    source = ecolib.ROOT / 'userspace/forktest/execthread/execthread.rs'
    subprocess.run(['rustc', '--edition', '2021', '-O', '--target', 'x86_64-unknown-linux-musl',
                    '-C', 'target-feature=+crt-static', str(source), '-o', str(binary)], check=True)
    failures = 0
    for boxed in (False, True):
        for mode in ('fail', 'ok'):
            for iteration in range(args.iterations):
                run_dir = work / f"{'box' if boxed else 'host'}-{mode}-{iteration}"
                run_dir.mkdir()
                d = launch.Demo(args.out, run_dir, 3072, 2)
                try:
                    d.up(which=(1,))
                    g = d.guests[1]
                    if boxed:
                        g.run('/sbin/eco-box exec thread-probe rust true', check=True)
                        target = '/var/lib/box/containers/thread-probe/upper/tmp/execthread'
                    else:
                        target = '/tmp/execthread'
                    with binary.open('rb') as src:
                        subprocess.run(g.ssh_argv(f'cat > {target} && chmod 755 {target}'), stdin=src, check=True, timeout=60)
                    start = Path(g.serial).stat().st_size
                    command = '/tmp/execthread fail' if mode == 'fail' else "/tmp/execthread ok /bin/sh -c 'echo EXECTHREAD-NEWIMAGE'"
                    if boxed:
                        rc, output = verify.bx(g, 'thread-probe', 'rust', command, timeout=60)
                    else:
                        rc, output = g.run(command, timeout=60)
                    time.sleep(1)
                    alive, _ = g.run('echo alive', timeout=15)
                    serial = Path(g.serial).read_bytes()[start:].decode(errors='replace')
                    marker = 'EXECTHREAD-MAIN-ALIVE' if mode == 'fail' else 'EXECTHREAD-NEWIMAGE'
                    missing_ok = mode != 'fail' or 'No such file or directory' in output
                    ok = rc == 0 and alive == 0 and marker in output and missing_ok and '[Fault]' not in serial and 'panic' not in serial.lower()
                    print(f"{'PASS' if ok else 'FAIL'} {run_dir.name}: rc={rc}, ssh={alive}, marker={marker in output}", flush=True)
                    if not ok:
                        print(output[-1000:], flush=True)
                        failures += 1
                except (subprocess.SubprocessError, RuntimeError, OSError) as exc:
                    print(f'FAIL {run_dir.name}: {exc}', flush=True)
                    failures += 1
                finally:
                    d.down()
    print(f'{failures} failures; artifacts: {work}', flush=True)
    return bool(failures)


if __name__ == '__main__':
    raise SystemExit(main())
