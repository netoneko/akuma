#!/usr/bin/env python3
"""Run the `mtstress` multi-threaded SMP probe, on Akuma and on real Linux.

`userspace/amd64/mtstress/mtstress.c` is the LLD-shaped probe: one process,
many threads, one address space — the shape the three open SMP failures in
`docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §11 share and that the
process-shaped probes (`smpstress`, `execleak2`) do not exercise.

    scripts/benchmarks/amd64_mtstress_run.py                    # linux, 1, 4
    scripts/benchmarks/amd64_mtstress_run.py --cells 4 --secs 300
    scripts/benchmarks/amd64_mtstress_run.py --modes p --secs 60 # corruption only
    scripts/benchmarks/amd64_mtstress_run.py --no-linux

# The Linux arm is not optional by default, and that is the point

The same static musl binary runs on the box's Ubuntu before it runs on Akuma.
A probe that has only ever been run against the kernel it was written to
accuse is not yet evidence — this repo has paid for that lesson more than once
(`scripts/probes/`, `mem_suite --linux`, and
`docs/archive/AKUMA_AMD64_STEP5B_SLICE4_PROCS.md`). If the Linux arm does not
report PASS, the probe is wrong and nothing it says about Akuma counts.

Everything runs on the box; nothing here reboots it.
"""

import argparse
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "utils"))
import hpbox  # noqa: E402

SRC = REPO / "userspace/amd64/mtstress/mtstress.c"
GUEST_IP = "10.0.2.15"
GUEST_KEY = "/root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key"
FC_JSON = "/root/akuma-fc.json"
FC_LOG = "/root/akuma-fc.log"
BOX_BIN = "/root/mtstress"

SSH = (f"ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "
       f"-o LogLevel=ERROR -o ConnectTimeout=8 -i {GUEST_KEY} "
       f"-p 2222 root@{GUEST_IP}")


def build_local(out):
    """Cross-compile the probe here. The kernel is built from an Apple Silicon
    machine, so `cc` is Apple clang and cannot emit ELF; `x86_64-linux-musl-gcc`
    (Homebrew `musl-cross`) is the one that can."""
    cc = shutil.which("x86_64-linux-musl-gcc")
    if not cc:
        raise SystemExit("x86_64-linux-musl-gcc not found (brew install FiloSottile/musl-cross/musl-cross)")
    r = subprocess.run([cc, "-static", "-O2", "-pthread", "-Wall", "-Wextra",
                        "-o", str(out), str(SRC)],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit("probe build failed:\n" + r.stdout + r.stderr)
    return out


def push_to_box(local):
    """Copy the probe to the box's Ubuntu side, base64 over the ssh channel.

    `scp` would need its own host-key handling and the `akuma` alias trap;
    this goes through the one transport `hpbox` already gets right.
    """
    import base64
    data = base64.b64encode(local.read_bytes()).decode()
    chunk = 60000
    hpbox.ubuntu(f"rm -f {BOX_BIN}.b64 {BOX_BIN}", timeout=60)
    for i in range(0, len(data), chunk):
        part = data[i:i + chunk]
        r = subprocess.run(hpbox.UB + [f"cat >> {BOX_BIN}.b64"], input=part,
                           capture_output=True, text=True, timeout=120)
        if r.returncode != 0:
            raise SystemExit("push failed: " + r.stderr)
    rc, out, err = hpbox.ubuntu(
        f"base64 -d {BOX_BIN}.b64 > {BOX_BIN} && chmod +x {BOX_BIN} && "
        f"rm -f {BOX_BIN}.b64 && ls -la {BOX_BIN}", timeout=120)
    return rc, (out + err).strip()


def verdict(blob):
    """PASS / FAIL / SILENT. A probe that printed nothing is never a pass."""
    if "mtstress: PASS" in blob:
        return "PASS"
    if ("mtstress: FAIL" in blob or "MTSTRESS-FAIL" in blob
            or "MTSTRESS-STUCK" in blob or "MTSTRESS-WAITSTUCK" in blob):
        return "FAIL"
    if not blob.strip():
        return "SILENT"
    return "NO-VERDICT"


def findings(blob):
    return [ln for ln in blob.splitlines() if ln.startswith("MTSTRESS-")]


def run_linux(secs, threads, modes):
    """The calibration arm: the same binary, on the box's own Ubuntu."""
    rc, out, err = hpbox.ubuntu(f"{BOX_BIN} {secs} {threads} {modes}",
                                timeout=secs + 180)
    return rc, out + err


def set_vcpus(n):
    py = (f"import json;d=json.load(open('{FC_JSON}'));"
          f"d['machine-config']['vcpu_count']={n};"
          f"json.dump(d,open('{FC_JSON}','w'))")
    return hpbox.ubuntu(f'python3 -c "{py}"', timeout=60)


def guest_up(wait_s=180):
    hpbox.ubuntu("sh /root/akuma-fc-run.sh", timeout=120)
    t0 = time.time()
    while time.time() - t0 < wait_s:
        rc, out, _ = hpbox.ubuntu(f"{SSH} 'echo READY'", timeout=40)
        if "READY" in out:
            return True
        time.sleep(4)
    return False


def push_to_guest():
    """Base64 the probe from the box into the running guest.

    busybox in this guest has **no `base64` applet**, and the failure is the
    worst kind: the shell creates the redirect target before discovering the
    command is missing, so `base64 -d > /bin/x` leaves a zero-byte file and the
    probe then "runs" and prints nothing. Check for the applet, and fall back
    to writing raw bytes down the channel (ssh with no `-t` is 8-bit clean).
    """
    rc, out, _ = hpbox.ubuntu(f"{SSH} 'which base64'", timeout=60)
    if "base64" in out:
        cmd = (f"base64 {BOX_BIN} | {SSH} "
               f"'base64 -d > /root/mtstress && chmod 755 /root/mtstress'")
    else:
        cmd = f"{SSH} 'cat > /root/mtstress' < {BOX_BIN}"
    hpbox.ubuntu(cmd, timeout=300)
    rc, out, err = hpbox.ubuntu(f"{SSH} 'chmod 755 /root/mtstress; ls -la /root/mtstress'",
                                timeout=60)
    return (out + err).strip()


def run_akuma(vcpus, secs, threads, modes):
    set_vcpus(vcpus)
    if not guest_up():
        rc, out, _ = hpbox.ubuntu(f"tail -c 1500 {FC_LOG}", timeout=60)
        return "NOBOOT", out.strip()[-1200:], []
    staged = push_to_guest()
    if "No such file" in staged or "0 " in staged.split("root")[-1][:8]:
        return "NOSTAGE", staged, []
    try:
        rc, out, err = hpbox.ubuntu(f"{SSH} '/root/mtstress {secs} {threads} {modes}'",
                                    timeout=secs + 300)
        blob = out + err
    except Exception as exc:
        # The probe never returned: that is the wedge, and the console plus a
        # second connection is the evidence §10 used.
        rc2, log, _ = hpbox.ubuntu(f"tail -c 3000 {FC_LOG}", timeout=60)
        rc3, ps, _ = hpbox.ubuntu(f"{SSH} 'ps'", timeout=60)
        return "WEDGE", f"{type(exc).__name__}\n--- ps ---\n{ps}\n--- console ---\n{log[-1500:]}", []
    return verdict(blob), blob, findings(blob)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cells", default="1,4",
                    help="comma-separated guest vcpu counts (default 1,4)")
    ap.add_argument("--secs", type=int, default=120)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--modes", default="pschf")
    ap.add_argument("--no-linux", action="store_true",
                    help="skip the calibration arm (you are asserting it already passed)")
    a = ap.parse_args()

    if hpbox.which_system() != "ubuntu":
        print("the box is not on Ubuntu — Firecracker runs there.")
        return 2

    tmp = Path("/tmp") / "mtstress.bin"
    build_local(tmp)
    rc, msg = push_to_box(tmp)
    print("staged on box:", msg)

    results = []
    if not a.no_linux:
        print(f"--- linux (the box's own Ubuntu), {a.threads} threads, {a.secs}s ...",
              flush=True)
        rc, blob = run_linux(a.secs, a.threads, a.modes)
        v = verdict(blob)
        results.append(("linux", v, findings(blob), blob))
        print(f"    {v}", flush=True)
        if v != "PASS":
            print("\nThe Linux arm did not pass, so the probe is wrong and nothing "
                  "it says about Akuma counts. Output:\n")
            print(blob[-3000:])
            return 1

    for vcpus in [int(x) for x in a.cells.split(",")]:
        print(f"--- akuma FC vcpu={vcpus}, {a.threads} threads, {a.secs}s ...", flush=True)
        v, blob, f = run_akuma(vcpus, a.secs, a.threads, a.modes)
        results.append((f"akuma-smp{vcpus}", v, f, blob))
        print(f"    {v}" + (f"  ({len(f)} finding(s))" if f else ""), flush=True)

    print("\n=== mtstress ===")
    for name, v, f, _ in results:
        print(f"{name:>14}  {v}" + (f"  {len(f)} finding(s)" if f else ""))
    for name, v, f, blob in results:
        if v != "PASS":
            print(f"\n--- {name}: {v} ---")
            for ln in f[:20]:
                print("  ", ln[:200])
            print((blob or "")[-2500:])

    return 0 if all(v == "PASS" for _, v, _, _ in results) else 1


if __name__ == "__main__":
    sys.exit(main())
