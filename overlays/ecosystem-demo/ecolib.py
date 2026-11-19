#!/usr/bin/env python3
"""Shared host-side pieces of the ecosystem demo launcher (see README.md).

Each guest runs behind QEMU user-mode networking (SLIRP): no privileges, and the guest has the host's
internet through QEMU's NAT, which is what a box needs to reach a model API or Maven Central. Three
QEMU features replace a bridge:

    hostfwd   host 127.0.0.1:P -> guest :22 (ssh) and :8081/:8082 (the services). Guest 2 reaches guest 1
              at http://169.254.0.2:P, because SLIRP maps its `host=` address to the host's loopback.
    guestfwd  the guest's 169.254.169.254:80 -> this guest's own MetadataServer. One server per guest.
    169.254.0.2  the host as the guest sees it: the pinned apk mirror is served on 127.0.0.1:8000.

The SLIRP network is 169.254.0.0/16 (not the default 10.0.2.0/24) because guestfwd only accepts an
address inside that network and the metadata service must be at its real address, 169.254.169.254.

The same shape (a per-guest forward for ssh and services, a metadata endpoint) is what Firecracker's
MMDS + tap setup provides, so the guest images do not change between backends.
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
META_IP = "169.254.169.254"
HOST_ALIAS = "169.254.0.2"   # the host as a guest sees it (SLIRP `host=`)
MIRROR_PORT = 8000

def sh(*args, check=True, **kw):
    return subprocess.run(args, check=check, text=True, **kw)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


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
    """An EC2-IMDSv2-shaped endpoint for ONE guest, started by the launcher per guest.

    It listens on a host loopback port; QEMU's guestfwd carries the guest's
    169.254.169.254:80 to it, so the guest uses the real address. Requests are logged
    by (method, path, status) and never by body or token, so the log can be kept and
    read without leaking a secret.
    """

    def __init__(self, guest: "GuestMeta", bind=("127.0.0.1", 0), require_token=True):
        self.guest = guest
        self.guests = {}
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
                g = outer.guest
                if g is None or self.path != "/latest/api/token":
                    return self._send(404)
                tok = secrets.token_hex(16)
                outer.tokens[tok] = time.monotonic() + 60
                g.log.append(("PUT", self.path, 200))
                self._send(200, tok.encode())

            def do_GET(self):
                g = outer.guest
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

    @property
    def port(self) -> int:
        return self.httpd.server_address[1]

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


# ---------------------------------------------------------------------- mirror


class MirrorServer:
    """The pinned apk mirror, on 127.0.0.1:8000 (the guest's http://169.254.0.2:8000)."""

    def __init__(self, directory: Path, run_dir: Path):
        self.proc = subprocess.Popen(
            [sys.executable, "-m", "http.server", str(MIRROR_PORT), "--bind", "127.0.0.1", "--directory", str(directory)],
            stdout=open(run_dir / "mirror.log", "wb"), stderr=subprocess.STDOUT)

    def stop(self):
        self.proc.terminate()
        self.proc.wait(timeout=5)


# ----------------------------------------------------------------------- guest


class Guest:
    """One QEMU+KVM guest on the main amd64 kernel behind SLIRP, started and stopped by us."""

    def __init__(self, n, disk, kernel, run_dir, key, md_port, mem=3072, smp=2, forwards=(), cmdline_extra=""):
        self.n, self.disk, self.kernel = n, Path(disk), Path(kernel)
        self.run_dir, self.key = Path(run_dir), Path(key)
        self.mem, self.smp, self.extra = mem, smp, cmdline_extra
        self.md_port = md_port
        self.ssh_port = free_port()
        self.forwards = {p: free_port() for p in forwards}       # guest port -> host port
        self.proc = None
        self.serial = self.run_dir / f"guest{n}.serial.log"

    def start(self):
        cmdline = ("virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 "
                   f"init=/bin/herd epoch={int(time.time())} " + self.extra).strip()
        fwd = f"hostfwd=tcp:127.0.0.1:{self.ssh_port}-:22," + "".join(
            f"hostfwd=tcp:127.0.0.1:{h}-:{g}," for g, h in self.forwards.items())
        net = f"user,id=n0,net=169.254.0.0/16,host={HOST_ALIAS},dhcpstart=169.254.0.15,dns=169.254.0.3,{fwd}guestfwd=tcp:{META_IP}:80-cmd:nc 127.0.0.1 {self.md_port}"
        args = ["qemu-system-x86_64", "-M", "microvm", "-accel", "kvm", "-cpu", "host",
                "-global", "virtio-mmio.force-legacy=false", "-kernel", str(self.kernel),
                "-m", str(self.mem), "-smp", str(self.smp),
                "-drive", f"id=d0,file={self.disk},format=raw,if=none,snapshot=on",
                "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0",
                "-netdev", net,
                "-device", "virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1",
                "-append", cmdline, "-serial", f"file:{self.serial}", "-display", "none",
                "-no-reboot"]
        (self.run_dir / f"guest{self.n}.qemu.cmd").write_text(shlex.join(args) + "\n")
        # Its own process group: teardown signals this QEMU and nothing else.
        self.proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=open(self.run_dir / f"guest{self.n}.qemu.log", "wb"),
                                     start_new_session=True)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait()

    def ssh_argv(self, *cmd, user="root", extra=()):
        return ["ssh", "-i", str(self.key), "-p", str(self.ssh_port), "-o", "BatchMode=yes",
                "-o", "IdentitiesOnly=yes", "-o", "ConnectTimeout=3",
                "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR", *extra, f"{user}@127.0.0.1", *cmd]

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
        if not vm_ready.wait_ready(self.ssh_port, timeout, 2, self.proc, host="127.0.0.1", identity=self.key):
            raise RuntimeError(f"guest{self.n} not ready after {timeout}s (see {self.serial})")


def sha256_hex(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def gen_key(path: Path) -> str:
    sh("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "eco-launch", "-f", str(path))
    return path.with_suffix(".pub").read_text().strip()
