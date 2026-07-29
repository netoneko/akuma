#!/usr/bin/env python3
"""Stage the regimen into the VM, run it detached, and poll it to completion.

Detached (`nohup sh ... &`) because the devbox sshd tears down exec channels when
the guest's cores are pegged; the job must outlive its channel. Progress is read
back over short-lived connections — and the poll interval is deliberately long,
because every poll is an ssh session, i.e. more process churn on a kernel whose
thread-slot reclamation already starves under load (doc §11.4).
"""
import sys
import time

from vm import run

JOB_URL = "http://10.0.2.2:8899/job.sh"
POLL_SECONDS = 45


def main():
    budget = int(sys.argv[1]) if len(sys.argv) > 1 else 1800
    out, err = run(f'sh -c "rm -f /tmp/bkl.done /tmp/job.log; curl -s -o /tmp/job.sh {JOB_URL}; '
                   'chmod +x /tmp/job.sh; wc -c /tmp/job.sh"', timeout=60)
    print("staged:", out.strip(), err.strip())
    t0 = time.time()
    print(f"host_start_epoch={t0:.0f}")
    # `sh /tmp/job.sh`, not the shebang: busybox resolves a bare path with no
    # recognised interpreter as an applet name and fails with "applet not found".
    run('sh -c "nohup sh /tmp/job.sh > /tmp/job.log 2>&1 &"', timeout=60)

    seen = 0
    while time.time() - t0 < budget:
        time.sleep(POLL_SECONDS)
        try:
            out, _ = run("cat /tmp/job.log", timeout=90)
        except Exception as e:  # timeout while the cores are pegged — just retry
            print(f"[{time.time()-t0:6.0f}s] poll failed: {type(e).__name__}")
            continue
        lines = out.splitlines()
        # A truncated read can make the log look shorter than last time; never
        # rewind, or completed phases get reprinted.
        for ln in lines[seen:]:
            print(f"[{time.time()-t0:6.0f}s] {ln}", flush=True)
        seen = max(seen, len(lines))
        if any("REGIMEN DONE" in ln for ln in lines):
            print(f"COMPLETED in {time.time()-t0:.0f}s")
            out, _ = run("cat /tmp/bkl/digests.txt", timeout=90)
            print("digests:\n" + out)
            return 0
    print("BUDGET EXHAUSTED — regimen did not finish")
    return 1


if __name__ == "__main__":
    sys.exit(main())
