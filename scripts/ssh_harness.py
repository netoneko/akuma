#!/usr/bin/env python3
"""
Akuma SSH test harness.

A library-driven SSH client that bypasses the `ssh` binary entirely so it can
run inside sandboxed environments that allowlist Python but not the OpenSSH
client. Used to exercise the SSH stability instrumentation added in
src/ssh/{server,protocol,keys}.rs.

Run via the project venv:
    ./venv/bin/python scripts/ssh_harness.py <subcommand> [options]

All subcommands default to host=127.0.0.1, port=2222, user=user.
The harness installs no known_hosts entry — it always accepts whatever
host key the server presents. The host-key fingerprint is reported on
each connect so test scripts can verify persistence across reboots
(see C2 in the SSH stability plan).
"""
from __future__ import annotations

import argparse
import csv
import io
import json
import os
import socket
import statistics
import sys
import threading
import time
from contextlib import closing
from dataclasses import dataclass, field
from typing import Optional

try:
    import paramiko
    # Akuma advertises the bare-name "curve25519-sha256" (RFC 8731 form);
    # paramiko 5.0+ dropped it from its default preferred list and only
    # keeps "curve25519-sha256@libssh.org" (semantically identical).
    # Both names are accepted by RFC, and both are interchangeable in
    # wire protocol. Patch paramiko's preferred list so the harness can
    # negotiate against akuma without modifying the kernel's
    # advertisement.
    _kex = paramiko.Transport._preferred_kex
    if "curve25519-sha256" not in _kex:
        paramiko.Transport._preferred_kex = ("curve25519-sha256",) + _kex
    # The two names share an engine — alias the bare name in _kex_info too.
    if (
        "curve25519-sha256" not in paramiko.Transport._kex_info
        and "curve25519-sha256@libssh.org" in paramiko.Transport._kex_info
    ):
        paramiko.Transport._kex_info["curve25519-sha256"] = (
            paramiko.Transport._kex_info["curve25519-sha256@libssh.org"]
        )
except ImportError:
    sys.stderr.write(
        "paramiko is not installed in this interpreter. Run:\n"
        "    ./venv/bin/pip install paramiko\n"
    )
    sys.exit(2)


DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 2222
DEFAULT_USER = "user"


# ---------------------------------------------------------------------------
# Shared connection helpers
# ---------------------------------------------------------------------------


@dataclass
class ConnectResult:
    ok: bool
    latency_ms: float
    reason: str = ""
    host_key_type: str = ""
    host_key_fp: str = ""


def _try_password_auth(client: paramiko.SSHClient, host: str, port: int,
                       user: str, timeout: float) -> None:
    """
    Authenticate with the project's conventional credentials. Akuma's SSH
    server currently only accepts pubkey auth (see `src/ssh/keys.rs`); we
    try a key path first, then fall back to password for older test builds.

    Key precedence: ``AKUMA_SSH_KEY`` env var → ``~/.ssh/id_ed25519`` →
    ``~/.ssh/id_rsa``. The bootstrap disk ships
    `bootstrap/etc/sshd/authorized_keys` so the host's id_ed25519 is the
    expected match.
    """
    key_candidates = []
    env_key = os.environ.get("AKUMA_SSH_KEY")
    if env_key:
        key_candidates.append(env_key)
    home = os.path.expanduser("~")
    key_candidates.extend([
        os.path.join(home, ".ssh", "id_ed25519"),
        os.path.join(home, ".ssh", "id_rsa"),
    ])
    key_filename = next((p for p in key_candidates if os.path.isfile(p)), None)

    try:
        client.connect(
            hostname=host,
            port=port,
            username=user,
            key_filename=key_filename,
            password=os.environ.get("AKUMA_SSH_PASSWORD", ""),
            allow_agent=False,
            look_for_keys=False,
            timeout=timeout,
            auth_timeout=timeout,
            banner_timeout=timeout,
        )
    except paramiko.ssh_exception.BadAuthenticationType:
        # Server doesn't accept our methods — retry without the password so
        # paramiko fails with a useful error rather than mis-classifying it.
        client.connect(
            hostname=host,
            port=port,
            username=user,
            key_filename=key_filename,
            allow_agent=False,
            look_for_keys=False,
            timeout=timeout,
            auth_timeout=timeout,
            banner_timeout=timeout,
        )


def _kex_once(host: str, port: int, timeout: float) -> ConnectResult:
    """
    Reach AwaitingUserAuth, capture the host key, then immediately close.
    This is the stability-relevant probe: it exercises TCP accept, SSH
    handshake (version + kex + newkeys), and the AwaitingUserAuth exit
    path. It does NOT require a valid authorized_keys entry on the VM.
    Auth-stage exit shows up in the server's AUTH_FAIL counter, which is
    the expected behavior; the harness reports ok=True iff KEX
    completed.
    """
    start = time.perf_counter()
    fp_type = ""
    fp = ""
    transport: Optional[paramiko.Transport] = None
    sock = None
    try:
        sock = socket.create_connection((host, port), timeout=timeout)
        transport = paramiko.Transport(sock)
        transport.start_client(timeout=timeout)
        host_key = transport.get_remote_server_key()
        fp_type = host_key.get_name()
        fp = host_key.get_fingerprint().hex()
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        return ConnectResult(True, elapsed_ms, "", fp_type, fp)
    except Exception as exc:
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        return ConnectResult(False, elapsed_ms, repr(exc), fp_type, fp)
    finally:
        if transport is not None:
            try:
                transport.close()
            except Exception:
                pass
        if sock is not None:
            try:
                sock.close()
            except Exception:
                pass


def _connect_once(host: str, port: int, user: str,
                  timeout: float) -> ConnectResult:
    """
    Full handshake + auth + close. Will only succeed if /etc/sshd/
    authorized_keys contains a matching pubkey (or password auth is
    enabled, which akuma does not enable by default).
    """
    start = time.perf_counter()
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    # Don't load the user's known_hosts — we want the harness to be
    # idempotent and not pollute ~/.ssh/known_hosts.
    client._host_keys = paramiko.HostKeys()

    fp_type = ""
    fp = ""
    try:
        _try_password_auth(client, host, port, user, timeout)
        transport = client.get_transport()
        if transport is not None:
            host_key = transport.get_remote_server_key()
            fp_type = host_key.get_name()
            fp = host_key.get_fingerprint().hex()
        # Force a trivial command roundtrip so we know the channel layer works.
        stdin, stdout, stderr = client.exec_command("true", timeout=timeout)
        stdout.channel.recv_exit_status()
    except Exception as exc:
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        return ConnectResult(False, elapsed_ms, repr(exc), fp_type, fp)
    finally:
        client.close()
    elapsed_ms = (time.perf_counter() - start) * 1000.0
    return ConnectResult(True, elapsed_ms, "", fp_type, fp)


# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------


def cmd_connect(args: argparse.Namespace) -> int:
    r = _kex_once(args.host, args.port, args.timeout)
    payload = {
        "ok": r.ok,
        "latency_ms": round(r.latency_ms, 2),
        "reason": r.reason,
        "host_key_type": r.host_key_type,
        "host_key_fp": r.host_key_fp,
    }
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stderr.write(
        f"connect: ok={r.ok} latency={r.latency_ms:.1f}ms "
        f"hk={r.host_key_type} fp={r.host_key_fp or '-'} {r.reason}\n"
    )
    return 0 if r.ok else 1


def cmd_echo(args: argparse.Namespace) -> int:
    """
    Open an interactive channel, push N bytes through and read them back,
    measuring per-byte round-trip latency. Targets jitter findings J1/J2/J3.
    """
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    client._host_keys = paramiko.HostKeys()
    try:
        _try_password_auth(client, args.host, args.port, args.user,
                           args.timeout)
    except Exception as exc:
        sys.stderr.write(f"echo: connect failed: {exc!r}\n")
        return 1

    transport = client.get_transport()
    assert transport is not None
    channel = transport.open_session()
    channel.get_pty(term="xterm", width=80, height=24)
    channel.invoke_shell()

    # Drain banner / prompt so it doesn't pollute timing.
    deadline = time.perf_counter() + 1.0
    while time.perf_counter() < deadline:
        if channel.recv_ready():
            channel.recv(4096)
        else:
            time.sleep(0.02)

    samples: list[float] = []
    payload = b"x" * args.size
    for i in range(args.count):
        start = time.perf_counter()
        channel.send(payload + b"\n")
        # Wait until at least `size` bytes echo back (akuma's shell echoes
        # input by default for interactive PTY sessions). Bail after 1s.
        wanted = len(payload)
        got = 0
        while got < wanted:
            if channel.recv_ready():
                got += len(channel.recv(4096))
            elif time.perf_counter() - start > 1.0:
                break
            else:
                time.sleep(0.001)
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        samples.append(elapsed_ms)

    channel.close()
    client.close()

    if not samples:
        sys.stderr.write("echo: no samples collected\n")
        return 1

    samples_sorted = sorted(samples)

    def pct(p: float) -> float:
        if not samples_sorted:
            return 0.0
        i = max(0, min(len(samples_sorted) - 1,
                       int(round((p / 100.0) * (len(samples_sorted) - 1)))))
        return samples_sorted[i]

    outliers = [s for s in samples if s > 100.0]
    payload = {
        "n": len(samples),
        "size_bytes": args.size,
        "p50_ms": round(pct(50), 2),
        "p95_ms": round(pct(95), 2),
        "p99_ms": round(pct(99), 2),
        "min_ms": round(samples_sorted[0], 2),
        "max_ms": round(samples_sorted[-1], 2),
        "mean_ms": round(statistics.fmean(samples), 2),
        "outliers_over_100ms": len(outliers),
    }
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stderr.write(
        f"echo: n={payload['n']} p50={payload['p50_ms']}ms "
        f"p95={payload['p95_ms']}ms p99={payload['p99_ms']}ms "
        f"max={payload['max_ms']}ms outliers>100ms={len(outliers)}\n"
    )
    return 0 if payload["p95_ms"] < args.fail_above_p95_ms else 1


def cmd_soak(args: argparse.Namespace) -> int:
    """Repeated connect-disconnect to exercise lifecycle counters."""
    writer = csv.writer(sys.stdout)
    writer.writerow(["ts", "ok", "latency_ms", "reason", "host_key_fp"])

    end = time.time() + args.duration
    ok = 0
    fail = 0
    fps = set()
    while time.time() < end:
        r = _kex_once(args.host, args.port, args.timeout)
        writer.writerow([
            f"{time.time():.3f}",
            int(r.ok),
            f"{r.latency_ms:.2f}",
            r.reason,
            r.host_key_fp,
        ])
        sys.stdout.flush()
        if r.ok:
            ok += 1
            if r.host_key_fp:
                fps.add(r.host_key_fp)
        else:
            fail += 1
        time.sleep(max(0.0, args.interval))

    sys.stderr.write(
        f"soak: ok={ok} fail={fail} unique_host_keys={len(fps)} "
        f"({'stable' if len(fps) <= 1 else 'CHANGED'})\n"
    )
    return 0 if fail == 0 else 1


@dataclass
class ParallelStat:
    ok: int = 0
    fail: int = 0
    latencies: list[float] = field(default_factory=list)


def _parallel_worker(args: argparse.Namespace, stat: ParallelStat,
                     stop_at: float) -> None:
    while time.time() < stop_at:
        r = _kex_once(args.host, args.port, args.timeout)
        _ = args.user  # silence unused-attr warnings; auth is intentionally skipped
        if r.ok:
            stat.ok += 1
            stat.latencies.append(r.latency_ms)
        else:
            stat.fail += 1


def cmd_parallel(args: argparse.Namespace) -> int:
    """N workers hammering the accept loop in parallel."""
    stats = [ParallelStat() for _ in range(args.count)]
    threads = []
    stop_at = time.time() + args.duration
    for i in range(args.count):
        t = threading.Thread(
            target=_parallel_worker,
            args=(args, stats[i], stop_at),
            daemon=True,
        )
        t.start()
        threads.append(t)
    for t in threads:
        t.join()

    total_ok = sum(s.ok for s in stats)
    total_fail = sum(s.fail for s in stats)
    all_latencies = [x for s in stats for x in s.latencies]
    payload = {
        "workers": args.count,
        "duration_s": args.duration,
        "ok": total_ok,
        "fail": total_fail,
        "mean_latency_ms": round(statistics.fmean(all_latencies), 2)
            if all_latencies else 0.0,
        "max_latency_ms": round(max(all_latencies), 2) if all_latencies else 0.0,
    }
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stderr.write(
        f"parallel: workers={args.count} ok={total_ok} fail={total_fail} "
        f"mean={payload['mean_latency_ms']}ms max={payload['max_latency_ms']}ms\n"
    )
    return 0 if total_fail == 0 else 1


def cmd_auth_probe(args: argparse.Namespace) -> int:
    """
    Hit the server with a deliberate bad-password attempt and an attempted
    pubkey auth using a freshly-generated ed25519 key that the server has
    never seen. Surfaces the AUTH_FAIL counter (A1) and the loud
    authorized_keys-missing warning (A3) in the kernel log.
    """
    # First: bad password.
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    client._host_keys = paramiko.HostKeys()
    pw_reason = ""
    pw_ok = False
    try:
        client.connect(
            hostname=args.host,
            port=args.port,
            username=args.user,
            password="!!intentionally-bad-password!!",
            allow_agent=False,
            look_for_keys=False,
            timeout=args.timeout,
            auth_timeout=args.timeout,
        )
        pw_ok = True
    except paramiko.AuthenticationException as exc:
        pw_reason = f"auth_fail_as_expected: {exc!r}"
    except Exception as exc:
        pw_reason = f"unexpected: {exc!r}"
    finally:
        client.close()

    # Second: throwaway pubkey (paramiko 5.0 dropped Ed25519Key.generate, so
    # we go through `cryptography` directly and re-import via PEM).
    import io as _io
    from cryptography.hazmat.primitives.asymmetric import ed25519 as _ed
    from cryptography.hazmat.primitives import serialization as _ser
    _priv = _ed.Ed25519PrivateKey.generate()
    _pem = _priv.private_bytes(
        encoding=_ser.Encoding.PEM,
        format=_ser.PrivateFormat.OpenSSH,
        encryption_algorithm=_ser.NoEncryption(),
    )
    pk = paramiko.Ed25519Key.from_private_key(_io.StringIO(_pem.decode()))
    pk_reason = ""
    pk_ok = False
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    client._host_keys = paramiko.HostKeys()
    try:
        client.connect(
            hostname=args.host,
            port=args.port,
            username=args.user,
            pkey=pk,
            allow_agent=False,
            look_for_keys=False,
            timeout=args.timeout,
            auth_timeout=args.timeout,
        )
        pk_ok = True
    except paramiko.AuthenticationException as exc:
        pk_reason = f"auth_fail_as_expected: {exc!r}"
    except Exception as exc:
        pk_reason = f"unexpected: {exc!r}"
    finally:
        client.close()

    payload = {
        "bad_password_accepted": pw_ok,
        "bad_password_reason": pw_reason,
        "unknown_pubkey_accepted": pk_ok,
        "unknown_pubkey_reason": pk_reason,
    }
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stderr.write(
        f"auth-probe: bad_password_accepted={pw_ok} "
        f"unknown_pubkey_accepted={pk_ok}\n"
    )
    # Healthy = both rejected. Either acceptance is a problem.
    return 0 if (not pw_ok and not pk_ok) else 1


def cmd_burst(args: argparse.Namespace) -> int:
    """
    Open MAX_CONNECTIONS+overflow KEX-only sessions as fast as possible,
    hold them, then close. Each session stops at AwaitingUserAuth — that's
    enough to count against ACTIVE_SESSIONS on the server side, so this
    exercises the burst-handshake path and the MAX_CONNECTIONS gate.
    """
    n = args.count
    transports: list = []  # (Transport, socket) pairs
    opened = 0
    failed = 0
    for _ in range(n):
        try:
            sock = socket.create_connection((args.host, args.port),
                                            timeout=args.timeout)
            t = paramiko.Transport(sock)
            t.start_client(timeout=args.timeout)
            transports.append((t, sock))
            opened += 1
        except Exception as exc:
            failed += 1
            sys.stderr.write(f"burst: connect failed: {exc!r}\n")
    sys.stderr.write(f"burst: opened={opened} failed={failed} (target={n})\n")
    time.sleep(args.hold)
    for t, sock in transports:
        try:
            t.close()
        except Exception:
            pass
        try:
            sock.close()
        except Exception:
            pass
    sys.stdout.write(json.dumps({"opened": opened, "failed": failed,
                                 "target": n}) + "\n")
    return 0


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def _common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--user", default=DEFAULT_USER)
    parser.add_argument("--timeout", type=float, default=10.0,
                        help="Per-stage timeout in seconds")


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="Akuma SSH test harness")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_connect = sub.add_parser("connect", help="single handshake smoke test")
    _common(p_connect)
    p_connect.set_defaults(func=cmd_connect)

    p_echo = sub.add_parser("echo", help="measure round-trip echo latency")
    _common(p_echo)
    p_echo.add_argument("--size", type=int, default=16,
                        help="bytes per echo round")
    p_echo.add_argument("--count", type=int, default=50,
                        help="number of echo rounds")
    p_echo.add_argument("--fail-above-p95-ms", type=float, default=500.0,
                        help="non-zero exit if p95 latency exceeds this")
    p_echo.set_defaults(func=cmd_echo)

    p_soak = sub.add_parser("soak", help="repeated connect/disconnect")
    _common(p_soak)
    p_soak.add_argument("--interval", type=float, default=5.0)
    p_soak.add_argument("--duration", type=float, default=60.0)
    p_soak.set_defaults(func=cmd_soak)

    p_par = sub.add_parser("parallel", help="N concurrent connect/echo loops")
    _common(p_par)
    p_par.add_argument("--count", type=int, default=4)
    p_par.add_argument("--duration", type=float, default=30.0)
    p_par.set_defaults(func=cmd_parallel)

    p_auth = sub.add_parser("auth-probe",
                            help="probe pubkey + password auth rejection paths")
    _common(p_auth)
    p_auth.set_defaults(func=cmd_auth_probe)

    p_burst = sub.add_parser("burst",
                             help="open N concurrent sessions, hold, close")
    _common(p_burst)
    p_burst.add_argument("--count", type=int, default=6,
                         help="concurrent session count (try > MAX_CONNECTIONS)")
    p_burst.add_argument("--hold", type=float, default=2.0,
                         help="seconds to hold sessions open")
    p_burst.set_defaults(func=cmd_burst)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
