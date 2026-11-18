#!/usr/bin/env python3
"""The merge gate: every kernel, every capability we can check without a person, in parallel.

    scripts/gate.py                      # everything; exit 0 only if every job passed (or failed as expected)
    scripts/gate.py --list               # the jobs, with what each needs
    scripts/gate.py --only taoxichuan    # jobs whose name contains any of the comma-separated words (plus what they need)
    scripts/gate.py --jobs 4,6           # build slots, boot slots (default: sized from the CPU count and RAM)
    scripts/gate.py --no-build           # reuse the kernels already built in $GATE_DIR (a boot-only rerun)

The budget is **10-15 minutes on a warm cache** (the first run on a clean machine builds sixteen kernels
from scratch and is slower; every later run only rebuilds what changed). Everything that can overlap does:

  prep    gateprobe (a static musl program, std only) -> a tiny ext2 root; the main kernel's own root image
  static  the ratchets (`scripts/check_*.py`), host tests of the extracted crates
  build   the sixteen kernel builds `scripts/premerge_build.sh` names, each in its own target dir
  boot    amd64 only, under QEMU microvm + KVM, each on a snapshot of a shared read-only image:
            akuma amd64        the in-kernel boot suite (`Akuma/amd64 self-test: N passed`), SMP 1 and 2, and gateprobe
            taoxichuan, shanghai, denver   gateprobe as `init=` (they boot straight into it)

`gateprobe` prints one `GATE <capability> PASS|FAIL` line per capability: threads (clone+futex), time, memory
(mmap/brk), files, tcp_bind, tcp_loopback, spawn_wait, spawn_many, threads_and_children. The result is a matrix,
capability x kernel. aarch64 kernels are built here but not booted: this machine has no aarch64 KVM, and TCG
is too slow for the budget (see docs/runbooks/merge-gate.md).

Known failures live in scripts/gate/xfail.txt (`kernel<TAB>capability<TAB>reason`). A listed failure does not
fail the gate; a listed capability that PASSES does (XPASS: delete the line), so the list cannot rot - unless the
reason starts with `flaky:`, which allows both outcomes.
Logs: $GATE_DIR/logs/<job>.log. $GATE_DIR defaults to /var/tmp/akuma-gate.
"""
import argparse
import concurrent.futures as cf
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GATE = os.environ.get("GATE_DIR", "/var/tmp/akuma-gate")
LOGS = os.path.join(GATE, "logs")
PROBE_DIR = os.path.join(REPO, "scripts", "gate", "gateprobe")
PROBE_TARGET = os.path.join(GATE, "gateprobe-target")
PROBE_BIN = os.path.join(PROBE_TARGET, "x86_64-unknown-linux-musl", "release", "gateprobe")
GATE_ROOT = os.path.join(GATE, "gate-root.img")
AKUMA_ROOT = os.path.join(GATE, "akuma-root.img")
XFAIL_FILE = os.path.join(REPO, "scripts", "gate", "xfail.txt")
X64, A64 = "x86_64-unknown-none", "aarch64-unknown-none"
CMDLINE_DEVICES = "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6"


class Job:
    def __init__(self, name, kernel, cap, pool, fn, needs=(), timeout=900):
        self.name, self.kernel, self.cap, self.pool, self.fn = name, kernel, cap, pool, fn
        self.needs, self.timeout = list(needs), timeout
        self.status = "PENDING"  # PASS FAIL SKIP XFAIL XPASS
        self.detail = ""
        self.secs = 0.0
        self.caps = {}  # capability -> (ok, detail), for jobs that report several


class Fail(Exception):
    pass


def logpath(name):
    return os.path.join(LOGS, name.replace(":", "_").replace("/", "_") + ".log")


def run(cmd, log, env=None, cwd=REPO, timeout=900, shell=False):
    """Run a command, appending its output to `log`; raise Fail on a non-zero status or timeout."""
    e = dict(os.environ)
    if env:
        e.update(env)
    with open(log, "ab") as f:
        f.write(("$ " + (cmd if shell else shlex.join(cmd)) + "\n").encode())
        f.flush()
        p = subprocess.Popen(cmd, cwd=cwd, env=e, stdout=f, stderr=subprocess.STDOUT, shell=shell,
                             start_new_session=True)
        try:
            rc = p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            raise Fail(f"timed out after {timeout}s")
    if rc != 0:
        raise Fail(f"exit {rc} (see {log})")


# ---- boot ------------------------------------------------------------------------------------------------

def qemu_boot(kernel, disk, log, append, smp=1, mem=1024, net=True, done=None, timeout=120, stall=None):
    """Boot `kernel` on a snapshot of `disk`; return the console text once `done` (a regex) matches or on timeout."""
    serial = log + ".serial"
    open(serial, "w").close()
    cmd = ["qemu-system-x86_64", "-M", "microvm", "-accel", "kvm", "-cpu", "max",
           "-global", "virtio-mmio.force-legacy=false", "-kernel", kernel, "-m", str(mem), "-smp", str(smp),
           "-drive", f"id=d0,file={disk},format=raw,if=none,snapshot=on",
           "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0"]
    if net:
        cmd += ["-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1"]
    cmd += ["-serial", f"file:{serial}", "-display", "none", "-no-reboot", "-append", append]
    with open(log, "ab") as lf:
        lf.write(("$ " + shlex.join(cmd) + "\n").encode())
        p = subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT, start_new_session=True)
        t0 = time.time()
        last_len, last_grow = 0, time.time()
        text = ""
        try:
            while time.time() - t0 < timeout:
                time.sleep(0.5)
                with open(serial, "rb") as f:
                    text = f.read().decode("utf-8", "replace").replace("\r", "")
                if done and re.search(done, text):
                    break
                # `stall`: seconds of console silence, after the guest started printing, that mean it is wedged.
                if stall:
                    if len(text) != last_len:
                        last_len, last_grow = len(text), time.time()
                    elif time.time() - last_grow > stall and "GATE " in text:
                        break
                if p.poll() is not None:
                    break
        finally:
            # Only our own QEMU (its own process group), never a name match: other VMs may be running.
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            p.wait()
    with open(serial, "rb") as f:
        text = f.read().decode("utf-8", "replace").replace("\r", "")
    return text


def read_gate_lines(text):
    caps = {}
    for m in re.finditer(r"^GATE (\w+) (PASS|FAIL)(?: (.*))?$", text, re.M):
        if m.group(1) == "RESULT":
            continue
        caps[m.group(1)] = (m.group(2) == "PASS", m.group(3) or "")
    return caps


def probe_names():
    out = subprocess.run([PROBE_BIN, "--list"], capture_output=True, text=True, check=True).stdout
    return out.split()


def boot_gateprobe(job, kernel_of, disk, smp=2, extra=""):
    def fn():
        k = kernel_of()
        log = logpath(job.name)
        text = qemu_boot(k, disk, log, f"{CMDLINE_DEVICES} epoch={int(time.time())} init=/bin/gateprobe {extra}",
                         smp=smp, mem=1024, done=r"(?m)^GATE RESULT", timeout=150, stall=40)
        caps = read_gate_lines(text)
        for name in probe_names():
            if name not in caps:
                caps[name] = (False, "no result (the probe hung, crashed, or the kernel stopped first)")
        job.caps = caps
        with open(log, "ab") as f:
            f.write(("\n--- console ---\n" + text).encode())
        bad = [n for n, (ok, _) in caps.items() if not ok]
        if "FATAL" in text or "PANIC" in text.upper() and not caps:
            raise Fail("kernel fatal/panic before the probe ran; see log")
        if bad:
            raise Fail("capabilities failed: " + ", ".join(bad))
    return fn


def boot_selftests(job, kernel_of, smp):
    def fn():
        k = kernel_of()
        log = logpath(job.name)
        text = qemu_boot(k, AKUMA_ROOT, log, f"{CMDLINE_DEVICES} init=/bin/herd", smp=smp, mem=1024,
                         done=r"(?m)self-tests passed|SELF-TESTS FAILED", timeout=150)
        with open(log, "ab") as f:
            f.write(("\n--- console ---\n" + text).encode())
        m = re.search(r"self-test: (\d+) passed, (\d+) (?:FAILED|failed)", text)
        if not m:
            job.caps = {"selftests": (False, "no summary line")}
            raise Fail("no 'Akuma/amd64 self-test' summary on the console")
        ok = m.group(2) == "0" and "all self-tests passed" in text
        job.caps = {"selftests": (ok, f"{m.group(1)} passed, {m.group(2)} failed")}
        if not ok:
            fails = re.findall(r"^\s+FAILED: (.*)$", text, re.M)
            raise Fail(f"{m.group(2)} self-tests failed: " + "; ".join(sorted(set(fails))[:5]))
    return fn


def boot_akuma_gateprobe(job, kernel_of):
    """The main kernel runs the same probe from its own ext2 root (`init=/bin/gateprobe`)."""
    return boot_gateprobe(job, kernel_of, AKUMA_ROOT, smp=2)


# ---- the job set -----------------------------------------------------------------------------------------

def step_artifact(step_name, args):
    pkg = re.search(r"-p (\S+)", args).group(1)
    target = re.search(r"--target (\S+)", args).group(1)
    return os.path.join(GATE, "target", step_name, target, "release", pkg)


def build_jobs():
    steps = subprocess.run([os.path.join(REPO, "scripts", "premerge_build.sh"), "--emit-steps"],
                           capture_output=True, text=True, check=True, cwd=REPO).stdout.split("\n")
    out = []
    for line in steps:
        if not line.strip():
            continue
        name, _fast, args = line.split("|", 2)
        out.append((name, args))
    return out


def make_jobs(no_build, with_jvm=False):
    jobs = []

    def add(*a, **k):
        j = Job(*a, **k)
        jobs.append(j)
        return j

    # prep
    def probe_build():
        run(["cargo", "build", "--release", "--target", "x86_64-unknown-linux-musl"], logpath("prep:gateprobe"),
            cwd=PROBE_DIR, env={"CARGO_TARGET_DIR": PROBE_TARGET,
                                "RUSTFLAGS": "-C relocation-model=static -C target-feature=+crt-static"})
        # The probe is checked on the host first: a probe that fails on Linux says nothing about a kernel.
        run([PROBE_BIN], logpath("prep:gateprobe"))

    add("prep:gateprobe", "prep", "gateprobe", "light", probe_build, timeout=300)

    def mkroot():
        if os.path.exists(GATE_ROOT):
            os.remove(GATE_ROOT)
        log = logpath("prep:gate-root")
        run(["truncate", "-s", "64M", GATE_ROOT], log)
        run(["mkfs.ext2", "-q", "-F", GATE_ROOT], log)
        for c in ("mkdir /bin", f"write {PROBE_BIN} /bin/gateprobe", "set_inode_field /bin/gateprobe mode 0100755"):
            run(["debugfs", "-w", "-R", c, GATE_ROOT], log)

    add("prep:gate-root", "prep", "gate-root", "light", mkroot, needs=["prep:gateprobe"], timeout=120)

    def mkakuma():
        log = logpath("prep:akuma-root")
        if os.path.exists(AKUMA_ROOT):
            os.remove(AKUMA_ROOT)
        run(["sh", "amd64/mkdisk.sh", AKUMA_ROOT, "128"], log, timeout=600)
        for c in (f"write {PROBE_BIN} /bin/gateprobe", "set_inode_field /bin/gateprobe mode 0100755"):
            run(["debugfs", "-w", "-R", c, AKUMA_ROOT], log)

    add("prep:akuma-root", "prep", "akuma-root", "light", mkakuma, needs=["prep:gateprobe"], timeout=700)

    # static
    for script in ("check_addr_params", "check_user_ptr_params", "check_unsafe_budget", "check_feature_parity",
                   "check_budgets", "check_no_cloud_secrets"):
        add(f"static:{script}", "repo", script.replace("check_", ""), "light",
            (lambda s=script: run([sys.executable, f"scripts/{s}.py"], logpath(f"static:{s}"))), timeout=300)

    host = subprocess.run(["rustc", "-vV"], capture_output=True, text=True).stdout
    host = re.search(r"host: (\S+)", host).group(1)

    def host_tests():
        # Per crate, like the pre-commit hook: the AArch64-only crates do not build on an x86 host.
        log = logpath("static:host-tests")
        crates = sorted(d for d in os.listdir(os.path.join(REPO, "crates"))
                        if os.path.isdir(os.path.join(REPO, "crates", d)))
        skip = {"akuma-kernel-amd64"}
        if not host.startswith("aarch64"):
            skip |= {"akuma-kernel-glue", "akuma-boot-amd64"}
        pk = []
        for c in crates:
            if c not in skip:
                pk += ["-p", c]
        # One cargo invocation, so shared crates are compiled once; failures name the test binary.
        run(["cargo", "test", "--target", host, "--no-fail-fast"] + pk, log,
            env={"CARGO_TARGET_DIR": os.path.join(GATE, "target", "host")}, timeout=1500)

    add("static:host-tests", "repo", "host-tests", "build", host_tests, timeout=1600)

    # builds
    built = {}
    for name, args in build_jobs():
        def mk(name=name, args=args):
            run(["cargo", "build", "--release"] + shlex.split(args), logpath(f"build:{name}"),
                env={"CARGO_TARGET_DIR": os.path.join(GATE, "target", name)}, timeout=1500)
        kernel = re.search(r"-p (\S+)", args).group(1)
        arch = "aarch64" if A64 in args else "amd64"
        if not no_build:
            add(f"build:{name}", f"{kernel}/{arch}", "build", "build", mk, timeout=1600)
        built[name] = step_artifact(name, args)

    def need(step):
        return [] if no_build else [f"build:{step}"]

    # boots (amd64)
    j = add("boot:akuma-amd64-selftests-smp1", "akuma/amd64", "selftests", "boot", None,
            needs=need("akuma-amd64-headless") + ["prep:akuma-root"], timeout=300)
    j.fn = boot_selftests(j, lambda: built["akuma-amd64-headless"], 1)
    j = add("boot:akuma-amd64-selftests-smp2", "akuma/amd64", "selftests-smp2", "boot", None,
            needs=need("akuma-amd64-headless") + ["prep:akuma-root"], timeout=300)
    j.fn = boot_selftests(j, lambda: built["akuma-amd64-headless"], 2)
    j = add("boot:akuma-amd64-gateprobe", "akuma/amd64", "probe", "boot", None,
            needs=need("akuma-amd64-headless-no-tests") + ["prep:akuma-root"], timeout=300)
    j.fn = boot_akuma_gateprobe(j, lambda: built["akuma-amd64-headless-no-tests"])
    for k, step in (("taoxichuan", "taoxichuan-amd64"), ("shanghai", "shanghai-amd64"), ("denver", "denver-amd64")):
        j = add(f"boot:{k}-amd64-gateprobe", f"{k}/amd64", "probe", "boot", None,
                needs=need(step) + ["prep:gate-root"], timeout=300)
        j.fn = boot_gateprobe(j, (lambda s=step: built[s]), GATE_ROOT)
    if with_jvm:
        # The Denver JVM gate: Clojure on OpenJDK 21 answering HTTP (overlays/denver/check.sh). Needs the image
        # (overlays/denver/build-image.sh) and takes minutes, so it is opt-in: run it when a shared crate changed.
        def jvm():
            run(["sh", "overlays/denver/check.sh"], logpath("boot:denver-jvm"), env={"WAIT": "600"}, timeout=700)
        add("boot:denver-amd64-jvm", "denver/amd64", "jvm", "boot", jvm, needs=need("denver-amd64-jvm"), timeout=720)
    return jobs


# ---- scheduling and report --------------------------------------------------------------------------------

def load_xfail():
    xf = {}
    if os.path.exists(XFAIL_FILE):
        for line in open(XFAIL_FILE):
            line = line.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            parts = line.split("\t")
            if len(parts) < 3:
                raise SystemExit(f"{XFAIL_FILE}: need kernel<TAB>capability<TAB>reason: {line!r}")
            xf[(parts[0], parts[1])] = parts[2]
    return xf


def execute(jobs, slots):
    by = {j.name: j for j in jobs}
    sems = {p: threading.Semaphore(n) for p, n in slots.items()}
    lock = threading.Lock()
    done = {}

    def work(j):
        for n in j.needs:
            if by[n].status not in ("PASS", "XFAIL"):
                j.status, j.detail = "SKIP", f"needs {n} ({by[n].status})"
                return
        with sems[j.pool]:
            t0 = time.time()
            try:
                os.makedirs(LOGS, exist_ok=True)
                j.fn()
                j.status = "PASS"
            except Fail as e:
                j.status, j.detail = "FAIL", str(e)
            except Exception as e:  # a bug in the gate itself is a failure, with its type
                j.status, j.detail = "FAIL", f"{type(e).__name__}: {e}"
            j.secs = time.time() - t0
        print(f"  {j.status:5} {j.name} ({j.secs:.0f}s){'  ' + j.detail if j.status != 'PASS' else ''}", flush=True)

    # One thread per job; the semaphores bound what actually runs, the `needs` order is a wait on the other thread.
    events = {j.name: threading.Event() for j in jobs}

    def runner(j):
        for n in j.needs:
            events[n].wait()
        work(j)
        events[j.name].set()

    ts = [threading.Thread(target=runner, args=(j,), daemon=True) for j in jobs]
    for t in ts:
        t.start()
    for t in ts:
        t.join()


def select(jobs, only):
    if not only:
        return jobs
    words = only.split(",")
    by = {j.name: j for j in jobs}
    keep = set()

    def add(n):
        if n in keep:
            return
        keep.add(n)
        for m in by[n].needs:
            add(m)

    for j in jobs:
        if any(w in j.name or w in j.kernel for w in words):
            add(j.name)
    return [j for j in jobs if j.name in keep]


def report(jobs, xfail):
    # Apply the known-failure list to per-capability results, then to whole jobs.
    unexpected = []
    for j in jobs:
        if j.caps:
            for cap, (ok, d) in sorted(j.caps.items()):
                x = xfail.get((j.kernel, cap))
                if x and not ok:
                    j.caps[cap] = ("XFAIL", d)
                elif x and ok and x.startswith("flaky:"):
                    pass
                elif x and ok:
                    j.caps[cap] = ("XPASS", d)
                    unexpected.append(f"{j.kernel} {cap}: expected to fail ({x}) but passed - delete its line from scripts/gate/xfail.txt")
            if j.status == "FAIL" and all(v[0] in (True, "XFAIL") for v in j.caps.values()):
                j.status = "XFAIL"
        else:
            x = xfail.get((j.kernel, j.cap))
            if x and j.status == "FAIL":
                j.status = "XFAIL"
            elif x and j.status == "PASS":
                j.status = "XPASS"
                unexpected.append(f"{j.kernel} {j.cap}: expected to fail ({x}) but passed - delete its line from scripts/gate/xfail.txt")
    print("\n== jobs ==")
    for j in jobs:
        print(f"{j.status:6} {j.name:44} {j.secs:6.0f}s  {j.detail if j.status in ('FAIL', 'SKIP') else ''}")
    kernels = sorted({j.kernel for j in jobs if j.caps})
    caps = []
    for j in jobs:
        for c in j.caps:
            if c not in caps:
                caps.append(c)
    if kernels:
        print("\n== capabilities ==")
        w = max(len(k) for k in kernels) + 2
        print(" " * 24 + "".join(k.ljust(w) for k in kernels))
        for c in caps:
            row = c.ljust(24)
            for k in kernels:
                cell = "-"
                for j in jobs:
                    if j.kernel == k and c in j.caps:
                        ok = j.caps[c][0]
                        cell = "pass" if ok is True else "FAIL" if ok is False else ("xfail" if ok == "XFAIL" else "XPASS")
                row += cell.ljust(w)
            print(row)
    for c_j in jobs:
        for c, (ok, d) in c_j.caps.items():
            if ok is False:
                print(f"  FAIL {c_j.kernel} {c}: {d}")
    for u in unexpected:
        print("  XPASS:", u)
    bad = [j for j in jobs if j.status in ("FAIL", "SKIP", "XPASS")]
    return not bad and not unexpected


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--only", default="")
    ap.add_argument("--jobs", default="")
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--with-jvm", action="store_true", help="also run the Denver Clojure/JDK gate (needs its image; minutes)")
    a = ap.parse_args()
    os.makedirs(LOGS, exist_ok=True)
    cpus = os.cpu_count() or 4
    mem_gb = 8
    try:
        mem_gb = int(re.search(r"MemTotal:\s+(\d+)", open("/proc/meminfo").read()).group(1)) // 1048576
    except OSError:
        pass
    # A thin-LTO kernel link peaks near 1 GB: bound the build slots by RAM as well as cores.
    nb = max(1, min(cpus // 4, mem_gb // 3, 6))
    nboot = max(1, min(cpus // 2, 6))
    if a.jobs:
        nb, nboot = (int(x) for x in a.jobs.split(","))
    jobs = select(make_jobs(a.no_build, a.with_jvm), a.only)
    if a.list:
        for j in jobs:
            print(f"{j.name:44} {j.kernel:20} pool={j.pool:5} needs={','.join(j.needs) or '-'}")
        return 0
    if not shutil.which("qemu-system-x86_64") or not os.access("/dev/kvm", os.W_OK):
        print("note: no qemu-system-x86_64 or no writable /dev/kvm: boot jobs will fail", file=sys.stderr)
    xfail = load_xfail()
    if subprocess.run(["git", "config", "core.hooksPath"], capture_output=True, text=True, cwd=REPO).stdout.strip() != "scripts/git-hooks":
        print("warning: the pre-commit hook is not installed in this clone (scripts/install-hooks.sh)", file=sys.stderr)
    dirty = subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], capture_output=True, text=True,
                           cwd=REPO).stdout.strip().splitlines()
    head = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True, cwd=REPO).stdout.strip()
    print(f"gate: {head}{' (+%d modified files)' % len(dirty) if dirty else ''}, {len(jobs)} jobs, "
          f"{nb} build / {nboot} boot slots, logs in {LOGS}", flush=True)
    t0 = time.time()
    execute(jobs, {"build": nb, "boot": nboot, "light": cpus})
    ok = report(jobs, xfail)
    el = time.time() - t0
    print(f"\ngate: {'PASS' if ok else 'FAIL'} in {el / 60:.1f} min" + ("  (over the 15 min budget)" if el > 900 else ""))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
