#!/usr/bin/env python3
"""Boot Akuma with the AWS overlay and verify metadata-backed SSH login."""
import argparse
import http.server
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[3]
OVERLAY = ROOT / 'overlays/aws/rootfs'


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--disk', required=True, help='existing amd64 Akuma ext2 disk with herd, sshd and busybox')
    p.add_argument('--kernel', default=str(ROOT / 'target/x86_64-unknown-none/release/akuma'))
    p.add_argument('--timeout', type=int, default=180)
    p.add_argument('--imds-v2', action='store_true', help='require a metadata token and verify apk and curl')
    a = p.parse_args()
    with tempfile.TemporaryDirectory(prefix='akuma-aws-e2e-') as tmp:
        tmp = Path(tmp)
        disk = tmp / 'disk.img'
        shutil.copyfile(a.disk, disk)
        key = tmp / 'key'
        run('ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(key))
        wrong_key = tmp / 'wrong-key'
        run('ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(wrong_key))
        public = key.with_suffix('.pub').read_text().strip()
        class Metadata(http.server.BaseHTTPRequestHandler):
            requests = 0
            def log_message(self, *args): pass
            def do_PUT(self):
                if not a.imds_v2 or self.path != '/latest/api/token':
                    self.send_error(404); return
                self.send_response(200); self.end_headers(); self.wfile.write(b'test-token')
            def do_GET(self):
                if self.path != '/latest/meta-data/public-keys/0/openssh-key':
                    self.send_error(404); return
                if a.imds_v2 and self.headers.get('X-aws-ec2-metadata-token') != 'test-token':
                    self.send_error(401); return
                type(self).requests += 1
                self.send_response(200); self.end_headers(); self.wfile.write(public.encode())
        server = http.server.ThreadingHTTPServer(('0.0.0.0', 0), Metadata)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        def debug(command):
            result = run('debugfs', '-w', '-R', command, str(disk), text=True, capture_output=True)
            return result.stderr + result.stdout
        try:
            with socket.socket() as s:
                s.bind(('127.0.0.1', 0)); ssh_port = s.getsockname()[1]
            debug('mkdir /sbin')
            for path in ['/sbin/instance-key', '/etc/herd/enabled/instance-key.conf',
                         '/etc/herd/enabled/sshd.conf', '/etc/herd/enabled/console.conf',
                         '/etc/sshd/authorized_keys', '/etc/sshd/id_ed25519',
                         '/etc/sshd/id_ed25519.pub', '/etc/sshd/host_key']:
                debug('rm ' + path)
            for src, dest in [(OVERLAY / 'sbin/instance-key', '/sbin/instance-key'),
                              (OVERLAY / 'etc/herd/enabled/sshd.conf', '/etc/herd/enabled/sshd.conf')]:
                out = debug(f'write {src} {dest}')
                if 'Allocated inode' not in out: raise RuntimeError(out)
            config = tmp / 'instance-key.conf'
            config.write_text((OVERLAY / 'etc/herd/enabled/instance-key.conf').read_text() +
                              f'\nenv = METADATA_URL=http://10.0.2.2:{server.server_port}\n')
            out = debug(f'write {config} /etc/herd/enabled/instance-key.conf')
            if 'Allocated inode' not in out: raise RuntimeError(out)
            log = tmp / 'qemu.log'
            with log.open('wb') as output:
                vm = subprocess.Popen(['qemu-system-x86_64', '-M', 'microvm', '-cpu', 'max',
                    '-global', 'virtio-mmio.force-legacy=false', '-kernel', a.kernel,
                    '-m', '2048', '-smp', '1', '-drive', f'id=d0,file={disk},format=raw,if=none',
                    '-device', 'virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0',
                    '-netdev', f'user,id=n0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22',
                    '-device', 'virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1',
                    '-append', 'virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=/bin/herd',
                    '-serial', 'stdio', '-display', 'none', '-no-reboot'], stdout=output, stderr=subprocess.STDOUT)
                try:
                    deadline = time.monotonic() + a.timeout
                    while time.monotonic() < deadline and vm.poll() is None:
                        login = subprocess.run(['ssh', '-i', str(key), '-p', str(ssh_port), '-o', 'BatchMode=yes',
                            '-o', 'ConnectTimeout=2', '-o', 'StrictHostKeyChecking=no',
                            '-o', 'UserKnownHostsFile=/dev/null', 'root@127.0.0.1',
                            'echo AWS_E2E_OK'], capture_output=True, text=True)
                        if login.returncode == 0 and 'AWS_E2E_OK' in login.stdout and Metadata.requests:
                            rejected = subprocess.run(['ssh', '-i', str(wrong_key), '-p', str(ssh_port),
                                '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=2',
                                '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=/dev/null',
                                'root@127.0.0.1', 'echo UNEXPECTED'], capture_output=True, text=True)
                            if rejected.returncode == 0 or 'UNEXPECTED' in rejected.stdout:
                                raise RuntimeError('an unrelated key was accepted')
                            if a.imds_v2:
                                check = subprocess.run(['ssh', '-i', str(key), '-p', str(ssh_port),
                                    '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=2',
                                    '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=/dev/null',
                                    'root@127.0.0.1', '/sbin/apk --version && /usr/bin/curl --version'],
                                    capture_output=True, text=True)
                                if check.returncode or 'apk-tools' not in check.stdout or 'curl ' not in check.stdout:
                                    raise RuntimeError('guest apk/curl check failed: ' + check.stdout + check.stderr)
                            print('PASS: VM booted herd, fetched launch key, authenticated SSH, and rejected another key'); return
                        time.sleep(2)
                    raise RuntimeError('VM did not complete metadata-backed SSH login')
                finally:
                    vm.terminate()
                    try: vm.wait(timeout=5)
                    except subprocess.TimeoutExpired: vm.kill(); vm.wait()
                    if vm.returncode and log.exists(): print(log.read_text(errors='replace')[-12000:])
        finally:
            server.shutdown(); server.server_close()


if __name__ == '__main__': main()
