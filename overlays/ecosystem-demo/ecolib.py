#!/usr/bin/env python3
"""Shared host-side pieces of the ecosystem demo launcher (see README.md).

Everything here runs inside ONE unprivileged user+network namespace
(`unshare --map-root-user --net`): no sudo, nothing on the host's own network is
touched, and tearing the namespace down removes the bridge, the taps
and every server below with it.

    br0   10.77.0.1/24 and 169.254.169.254/32   the host's side of the segment
    tapN  one per guest, enslaved to br0        a single L2 segment, so guest 2
                                                reaches guest 1 directly
    DhcpServer  a fixed lease per MAC           (the kernel speaks DHCP; dnsmasq cannot
                                                 drop groups inside a user namespace)
    MetadataServer  :80 on 169.254.169.254      EC2-IMDS-shaped, per-guest data
    MirrorServer    :8000 on 10.77.0.1          the pinned Alpine apk cache

The namespace has no route to the internet. That is deliberate for stage 1 (the
images must not need it), and it is the gap a later stage has to close (model
API access) with a NAT the launcher cannot create without privilege.
"""
import dataclasses
import hashlib
import http.server
import json
import os
import secrets
import shlex
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
SUBNET = "10.77.0.0/24"
HOST_IP = "10.77.0.1"
META_IP = "169.254.169.254"
MIRROR_PORT = 8000

# --------------------------------------------------------------------- namespace


def in_namespace() -> bool:
    return os.environ.get("ECO_IN_NS") == "1"


def enter_namespace():
    """Re-exec this script inside a fresh user+net namespace (once)."""
    if in_namespace():
        return
    env = dict(os.environ, ECO_IN_NS="1")
    os.execvpe("unshare", ["unshare", "--map-root-user", "--net", "--kill-child",
                           sys.executable, *sys.argv], env)


def sh(*args, check=True, **kw):
    return subprocess.run(args, check=check, text=True, **kw)


def mac_for(n: int) -> str:
    return f"52:54:00:77:00:{n:02x}"


def ip_for(n: int) -> str:
    return f"10.77.0.{10 + n}"


# ------------------------------------------------------------------- metadata


@dataclasses.dataclass
class AgentData:
    """What the metadata service hands one agent: prompt files and secrets."""
    prompts: dict = dataclasses.field(default_factory=dict)   # file name -> text
    secrets: dict = dataclasses.field(default_factory=dict)   # NAME -> value


@dataclasses.dataclass
class GuestMeta:
    """The per-guest side of the metadata service, looked up by the guest's IP."""
    name: str
    ip: str
    launch_key: str                  # ssh-ed25519 line: the launcher's key
    extra_keys: list = dataclasses.field(default_factory=list)
    bootstrap: bytes = b""           # the script served at /latest/user-data
    bootstrap_sha256: str = ""       # served at a DIFFERENT path (see README)
    agents: dict = dataclasses.field(default_factory=dict)   # name -> AgentData
    log: list = dataclasses.field(default_factory=list)      # (method, path, status)


class MetadataServer:
    """An EC2-IMDSv2-shaped endpoint with one data set per guest.

    One listener serves every guest and picks the data set by the *peer address*,
    which is how the real service tells instances apart. Starting it "per guest"
    means `add_guest`, which is what the launcher does for each guest it boots.
    Requests are logged by (method, path, status) and never by body or token, so
    the log can be kept and read without leaking a secret.
    """

    def __init__(self, bind=(META_IP, 80), require_token=True):
        self.guests: dict[str, GuestMeta] = {}
        self.tokens: dict[str, float] = {}
        self.require_token = require_token
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def log_message(self, *a):
                pass

            def _send(self, code, body=b"", ctype="text/plain"):
                self.send_response(code)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_PUT(self):
                g = outer.guests.get(self.client_address[0])
                if g is None or self.path != "/latest/api/token":
                    return self._send(404)
                tok = secrets.token_hex(16)
                outer.tokens[tok] = time.monotonic() + 60
                g.log.append(("PUT", self.path, 200))
                self._send(200, tok.encode())

            def do_GET(self):
                g = outer.guests.get(self.client_address[0])
                if g is None:
                    return self._send(404)
                if outer.require_token:
                    t = self.headers.get("X-aws-ec2-metadata-token", "")
                    if outer.tokens.get(t, 0) < time.monotonic():
                        g.log.append(("GET", self.path, 401))
                        return self._send(401)
                code, body = outer.route(g, self.path)
                g.log.append(("GET", self.path, code))
                self._send(code, body)

        class S(http.server.ThreadingHTTPServer):
            daemon_threads = True
            request_queue_size = 32

        self.httpd = S(bind, H)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def add_guest(self, g: GuestMeta):
        self.guests[g.ip] = g

    def start(self):
        self.thread.start()

    def stop(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    @staticmethod
    def route(g: GuestMeta, path: str):
        p = path.split("?")[0].rstrip("/")
        pre = "/latest/meta-data"
        keys = [g.launch_key, *g.extra_keys]
        if p == "/latest/user-data":
            return 200, g.bootstrap
        if p == pre + "/bootstrap-sha256":
            return 200, g.bootstrap_sha256.encode()
        if p == pre + "/public-keys/0/openssh-key":
            return 200, keys[0].encode()
        if p == pre + "/public-keys":
            return 200, "".join(f"{i}={'launch' if i == 0 else 'extra'}\n" for i in range(len(keys))).encode()
        if p.startswith(pre + "/public-keys/") and p.endswith("/openssh-key"):
            try:
                return 200, keys[int(p.split("/")[-2])].encode()
            except (ValueError, IndexError):
                return 404, b""
        if p == pre + "/instance-id":
            return 200, f"i-{g.name}".encode()
        if p == pre + "/local-ipv4":
            return 200, g.ip.encode()
        if p == pre + "/agents":
            return 200, "".join(n + "\n" for n in sorted(g.agents)).encode()
        if p.startswith(pre + "/agents/"):
            rest = p[len(pre + "/agents/"):].split("/")
            a = g.agents.get(rest[0])
            if a is None:
                return 404, b""
            if len(rest) == 2 and rest[1] in ("prompts", "secrets"):
                d = a.prompts if rest[1] == "prompts" else a.secrets
                return 200, "".join(k + "\n" for k in sorted(d)).encode()
            if len(rest) == 3 and rest[1] == "prompts" and rest[2] in a.prompts:
                return 200, a.prompts[rest[2]].encode()
            if len(rest) == 3 and rest[1] == "secrets" and rest[2] in a.secrets:
                return 200, a.secrets[rest[2]].encode()
        return 404, b""


# --------------------------------------------------------------------- network


class DhcpServer:
    """Fixed-lease DHCP on br0: DISCOVER -> OFFER, REQUEST -> ACK, nothing else."""

    def __init__(self, leases: dict):
        self.leases = {m.lower(): ip for m, ip in leases.items()}   # MAC -> IP
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, b"br0")
        self.sock.bind(("0.0.0.0", 67))
        self.log = []
        self.thread = threading.Thread(target=self.loop, daemon=True)

    def start(self):
        self.thread.start()

    def stop(self):
        self.sock.close()

    def loop(self):
        while True:
            try:
                pkt, _ = self.sock.recvfrom(1500)
            except OSError:
                return
            if len(pkt) < 241 or pkt[0] != 1 or pkt[236:240] != b"\x63\x82\x53\x63":
                continue
            mac = ":".join(f"{b:02x}" for b in pkt[28:34])
            ip = self.leases.get(mac)
            mtype, i = None, 240
            while i < len(pkt) and pkt[i] != 255:
                if pkt[i] == 0:
                    i += 1
                    continue
                if pkt[i] == 53:
                    mtype = pkt[i + 2]
                i += 2 + pkt[i + 1]
            if ip is None or mtype not in (1, 3):
                continue
            reply_type = 2 if mtype == 1 else 5
            self.log.append((mac, "OFFER" if mtype == 1 else "ACK", ip))
            a = socket.inet_aton
            opts = (bytes([53, 1, reply_type]) + bytes([54, 4]) + a(HOST_IP) +
                    bytes([51, 4]) + (3600).to_bytes(4, "big") + bytes([1, 4]) + a("255.255.255.0") +
                    bytes([3, 4]) + a(HOST_IP) + bytes([6, 4]) + a(HOST_IP) + b"\xff")
            out = (bytes([2, 1, 6, 0]) + pkt[4:8] + b"\0\0\x80\0" + b"\0" * 4 + a(ip) + a(HOST_IP) +
                   b"\0" * 4 + pkt[28:44] + b"\0" * 192 + b"\x63\x82\x53\x63" + opts)
            try:
                self.sock.sendto(out, ("255.255.255.255", 68))
            except OSError:
                pass


class Network:
    """br0 + taps + DHCP + the apk mirror, inside the namespace."""

    def __init__(self, run_dir: Path, mirror_dir: Path | None, guests: int):
        self.run_dir = run_dir
        self.mirror_dir = mirror_dir
        self.n = guests
        self.procs: list[subprocess.Popen] = []

    def up(self):
        sh("ip", "link", "set", "lo", "up")
        sh("ip", "link", "add", "br0", "type", "bridge")
        sh("ip", "addr", "add", f"{HOST_IP}/24", "dev", "br0")
        sh("ip", "addr", "add", f"{META_IP}/32", "dev", "br0")
        sh("ip", "link", "set", "br0", "up")
        for i in range(1, self.n + 1):
            sh("ip", "tuntap", "add", "dev", f"tap{i}", "mode", "tap")
            sh("ip", "link", "set", f"tap{i}", "master", "br0")
            sh("ip", "link", "set", f"tap{i}", "up")
        self.dhcp = DhcpServer({mac_for(i): ip_for(i) for i in range(1, self.n + 1)})
        self.dhcp.start()
        if self.mirror_dir is not None:
            self.procs.append(subprocess.Popen(
                [sys.executable, "-m", "http.server", str(MIRROR_PORT), "--bind", HOST_IP,
                 "--directory", str(self.mirror_dir)],
                stdout=open(self.run_dir / "mirror.log", "wb"), stderr=subprocess.STDOUT))

    def down(self):
        if getattr(self, 'dhcp', None):
            self.dhcp.stop()
        for p in self.procs:
            p.terminate()
        for p in self.procs:
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()


# ----------------------------------------------------------------------- guest


class Guest:
    """One QEMU+KVM guest on the main amd64 kernel, started and stopped by us."""

    def __init__(self, n, disk, kernel, run_dir, key, mem=3072, smp=2, cmdline_extra=""):
        self.n, self.disk, self.kernel = n, Path(disk), Path(kernel)
        self.run_dir, self.key = Path(run_dir), Path(key)
        self.mem, self.smp, self.extra = mem, smp, cmdline_extra
        self.ip, self.mac = ip_for(n), mac_for(n)
        self.proc = None
        self.serial = self.run_dir / f"guest{n}.serial.log"

    def start(self):
        cmdline = ("virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 "
                   "init=/bin/herd " + self.extra).strip()
        args = ["qemu-system-x86_64", "-M", "microvm", "-accel", "kvm", "-cpu", "host",
                "-global", "virtio-mmio.force-legacy=false", "-kernel", str(self.kernel),
                "-m", str(self.mem), "-smp", str(self.smp),
                "-drive", f"id=d0,file={self.disk},format=raw,if=none,snapshot=on",
                "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0",
                "-netdev", f"tap,id=n0,ifname=tap{self.n},script=no,downscript=no",
                "-device", f"virtio-net-device,netdev=n0,mac={self.mac},bus=virtio-mmio-bus.1",
                "-append", cmdline, "-serial", f"file:{self.serial}", "-display", "none",
                "-no-reboot"]
        (self.run_dir / f"guest{self.n}.qemu.cmd").write_text(shlex.join(args) + "\n")
        # Its own process group: teardown signals this QEMU and nothing else.
        self.proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT,
                                     start_new_session=True)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait()

    def ssh_argv(self, *cmd, port=22, user="root", extra=()):
        return ["ssh", "-i", str(self.key), "-p", str(port), "-o", "BatchMode=yes",
                "-o", "IdentitiesOnly=yes", "-o", "ConnectTimeout=3",
                "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR", *extra, f"{user}@{self.ip}", *cmd]

    def run(self, command: str, timeout=120, check=False):
        """Run a shell command in the guest's host root over ssh; (rc, stdout+stderr)."""
        r = subprocess.run(self.ssh_argv(command), capture_output=True, text=True, timeout=timeout)
        out = r.stdout + r.stderr
        if check and r.returncode:
            raise RuntimeError(f"guest{self.n}: `{command}` rc={r.returncode}\n{out[-2000:]}")
        return r.returncode, out

    def wait_ready(self, timeout=240):
        """Readiness is a completed ssh round-trip (scripts/vm_ready.py), never a log marker."""
        sys.path.insert(0, str(ROOT / "scripts"))
        import vm_ready
        if not vm_ready.wait_ready(22, timeout, 2, self.proc, host=self.ip, identity=self.key):
            raise RuntimeError(f"guest{self.n} not ready after {timeout}s (see {self.serial})")


def sha256_hex(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def gen_key(path: Path) -> str:
    sh("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "eco-launch", "-f", str(path))
    return path.with_suffix(".pub").read_text().strip()
