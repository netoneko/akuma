#!/usr/bin/env python3
"""One trial of "can a local model follow install.txt and boot Akuma?" - see proposals/INSTALL_SCRIPT_QWEN_EXPERIMENT.md.

    python3 -I scripts/kvm/goose/qwen_install_trial.py --label qwen3-8b --host http://192.168.1.120:8081 \
        --model qwen3-8b [--keep-build] [--max-turns 40] [--budget 5400]

Runs on the Mac. Everything else happens on Pop (the ryzen, root over hpbox, the model's commands as `netoneko`):
  1. reset  : stop whatever VM the last trial left (its recorded pid only), wipe the run folder
              (--keep-build keeps the clone and the build for a faster DEVELOPMENT trial; a scored trial never does)
  2. ship   : scripts/install.sh, scripts/install/lock.txt, bootstrap/public/install.txt -> /home/netoneko/demo
  3. run    : `goose run` as netoneko against the model's OpenAI-compatible endpoint, detached, with a wall budget
  4. judge  : the HARNESS decides, not the model: a live VM, `install.sh ssh 'busybox uname -a'` returning the
              expected line, no sudo / package manager in the transcript.
Writes /home/netoneko/demo/trials/<label>-<n>/{transcript.txt,result.json} on Pop and prints result.json.
No model-facing logic lives here.
"""
import argparse, json, pathlib, sys, time

REPO = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "scripts/utils"))
import hpbox  # noqa: E402

DEMO = "/home/netoneko/demo"
RUN = f"{DEMO}/run"
SH = f"{DEMO}/scripts/install.sh"
ENVP = "export PATH=/home/netoneko/.cargo/bin:/home/netoneko/bin:/usr/local/bin:$PATH"

TASK = """Your task brief is the file /home/netoneko/demo/install.txt. Read it completely, then carry it out.
Run every command with your shell tool. Do not ask the user questions; the user is not there."""


def root(cmd, inp=None, timeout=300):
    rc, o, e = hpbox.ryzen_root(cmd, timeout=timeout, input=inp)
    return rc, o, e


def as_user(cmd, timeout=300):
    # login shell for a clean env; the command is passed on stdin so quoting never bites
    rc, o, e = root("su - netoneko -c 'sh -s'", inp=f"{ENVP}\n{cmd}\n", timeout=timeout)
    return rc, o, e


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--host", required=True, help="OpenAI-compatible base, e.g. http://192.168.1.120:8081")
    ap.add_argument("--model", required=True)
    ap.add_argument("--api-key", default="none")
    ap.add_argument("--provider", default="openai", choices=("openai", "ollama"),
                    help="goose provider: openai (llama-server) or ollama (native; goose's openai client cannot decode ollama's stream)")
    ap.add_argument("--keep-build", action="store_true", help="DEVELOPMENT only: keep clone+build, redo image+boot")
    ap.add_argument("--max-turns", type=int, default=40)
    ap.add_argument("--budget", type=int, default=5400, help="wall seconds")
    ap.add_argument("--temperature", default="0.7")
    a = ap.parse_args()

    # reset -----------------------------------------------------------------------------------------------
    as_user(f"sh {SH} down --dir {RUN} >/dev/null 2>&1; true")
    if a.keep_build:
        as_user(f"rm -f {RUN}/state/boot.stamp {RUN}/state/wait.stamp {RUN}/state/image.stamp {RUN}/state/disk-run.img "
                f"{RUN}/state/disk.img {RUN}/state/vmm; rm -f {DEMO}/up.log")
    else:
        as_user(f"rm -rf {RUN} {DEMO}/up.log")
    # /dev/kvm: udev can reset an ACL granted by hand. The harness (root, never the model) re-grants it.
    root("setfacl -m u:netoneko:rw /dev/kvm")
    # the tap is the one thing a human administrator sets up (the script never uses sudo)
    rc, o, _ = root("ip link show akuma-tap-2222 >/dev/null 2>&1 && echo tap_ok")
    if "tap_ok" not in o:
        sys.exit("harness: tap akuma-tap-2222 missing - an administrator must create it (see install.sh boot error)")

    # ship ------------------------------------------------------------------------------------------------
    root(f"mkdir -p {DEMO}/scripts/install {DEMO}/trials && chown -R netoneko:netoneko {DEMO}")
    for rel, dst in (("scripts/install.sh", f"{DEMO}/scripts/install.sh"),
                     ("scripts/install/lock.txt", f"{DEMO}/scripts/install/lock.txt"),
                     ("bootstrap/public/install.txt", f"{DEMO}/install.txt")):
        root(f"cat > {dst}", inp=(REPO / rel).read_text())
    root(f"cat > {DEMO}/TASK.txt", inp=TASK)
    root(f"chown -R netoneko:netoneko {DEMO}; chmod +x {SH}")

    _, o, _ = as_user(f"sh {SH} check --vmm firecracker --dir {RUN} > {DEMO}/preflight.txt 2>&1; echo $?; tail -4 {DEMO}/preflight.txt")
    if o.strip().splitlines()[0:1] != ["0"]:
        sys.exit("harness: host preflight failed, trial NOT started:\n" + o)
    rc, o, _ = as_user(f"ls {DEMO}/trials | grep -c '^{a.label}-'")
    n = int(o.strip() or 0) + 1
    tdir = f"{DEMO}/trials/{a.label}-{n}"
    as_user(f"mkdir -p {tdir}")

    # run -------------------------------------------------------------------------------------------------
    if a.provider == "ollama":
        prov = f"GOOSE_PROVIDER=ollama OLLAMA_HOST={a.host}"
    else:
        prov = (f"GOOSE_PROVIDER=openai OPENAI_HOST={a.host} OPENAI_BASE_PATH=v1/chat/completions "
                f"OPENAI_API_KEY={a.api_key}")
    env = (f"PATH=/home/netoneko/.cargo/bin:/home/netoneko/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin "
           f"XDG_CONFIG_HOME={tdir}/cfg GOOSE_DISABLE_KEYRING=1 GOOSE_MODE=auto {prov} "
           f"GOOSE_MODEL={a.model} GOOSE_TEMPERATURE={a.temperature} GOOSE_TELEMETRY_ENABLED=false")
    t0 = time.time()
    launcher = (f"cd {DEMO} && date +%s > {tdir}/start && "
                f"(setsid nohup sh -c 'env {env} timeout {a.budget} goose run -i {DEMO}/TASK.txt --no-session "
                f"--max-turns {a.max_turns} > {tdir}/transcript.txt 2>&1; echo $? > {tdir}/goose.rc; date +%s > {tdir}/end' "
                f"> /dev/null 2>&1 < /dev/null &)")
    as_user(launcher)
    print(f"trial {a.label}-{n} started; budget {a.budget}s, {a.max_turns} turns", flush=True)

    while True:
        time.sleep(60)
        rc, o, _ = as_user(f"test -f {tdir}/end && echo ENDED; tail -c 200 {DEMO}/up.log 2>/dev/null | tail -2")
        print(time.strftime("%H:%M:%S"), o.strip().replace("\n", " | ")[-180:], flush=True)
        if "ENDED" in o or time.time() - t0 > a.budget + 300:
            break

    # judge -----------------------------------------------------------------------------------------------
    _, st, _ = as_user(f"sh {SH} status --dir {RUN} 2>&1; echo status_rc=$?")
    _, ss, _ = as_user(f"sh {SH} ssh --dir {RUN} 'busybox uname -a' 2>&1 | tail -1")
    _, tr, _ = as_user(f"cat {tdir}/transcript.txt")
    low = tr.lower()
    forbidden = [w for w in ("sudo ", "apt ", "apt-get", "pip install", "pkill ", "killall") if w in low]
    ready = "AKUMA_READY=1" in tr
    ok_vm = "status_rc=0" in st
    ok_ssh = "Linux taoxichuan" in ss
    asked = any(w in low for w in ("which package manager", "please let me know", "would you like", "do you want me"))
    claimed = "DONE" in tr
    res = {
        "trial": f"{a.label}-{n}", "model": a.model, "keep_build": a.keep_build,
        "harness_vm_alive": ok_vm, "harness_ssh_ok": ok_ssh, "ssh_line": ss.strip()[-120:],
        "transcript_saw_ready": ready, "model_claimed_done": claimed, "asked_user": asked,
        "forbidden": forbidden,
        "PASS": bool(ok_vm and ok_ssh and not forbidden),
        "false_success": bool(claimed and not (ok_vm and ok_ssh)),
        "wall_s": int(time.time() - t0),
    }
    as_user(f"cat > {tdir}/result.json <<'EOF'\n{json.dumps(res, indent=1)}\nEOF")
    print(json.dumps(res, indent=1))
    return 0 if res["PASS"] else 1


if __name__ == "__main__":
    sys.exit(main())
