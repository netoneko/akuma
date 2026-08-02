#!/usr/bin/env python3
"""Self-host: shallow-clone akuma inside the VM and build the smoltcp-devbox SMP kernel.

Target is `--profile release-smp-shared --features devbox-smoltcp,no-tests`, which
inherits `profile.release` (`panic = "abort"`) and therefore links against the
precompiled `aarch64-unknown-none` std — no `-Z build-std`, no `rust-src` needed.

Toolchain split, per AKUMA_SELF_HOSTING.md §7j: apk **cargo** orchestrates (the nightly
cargo still crashes at startup), nightly **rustc** under /usr/local compiles (only it
ships the `aarch64-unknown-none` std). Stable cargo cannot parse the workspace manifest
while line 1 is `cargo-features = [...]`, so that line is stripped in the clone.

Everything long-running is launched detached and polled; ssh keepalives kill a channel
at ~240s under load and a slow-but-alive build would otherwise read as a failure.
"""
import subprocess, sys, time, shlex, re

PORT = sys.argv[1] if len(sys.argv) > 1 else '2322'
SSH = ['ssh', '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=/dev/null',
       '-o', 'ConnectTimeout=25', '-o', 'ServerAliveInterval=0', '-p', PORT, 'root@localhost']

ENVP = ('/bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root '
        'CARGO_HOME=/root/.cargo RUSTC=/usr/local/bin/rustc '
        'CARGO_BUILD_TARGET=aarch64-unknown-none '
        'CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/root/akuma/linker.ld ')


def sh(cmd, timeout=180):
    try:
        r = subprocess.run(SSH + [cmd], capture_output=True, text=True, timeout=timeout)
        return re.sub(r'\x1b\[[0-9;]*[A-Za-z]', '', r.stdout)
    except subprocess.TimeoutExpired:
        return '<ssh-timeout>'


def detached(tag, cmd):
    sh(f'/bin/busybox rm -f /tmp/{tag}.out /tmp/{tag}.rc')
    wrapper = f'{cmd} > /tmp/{tag}.out 2>&1; echo $? > /tmp/{tag}.rc'
    sh(f'/bin/busybox nohup /bin/busybox sh -c {shlex.quote(wrapper)} >/dev/null 2>&1 &')


def wait(tag, budget, interval=20, tail_every=6):
    t0, n = time.time(), 0
    while time.time() - t0 < budget:
        time.sleep(interval)
        n += 1
        rc = sh(f'/bin/busybox cat /tmp/{tag}.rc 2>/dev/null').strip()
        if rc:
            try:
                return int(rc), time.time() - t0
            except ValueError:
                return -999, time.time() - t0
        if n % tail_every == 0:
            tail = sh(f'/bin/busybox tail -3 /tmp/{tag}.out 2>/dev/null').strip()
            print(f'    [{tag}] +{time.time()-t0:.0f}s … {tail[-300:]!r}', flush=True)
    return None, time.time() - t0


def step(name, cmd, budget=3600):
    print(f'>>> {name}', flush=True)
    detached(name, cmd)
    rc, el = wait(name, budget)
    out = sh(f'/bin/busybox tail -25 /tmp/{name}.out 2>/dev/null').strip()
    print(f'<<< {name} rc={rc} {el:.0f}s\n{out[-2000:]}\n', flush=True)
    return rc


if __name__ == '__main__':
    print(sh('/bin/busybox uname -a').strip(), flush=True)
    print('nightly rustc:', sh('/usr/local/bin/rustc --version').strip(), flush=True)
    print('apk cargo    :', sh('/usr/bin/cargo --version').strip(), flush=True)
    print('none-std     :', sh('/bin/busybox ls /usr/local/lib/rustlib/ 2>&1').strip(), flush=True)

    # 1. shallow clone, in-VM, over the guest's own network + TLS
    step('clone',
         '/usr/bin/git clone --depth 1 https://github.com/netoneko/akuma.git /root/akuma',
         budget=2400)
    print('HEAD:', sh('/bin/busybox sh -c "cd /root/akuma && /usr/bin/git log --oneline -1"').strip(), flush=True)

    # 2. make the workspace parseable by *stable* cargo
    step('manifest',
         '/bin/busybox sh -c "cd /root/akuma && '
         '/bin/busybox cp Cargo.toml Cargo.toml.selfhost-bak && '
         '/bin/busybox sed -i \\"/^cargo-features/d\\" Cargo.toml && '
         '/bin/busybox sed -i \\"s/panic = .immediate-abort./panic = \\\\\\"abort\\\\\\"/\\" Cargo.toml && '
         '/bin/busybox head -3 Cargo.toml"',
         budget=300)

    # 3. the build under test
    step('build',
         ENVP + '/usr/bin/cargo build -p akuma --profile release-smp-shared '
                '--features devbox-smoltcp,no-tests '
                '--manifest-path /root/akuma/Cargo.toml -j1',
         budget=14400)

    print('artifact:', sh('/bin/busybox ls -la '
                         '/root/akuma/target/aarch64-unknown-none/release-smp-shared/akuma 2>&1').strip(),
          flush=True)
    print('elf magic:', sh('/bin/busybox od -A x -t x1z -N 20 '
                           '/root/akuma/target/aarch64-unknown-none/release-smp-shared/akuma 2>&1').strip(),
          flush=True)
    print('md5:', sh('/bin/busybox md5sum '
                     '/root/akuma/target/aarch64-unknown-none/release-smp-shared/akuma 2>&1').strip(),
          flush=True)
