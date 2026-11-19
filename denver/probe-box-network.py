#!/usr/bin/env python3
"""Exercise an actual boxed TCP request through Denver's virtio NIC and QEMU SLIRP."""
import http.server
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading

root = Path(__file__).resolve().parent.parent
kernel = os.environ.get("DENVER_KERNEL")
if not kernel:
    raise SystemExit("Set DENVER_KERNEL to a Denver amd64 kernel built with containers")
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"denver-box-network\n")
    def log_message(self, *args):
        pass
class Server(socketserver.TCPServer):
    allow_reuse_address = True
with tempfile.TemporaryDirectory(prefix="denver-box-net-") as tmp, Server(("0.0.0.0", 0), Handler) as server:
    d = Path(tmp)
    for source, name in (("parent.c", "parent"), ("network.c", "network")):
        subprocess.run(["cc", "-nostdlib", "-static", "-fno-stack-protector", "-mno-sse", "-O2", f"-DPROBE_PORT={server.server_address[1]}", str(root / "denver/probes/boxes" / source), "-o", str(d / name)], check=True)
    image = d / "root.img"
    with image.open("wb") as f:
        f.truncate(32 * 1024 * 1024)
    subprocess.run(["mke2fs", "-q", "-t", "ext2", "-F", str(image)], check=True)
    for command in ("mkdir /bin", "mkdir /box", "mkdir /box/bin", f"write {d}/parent /bin/parent", f"write {d}/network /box/bin/raw", "set_inode_field /bin/parent mode 0100755", "set_inode_field /box/bin/raw mode 0100755"):
        subprocess.run(["debugfs", "-w", "-R", command, str(image)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    cmd = ["qemu-system-x86_64", "-M", "microvm", "-cpu", "max", "-global", "virtio-mmio.force-legacy=false", "-m", "1024", "-smp", "1", "-kernel", kernel, "-drive", f"id=d0,file={image},format=raw,if=none,snapshot=on", "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0", "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1", "-append", "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 smp=1 init=/bin/parent", "-serial", "mon:stdio", "-display", "none", "-no-reboot"]
    try:
        result = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=40, text=True)
        output = result.stdout
        if result.returncode:
            raise SystemExit(f"QEMU failed ({result.returncode}):\n{output}")
    except subprocess.TimeoutExpired as exc:
        output = exc.stdout.decode(errors="replace") if isinstance(exc.stdout, bytes) else exc.stdout or ""
    finally:
        server.shutdown()
    print(output)
    markers = ("REGISTER PASS", "SPAWN PASS", "BOX_NET_PASS", "KILL PASS", "Denver: exit code 0")
    missing = [m for m in markers if m not in output]
    if missing or "NET_" in output.replace("BOX_NET_PASS", ""):
        raise SystemExit(f"RESULT FAIL: missing {missing} or guest reported network failure")
    print("RESULT PASS: boxed TCP request through Denver and SLIRP")
