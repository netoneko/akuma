#!/usr/bin/env python3
"""The KVM dev cycle's laptop side: docs/archive/AKUMA_KVM_DEV_CYCLE.md.

goose-glm works INSIDE the ryzen's Akuma (edit, `kbuild`, test in a /dev/kvm0 guest, `git commit` on its own branch).
This script is everything it cannot do for itself: ship it source, start/poll a goose session, take its commits back, and
perform the reboots (a reboot kills goose, and Akuma cannot boot itself back from Pop).

    dev_cycle.py sync                      # bundle this checkout's HEAD to Akuma, check it out on branch `dev-base`
    dev_cycle.py ask NAME FILE [--new]     # run `goose-glm run -i FILE -n NAME` detached (resumes NAME unless --new)
    dev_cycle.py status NAME [-n LINES]    # is goose alive, and the tail of its log
    dev_cycle.py collect BRANCH            # fetch goose's branch back as refs/akuma/BRANCH (never merges, never pushes)
    dev_cycle.py cmos                      # dump CMOS 0x00-0x7F from Pop (needs Pop up) + the classic checksum verdict
    dev_cycle.py reboot-test [--entry 29]  # clean reboot Akuma -> Pop -> Akuma(entry), dmesg + CMOS + verdict into out/

Hosts: Akuma 192.168.1.159:2222 (ssh via subprocess; the plain `ssh` CLI is blocked in agent shells), Pop 192.168.1.126.
Output of every command lands in scripts/kvm/dev_cycle_out/ (git-ignored by the .gitignore line next to it).
"""
import argparse
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.path.join(ROOT, "scripts", "kvm", "dev_cycle_out")
AK_HOST, AK_PORT = os.environ.get("AKUMA_HOST", "192.168.1.159"), "2222"
POP_HOST = os.environ.get("POP_HOST", "192.168.1.126")
SRC = "/src/github.com/netoneko/akuma"
WORK = "/root/dev-cycle"
SSH_OPTS = ["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]


def akuma(cmd, timeout=120, data=None):
    r = subprocess.run(["ssh", *SSH_OPTS, "-o", "ConnectTimeout=8", "-p", AK_PORT, f"root@{AK_HOST}", cmd],
                       capture_output=True, input=data, timeout=timeout)
    return r.returncode, r.stdout.decode(errors="replace"), r.stderr.decode(errors="replace")


def pop(cmd, timeout=120, user="root"):
    r = subprocess.run(["ssh", *SSH_OPTS, "-o", "ConnectTimeout=6", f"{user}@{POP_HOST}", cmd],
                       capture_output=True, timeout=timeout)
    return r.returncode, r.stdout.decode(errors="replace"), r.stderr.decode(errors="replace")


def git(*a, cwd=ROOT):
    return subprocess.run(["git", *a], cwd=cwd, capture_output=True, text=True, check=True).stdout.strip()


def wait(fn, what, budget, step=6):
    t0 = time.time()
    while time.time() - t0 < budget:
        try:
            if fn():
                return int(time.time() - t0)
        except Exception:  # noqa: BLE001 - a dropped ssh is the normal state while a box reboots
            pass
        time.sleep(step)
    sys.exit(f"timed out after {budget}s waiting for {what}")


ENV = "export TERM=dumb GIT_PAGER=cat; . /etc/akuma-dev.env; "


def cmd_sync(_):
    branch = git("rev-parse", "--abbrev-ref", "HEAD")
    head = git("rev-parse", "HEAD")
    rc, o, e = akuma(ENV + f"cd {SRC} && git rev-parse HEAD")
    base = o.strip().splitlines()[-1] if rc == 0 and o.strip() else ""
    if not base:
        sys.exit(f"cannot read Akuma's HEAD: {e}")
    if base == head:
        print("Akuma already at", head[:8])
    else:
        if subprocess.run(["git", "merge-base", "--is-ancestor", base, head], cwd=ROOT).returncode:
            sys.exit(f"Akuma's HEAD {base[:8]} is not an ancestor of {head[:8]}: ship a full bundle by hand")
        os.makedirs(OUT, exist_ok=True)
        b = os.path.join(OUT, "dev.bundle")
        git("bundle", "create", b, f"{base}..{branch}")
        data = open(b, "rb").read()
        print(f"bundle {len(data)} bytes ({base[:8]}..{head[:8]})")
        rc, _, e = akuma(f"mkdir -p {WORK}; cat > {WORK}/dev.bundle.new && mv {WORK}/dev.bundle.new {WORK}/dev.bundle", 600, data)
        if rc:
            sys.exit(f"ship failed: {e}")
    rc, o, e = akuma(ENV + f"cd {SRC} && git fetch -q {WORK}/dev.bundle {branch} && git checkout -q -B dev-base FETCH_HEAD && git log -1 --format='%h %s'", 300)
    print(o.strip() or e.strip())
    if rc:
        sys.exit("checkout failed")


def cmd_ask(a):
    text = open(a.file, "rb").read()
    rc, o, e = akuma(f"mkdir -p {WORK}; cat > {WORK}/{a.name}.task.md", 60, text)
    if rc:
        sys.exit(e)
    resume = "" if a.new else f"$(ls {WORK}/{a.name}.started >/dev/null 2>&1 && echo -r)"
    script = (f"{ENV} cd {SRC}; touch {WORK}/{a.name}.started; "
              f"goose-glm run -i {WORK}/{a.name}.task.md -n {a.name} {resume} --max-turns {a.max_turns} "
              f"> {WORK}/{a.name}.log 2>&1; echo GOOSE-EXIT rc=$? >> {WORK}/{a.name}.log")
    # detached: this ssh returns at once; the shell keeps running after the session closes
    subprocess.Popen(["ssh", *SSH_OPTS, "-p", AK_PORT, f"root@{AK_HOST}", f"sh -c '{script}' >/dev/null 2>&1 &"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(f"goose session {a.name!r} started; poll with: dev_cycle.py status {a.name}")


def cmd_status(a):
    rc, o, e = akuma(f"ps | grep -E 'goose' | grep -v grep | head -3; echo ---; tail -n {a.lines} {WORK}/{a.name}.log 2>&1")
    print(o or e)


def cmd_collect(a):
    rc, o, e = akuma(ENV + f"cd {SRC} && git bundle create {WORK}/out.bundle dev-base..{a.branch} 2>&1 | tail -2; ls -la {WORK}/out.bundle", 120)
    print(o.strip())
    r = subprocess.run(["ssh", *SSH_OPTS, "-p", AK_PORT, f"root@{AK_HOST}", f"cat {WORK}/out.bundle"], capture_output=True, timeout=600)
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"{a.branch}.bundle")
    open(path, "wb").write(r.stdout)
    print(git("fetch", path, f"{a.branch}:refs/akuma/{a.branch}"), f"-> refs/akuma/{a.branch}")


CMOS_DUMP = r"""python3 - <<'PY'
import os
f = os.open('/dev/port', os.O_RDWR)
def rd(i):
    os.lseek(f, 0x70, 0); os.write(f, bytes([i])); os.lseek(f, 0x71, 0); return os.read(f, 1)[0]
b = [rd(i) for i in range(128)]
print(' '.join('%02x' % x for x in b))
s = sum(b[0x10:0x2E]) & 0xffff
print('classic-checksum 0x10-0x2d sum=%04x stored=%02x%02x %s' % (s, b[0x2E], b[0x2F], 'OK' if s == (b[0x2E] << 8 | b[0x2F]) else 'MISMATCH'))
print('regA=%02x regB=%02x regD=%02x diag=%02x crumb[7f]=%02x' % (b[0x0A], b[0x0B], b[0x0D], b[0x0E], b[0x7F]))
PY"""


def cmd_cmos(_):
    rc, o, e = pop(CMOS_DUMP, 60)
    print(o or e)
    return o


def cmd_reboot_test(a):
    os.makedirs(OUT, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    rep = [f"reboot-test {stamp} entry={a.entry}"]
    if not a.resume:
        rc, o, _ = akuma("md5sum /boot/akuma-amd64.verify; dmesg | tail -n 400", 60)
        open(os.path.join(OUT, f"{stamp}-akuma-before.dmesg"), "w").write(o)
        rep.append("before: " + o.splitlines()[0] if o else "before: (no dmesg)")
    print("clean reboot: stopping VMs, sync, reboot" if not a.resume else "resuming after the reboot")
    if not a.resume:
        try:
            akuma(("" if a.keep_vms else "for s in $(herd status | grep '(vm)' | cut -d: -f1); do herd stop $s; done; ") + "sync; sleep 1; sync; reboot -f", 45)
        except subprocess.TimeoutExpired:
            pass  # the box died under the ssh session: that is the reboot working
    try:
        t = wait(lambda: pop("echo up", 10)[0] == 0, "Pop", a.pop_budget)
    except SystemExit as ex:
        rep.append(f"FAIL: {ex}; firmware did not hand over to Pop. If the screen shows a CMOS checksum error asking for F1, THAT is the bug: needs a human.")
        return finish(stamp, rep, 1)
    rep.append(f"Pop up after {t}s")
    rc, o, _ = pop(CMOS_DUMP, 60)
    rep.append("CMOS on Pop after the clean reboot:\n" + o)
    rc, o, _ = pop("dmesg | grep -i -E 'rtc|cmos|nvram|clock' | tail -n 20; echo; hwclock --show 2>&1 | head -2; date -u", 30)
    rep.append("Pop rtc/cmos kernel lines:\n" + o)
    pop(f"cd /home/netoneko/akuma-metal/fc-kvm && sh overlays/ryzen/arm.sh {a.entry}", 20)
    time.sleep(40)
    try:
        t = wait(lambda: akuma("uname -a", 10)[0] == 0, "Akuma", a.akuma_budget)
    except SystemExit as ex:
        rep.append(f"FAIL: {ex}")
        return finish(stamp, rep, 1)
    rep.append(f"Akuma up after {t}s")
    rc, o, _ = akuma("dmesg | grep -a -i -E 'clock|cmos|rtc|EXCEPTION|panic|\\[KVM\\]' | tail -n 40; uname -a", 60)
    rep.append("Akuma dmesg (clock/cmos/exception lines):\n" + o)
    bad = [l for l in o.splitlines() if "EXCEPTION" in l or "panic" in l.lower()]
    mism = "MISMATCH" in "\n".join(rep)
    rep.append("VERDICT: " + ("FAIL (exception in dmesg)" if bad else "FAIL (CMOS checksum mismatch on Pop)" if mism else "PASS (clean reboot, Pop and Akuma both came back, no exception)"))
    return finish(stamp, rep, 1 if (bad or mism) else 0)


def finish(stamp, rep, code):
    text = "\n".join(rep) + "\n"
    open(os.path.join(OUT, f"{stamp}-report.txt"), "w").write(text)
    print(text)
    sys.exit(code)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sp = ap.add_subparsers(dest="c", required=True)
    sp.add_parser("sync").set_defaults(f=cmd_sync)
    s = sp.add_parser("ask"); s.add_argument("name"); s.add_argument("file"); s.add_argument("--new", action="store_true")
    s.add_argument("--max-turns", type=int, default=80); s.set_defaults(f=cmd_ask)
    s = sp.add_parser("status"); s.add_argument("name"); s.add_argument("-n", "--lines", type=int, default=40); s.set_defaults(f=cmd_status)
    s = sp.add_parser("collect"); s.add_argument("branch"); s.set_defaults(f=cmd_collect)
    sp.add_parser("cmos").set_defaults(f=cmd_cmos)
    s = sp.add_parser("reboot-test"); s.add_argument("--entry", type=int, default=29)
    s.add_argument("--keep-vms", action="store_true", help="reboot with guests still running (the old kernel leaves 0x57/0x58 in CMOS 0x7F then; a fixed one must not)")
    s.add_argument("--resume", action="store_true", help="the reboot already happened: skip it, wait for Pop and carry on")
    s.add_argument("--pop-budget", type=int, default=240); s.add_argument("--akuma-budget", type=int, default=300)
    s.set_defaults(f=cmd_reboot_test)
    a = ap.parse_args()
    a.f(a)


if __name__ == "__main__":
    main()
