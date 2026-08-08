#!/usr/bin/env python3
"""
-j4 self-host cargo-build campaign driver.

Reconstructs the methodology from docs/archive/PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md
and docs/archive/PAGE_TABLE_UAF_BKL_STORM.md: fresh VM boot per round, snapshot
disk (writes discarded), rm -rf target/aarch64-unknown-none, then an in-guest
`cargo build ... -j4` self-host build. Classifies each round as:

  GREEN         - build finished, exit 0
  EXIT=<n>      - the build process itself died with a signal/crash exit code
  BKL_STORM     - round timed out; console log has many `[BKL] stuck` lines
  SILENT_WEDGE  - round timed out; console log has ~0 `[BKL] stuck` lines
  OTHER_FAIL    - build exited nonzero for a reason that isn't 128+signal
  BOOT_FAIL     - SSH never came up within the boot budget

Console log (QEMU's own stdout, not ssh) is the source of truth for the
storm/wedge discriminator, matching the doc's own table: "storm = thousands
of stuck lines; silent wedge = 0-1". Requires a prepared `disk_selfhost.img`
with `/root/akuma` already cloned and its `Cargo.toml` patched for the
stable-cargo/nightly-rustc split (see scripts/run_selfhost_kernelbuild.py's
"manifest" step) — this driver only clears `target/`, it does not stage the
disk from scratch.

Each round auto-probes a live storm/wedge via lockprobe.py (through the
gdbstub QEMU always opens for this driver) BEFORE tearing the VM down, so a
lockprobe capture of the actual stuck state is available for every such
round, not just ones you happen to catch manually in time.

Usage:
    scripts/j4_selfhost_campaign.py --lanes 2 --rounds-per-lane 15 --budget 1200
    scripts/j4_selfhost_campaign.py --lanes 2 --rounds-per-lane 15 --start-rounds 9,34  # resume
"""
import subprocess, sys, time, re, os, json, signal

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ELF = "target/aarch64-unknown-none/release-smp-shared/akuma"
DISK = "disk_selfhost.img"
DEFAULT_OUTDIR = os.path.join(REPO, "logs", "j4_campaign")

ENVP = ("/bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root "
        "CARGO_HOME=/root/.cargo RUSTC=/usr/local/bin/rustc "
        "CARGO_BUILD_TARGET=aarch64-unknown-none "
        "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/root/akuma/linker.ld ")

BUILD_CMD = (ENVP + "/usr/bin/cargo build -p akuma --profile release-smp-shared "
             "--features devbox-smoltcp,no-tests --manifest-path /root/akuma/Cargo.toml -j4")


def ssh_port(instance):
    return 2222 + 100 * instance


def ssh_cmd(instance, remote_cmd, timeout=25):
    port = ssh_port(instance)
    base = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=0",
            "-p", str(port), "root@localhost", remote_cmd]
    try:
        r = subprocess.run(base, capture_output=True, text=True, timeout=timeout)
        return r.returncode, re.sub(r'\x1b\[[0-9;]*[A-Za-z]', '', r.stdout), r.stderr
    except subprocess.TimeoutExpired:
        return None, "", "<ssh-timeout>"


def wait_for_ssh(instance, budget=90):
    t0 = time.time()
    while time.time() - t0 < budget:
        rc, out, err = ssh_cmd(instance, "echo READY", timeout=8)
        if rc == 0 and "READY" in out:
            return True
        time.sleep(3)
    return False


def boot_vm(instance, log_path):
    env = dict(os.environ)
    env.update({
        "INSTANCE": str(instance),
        "DISK": DISK,
        "MEMORY": "14336",
        "SMP": "4",
        "SNAPSHOT": "1",
        "GDB": "1",  # gdbstub on 1234+instance; QEMU-side only, doesn't touch the guest binary/timing
    })
    logf = open(log_path, "w")
    p = subprocess.Popen(["scripts/cargo_runner.sh", ELF], cwd=REPO, env=env,
                          stdout=logf, stderr=subprocess.STDOUT)
    return p, logf


def gdb_port(instance):
    return 1234 + instance


def run_lockprobe(instance, out_path, samples=3, interval=5.0):
    port = gdb_port(instance)
    try:
        r = subprocess.run(
            ["scripts/lockprobe.py", str(port), "-n", str(samples), "-i", str(interval), "-o", out_path],
            cwd=REPO, capture_output=True, text=True, timeout=samples * interval + 30)
        return r.returncode, r.stdout, r.stderr
    except Exception as e:
        return None, "", str(e)


def count_bkl_stuck(log_path):
    try:
        with open(log_path, "rb") as f:
            data = f.read()
        return data.count(b"[BKL] stuck")
    except FileNotFoundError:
        return 0


def log_growth_since(log_path, prev_size):
    try:
        return os.path.getsize(log_path)
    except FileNotFoundError:
        return prev_size


def run_round(instance, round_id, outdir, budget_s=1200, poll_interval=20):
    log_path = os.path.join(outdir, f"console_lane{instance}_round{round_id}.log")
    t_start = time.time()
    p, logf = boot_vm(instance, log_path)
    result = {"round": round_id, "instance": instance, "t_start": t_start}
    try:
        if not wait_for_ssh(instance, budget=120):
            result.update(outcome="BOOT_FAIL", elapsed=time.time() - t_start,
                           bkl_stuck=count_bkl_stuck(log_path))
            return result

        # Force a full rebuild every round.
        ssh_cmd(instance, "/bin/busybox rm -rf /root/akuma/target/aarch64-unknown-none", timeout=60)
        ssh_cmd(instance, "/bin/busybox rm -f /tmp/build.out /tmp/build.rc", timeout=15)

        wrapper = f"{BUILD_CMD} > /tmp/build.out 2>&1; echo $? > /tmp/build.rc"
        launch_cmd = f'/bin/busybox nohup /bin/busybox sh -c {json.dumps(wrapper)} >/dev/null 2>&1 &'
        # ssh needs a single shell string; nohup already backgrounds inside the guest,
        # so this ssh call itself should return quickly.
        ssh_cmd(instance, launch_cmd, timeout=15)

        rc = None
        last_size = 0
        stall_since = None
        probed = False
        probe_info = None
        STORM_PROBE_THRESHOLD = 500   # a healthy round's transient contention tops out ~60
        STALL_PROBE_S = 90            # console silence this long, while unfinished, is wedge-shaped
        while time.time() - t_start < budget_s:
            time.sleep(poll_interval)
            code, out, err = ssh_cmd(instance, "/bin/busybox cat /tmp/build.rc 2>/dev/null", timeout=15)
            if code == 0 and out.strip():
                try:
                    rc = int(out.strip())
                    break
                except ValueError:
                    pass
            # Track console-log growth as an independent hang signal (ssh can hang too).
            size = log_growth_since(log_path, last_size)
            if size == last_size:
                if stall_since is None:
                    stall_since = time.time()
            else:
                stall_since = None
            last_size = size

            if not probed:
                bkl_now = count_bkl_stuck(log_path)
                stalled_for = (time.time() - stall_since) if stall_since else 0
                if bkl_now > STORM_PROBE_THRESHOLD or stalled_for > STALL_PROBE_S:
                    probe_path = os.path.join(outdir, f"lockprobe_lane{instance}_round{round_id}.txt")
                    pr_rc, pr_out, pr_err = run_lockprobe(instance, probe_path)
                    probed = True
                    probe_info = {"trigger": "bkl_storm" if bkl_now > STORM_PROBE_THRESHOLD else "console_stall",
                                   "bkl_at_probe": bkl_now, "stalled_for_s": stalled_for,
                                   "probe_rc": pr_rc, "probe_path": probe_path,
                                   "probe_stderr": pr_err[-2000:] if pr_err else ""}

        elapsed = time.time() - t_start
        bkl_lines = count_bkl_stuck(log_path)
        if probe_info:
            result["probe"] = probe_info

        if rc is not None:
            if rc == 0:
                result.update(outcome="GREEN")
            elif rc >= 128:
                sig = rc - 128
                result.update(outcome=f"EXIT={rc}", signal=sig)
            else:
                result.update(outcome="OTHER_FAIL", rc=rc)
        else:
            if bkl_lines > 20:
                result.update(outcome="BKL_STORM")
            else:
                result.update(outcome="SILENT_WEDGE")

        result.update(elapsed=elapsed, bkl_stuck=bkl_lines,
                       console_stalled_s=(time.time() - stall_since) if stall_since else 0)
        return result
    finally:
        try:
            p.send_signal(signal.SIGTERM)
            p.wait(timeout=10)
        except Exception:
            try:
                p.kill()
            except Exception:
                pass
        logf.close()


def append_result(results_path, r):
    with open(results_path, "a") as f:
        f.write(json.dumps(r) + "\n")
    print(f"[round {r['round']} lane {r['instance']}] {r['outcome']} "
          f"elapsed={r.get('elapsed', 0):.0f}s bkl_stuck={r.get('bkl_stuck', 0)}", flush=True)


def lane_worker(instance, n_rounds, start_round, budget_s, outdir, results_path):
    for i in range(n_rounds):
        round_id = start_round + i
        r = run_round(instance, round_id, outdir, budget_s=budget_s)
        append_result(results_path, r)


if __name__ == "__main__":
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--lanes", type=int, default=2)
    ap.add_argument("--rounds-per-lane", type=int, default=6)
    ap.add_argument("--budget", type=int, default=1200)
    ap.add_argument("--start-instance", type=int, default=1)
    ap.add_argument("--outdir", default=DEFAULT_OUTDIR,
                     help="where console logs, lockprobe captures, and results.jsonl go")
    ap.add_argument("--start-rounds", default="",
                     help="comma-separated per-lane starting round id, overrides the default "
                          "contiguous numbering (lane*rounds_per_lane+1) — use to resume a campaign")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    results_path = os.path.join(args.outdir, "results.jsonl")
    starts = [int(x) for x in args.start_rounds.split(",")] if args.start_rounds else None
    import threading
    threads = []
    for lane in range(args.lanes):
        instance = args.start_instance + lane
        start_round = starts[lane] if starts else lane * args.rounds_per_lane + 1
        t = threading.Thread(target=lane_worker,
                              args=(instance, args.rounds_per_lane, start_round, args.budget,
                                    args.outdir, results_path))
        t.start()
        threads.append(t)
    for t in threads:
        t.join()
    print("CAMPAIGN DONE", flush=True)
