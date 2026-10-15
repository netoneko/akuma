"""Install the freshly built amd64 kernel on ryzen and boot a Gentoo menu entry.

    python3 overlays/ryzen/gentoo/deploy.py [entry]     # default 17 (18 = + strace_err)

If the Gentoo box is up it is asked to reboot to Pop first (over its own sshd on
2222). Then: kernel from target/x86_64-unknown-none/release/akuma-amd64 ->
the ESP, optionally p7's /akuma-init.sh from ./akuma-init.sh (--init), arm the
entry, reboot, and wait for 2222 + 4444. Needs scripts/utils/hpbox.py.
"""
import base64, os, socket, subprocess, sys, time
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, '..', '..', '..'))
sys.path.insert(0, os.path.join(ROOT, 'scripts', 'utils'))
import hpbox

GENTOO = "192.168.1.159"
args = [a for a in sys.argv[1:] if not a.startswith('--')]
entry = args[0] if args else '17'
push_init = '--init' in sys.argv


def up(port):
    s = socket.socket(); s.settimeout(1)
    try:
        s.connect((GENTOO, port)); return True
    except Exception:
        return False
    finally:
        s.close()


def put(path, text):
    return f"echo {base64.b64encode(text.encode()).decode()} | base64 -d > {path}"


if up(2222):
    subprocess.run(["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                    "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-p", "2222", f"root@{GENTOO}",
                    "sync; (sleep 1; /sbin/reboot -f) >/dev/null 2>&1 &"], capture_output=True, timeout=30)
    time.sleep(25)
t0 = time.time()
while time.time() - t0 < 300 and hpbox.ryzen_root('uptime')[0] != 0:
    time.sleep(8)
kernel = open(os.path.join(ROOT, 'target/x86_64-unknown-none/release/akuma-amd64'), 'rb').read()
r = subprocess.run(hpbox.RZ_ROOT + ['cat > /root/akuma-amd64.new && cp /root/akuma-amd64.new '
                                    '/boot/efi/EFI/akuma/akuma-amd64 && sync && md5sum /boot/efi/EFI/akuma/akuma-amd64'],
                   input=kernel, capture_output=True, timeout=600)
print(r.stdout.decode().strip())
cmd = ''
if push_init:
    init = open(os.path.join(HERE, 'akuma-init.sh')).read()
    stage = ("set -e\nmount /dev/nvme0n1p7 /mnt/p7\ncp /root/akuma-init.sh /mnt/p7/akuma-init.sh\n"
             "chmod 755 /mnt/p7/akuma-init.sh\nsync; umount /mnt/p7\n")
    cmd += put('/root/akuma-init.sh', init) + '\n' + put('/root/stage-init.sh', stage) + \
        '\nunshare -m --propagation private sh /root/stage-init.sh\n'
cmd += (f"D=/boot/efi/EFI/akuma\ngrub-editenv $D/grubenv set next_entry={entry}\n"
        "bootctl set-oneshot akuma.conf\nsync\n(sleep 2; systemctl reboot) >/dev/null 2>&1 &\n")
hpbox.ryzen_root(cmd, timeout=120)
time.sleep(20)
t0 = time.time()
while time.time() - t0 < 400 and not (up(2222) and up(4444)):
    time.sleep(5)
ok = up(2222) and up(4444)
print("entry", entry, "up" if ok else "NOT up", int(time.time() - t0), "s")
sys.exit(0 if ok else 1)
