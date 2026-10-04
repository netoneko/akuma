#!/usr/bin/env python3
"""Boot the amd64 kernel under QEMU, run a TUI program, and render what it drew.

The harness is a terminal: it feeds the guest's serial output through `pyte`
and ANSWERS `ESC[6n` (cursor-position query) with the emulated cursor. Without
that reply busybox ash's line editor blocks for good, which looks exactly like
a hung guest.

    python3 -m venv /tmp/v && /tmp/v/bin/pip install pyte
    /tmp/v/bin/python scripts/amd64_tui_probe.py IMG 'goose session' \
        --wait 200 --key right:20 --key enter:90

Output: `<out>/tui.raw` (bytes) and `<out>/snaps.txt` (screen after the command
and after each --key). Kernel `[PSTATS]`/`[HEAP]` lines share the serial port and
land on the emulated screen; that is the guest console, not a TUI fault.
Runbook: docs/runbooks/goose-tui-amd64-local.md
"""
import argparse, os, select, subprocess, sys, time
import pyte

KEYS = {'right': b'\x1b[C', 'left': b'\x1b[D', 'up': b'\x1b[A', 'down': b'\x1b[B',
        'enter': b'\r', 'esc': b'\x1b', 'ctrl-c': b'\x03', 'ctrl-d': b'\x04'}

ap = argparse.ArgumentParser()
ap.add_argument('img'); ap.add_argument('cmd')
ap.add_argument('--wait', type=int, default=200, help='seconds to let CMD run before the first snapshot')
ap.add_argument('--boot', type=int, default=40)
ap.add_argument('--key', action='append', default=[], help='NAME_OR_TEXT:SECONDS_TO_WAIT_AFTER')
ap.add_argument('--out', default='.')
ap.add_argument('--smp', default='2')
ap.add_argument('--kernel', default='target/x86_64-unknown-none/release/akuma-amd64')
a = ap.parse_args()

q = subprocess.Popen(
    ['qemu-system-x86_64', '-M', 'microvm', '-cpu', 'max', '-global', 'virtio-mmio.force-legacy=false',
     '-kernel', a.kernel, '-m', '3072', '-smp', a.smp,
     '-drive', f'id=d0,file={a.img},format=raw,if=none',
     '-device', 'virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0',
     '-netdev', 'user,id=n0', '-device', 'virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1',
     '-append', 'virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=/bin/busybox initargs=sh',
     '-serial', 'stdio', '-display', 'none', '-no-reboot', '-monitor', 'none'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE)
os.set_blocking(q.stdout.fileno(), False)
screen = pyte.Screen(80, 24); stream = pyte.ByteStream(screen)
raw = open(os.path.join(a.out, 'tui.raw'), 'wb')
snap = open(os.path.join(a.out, 'snaps.txt'), 'w')

def pump(t):
    end = time.time() + t
    while time.time() < end:
        if not select.select([q.stdout], [], [], 0.2)[0]:
            continue
        try: d = os.read(q.stdout.fileno(), 65536)
        except BlockingIOError: continue
        if not d: return
        raw.write(d); raw.flush(); stream.feed(d)
        for _ in range(d.count(b'\x1b[6n')):
            q.stdin.write(b'\x1b[%d;%dR' % (screen.cursor.y + 1, screen.cursor.x + 1)); q.stdin.flush()

def shot(label):
    snap.write('=== %s\n%s\n' % (label, '\n'.join(l.rstrip() for l in screen.display))); snap.flush()

def send(b):
    q.stdin.write(b); q.stdin.flush()

pump(a.boot)
send((a.cmd + '\n').encode()); pump(a.wait); shot('after command')
for k in a.key:
    name, _, w = k.rpartition(':')
    send(KEYS.get(name, name.encode())); pump(int(w)); shot(k)
send(b'\x04'); pump(3); q.kill()
