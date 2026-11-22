#!/usr/bin/env python3
"""Stage 3 trial: three Codex agents write the demo services and their tests, in boxes on two guests.

    overlays/ecosystem-demo/agents.py --ref ../akuma-demo-reference-impl

A (this script, the coordinator) boots both guests, hands each agent ITS OWN brief through the metadata
service, installs Codex in the agent's box, and runs the agents:

    B  rust box,   guest 1, gpt-6.1-terra   writes the Git service      (/work/run.sh, PORT, GIT_ROOT)
    C  scala box,  guest 1, glm-5.3-flash   writes the task board        (/work/run.sh, PORT)
    D  python box, guest 2, glm-5.3-flash   writes + runs the acceptance tests against both, over HTTP

B and C run concurrently. When both are done A starts their services from their run.sh, checks them from
guest 2, then starts D with the actual URLs. Last, A runs the REFERENCE acceptance suite against the agents'
services (the oracle) and saves everything to $ECO_WORK/agent-runs/<stamp>/.

Credentials: the Z.ai key is a per-agent secret (metadata service -> root-only file -> env of that box only);
Codex's ChatGPT login for B travels the same way as a base64 secret. Neither is in an image or a log.
"""
import argparse
import base64
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ecolib as E
import launch
from verify import bx

HERE = Path(__file__).resolve().parent
BIN = Path.home() / ".local/bin"
CODEX_HOME = Path.home() / ".codex"
ZAI_BASE = "https://api.z.ai/api/v1"

# agent name -> (guest, box/image, model family, section(s) of the contract it is given, brief file)
AGENTS = {
    "rust": dict(guest=1, family="openai", model="gpt-6.1-terra", sections=("Git service",), brief="rust.md"),
    "scala": dict(guest=1, family="zai", model="glm-5.3-flash", sections=("Task board",), brief="scala.md"),
    "python": dict(guest=2, family="zai", model="glm-5.3-flash", sections=("Task board", "Git service"), brief="python.md"),
}


def section(spec: str, title: str) -> str:
    out, on = [], False
    for line in spec.splitlines():
        if line.startswith("## "):
            on = line[3:].startswith(title)
            continue
        if on:
            out.append(line)
    return "\n".join(out).strip() + "\n"


def brief_for(name: str, spec: str) -> str:
    a = AGENTS[name]
    return (HERE / "briefs" / a["brief"]).read_text() + "\n".join(section(spec, s) for s in a["sections"])


def config_for(a) -> str:
    base = ('sandbox_mode = "danger-full-access"\napproval_policy = "never"\n'
            f'model = "{a["model"]}"\n[features]\ncode_mode_host = true\n'
            '[projects."/work"]\ntrust_level = "trusted"\n')
    if a["family"] == "zai":
        base = ('model_provider = "zai"\nmodel_reasoning_effort = "low"\n' + base +
                f'[model_providers.zai]\nname = "Z.ai"\nbase_url = "{ZAI_BASE}"\nenv_key = "ZAI_API_KEY"\n'
                'wire_api = "responses"\nsupports_websockets = false\n')
    else:
        base = 'model_reasoning_effort = "medium"\n' + base
    return base


def put(g, data: bytes, dest: str, mode=None):
    cmd = f"mkdir -p $(dirname {dest}) && cat > {dest}" + (f" && chmod {mode} {dest}" if mode else "")
    r = subprocess.run(g.ssh_argv(cmd), input=data, capture_output=True, timeout=600)
    if r.returncode:
        raise RuntimeError(f"put {dest}: {r.stderr.decode()[-300:]}")


def install_agent(g, name: str, a):
    """Create the box, put Codex, its config and the brief in it (the brief comes from the metadata-delivered file)."""
    rc, o = g.run(f"/sbin/eco-box exec {name} {name} true")
    if rc:
        raise RuntimeError(f"create box {name}: {o}")
    up = f"/var/lib/box/containers/{name}/upper"
    put(g, (BIN / "codex").read_bytes(), f"{up}/usr/local/bin/codex", "755")
    put(g, (BIN / "codex-code-mode-host").read_bytes(), f"{up}/usr/local/bin/codex-code-mode-host", "755")
    mc = CODEX_HOME / "models_cache.json"
    if mc.exists():
        put(g, mc.read_bytes(), f"{up}/root/.codex/models_cache.json")
    put(g, config_for(a).encode(), f"{up}/root/.codex/config.toml")
    g.run(f"cp /var/lib/agents/{name}/prompts/task.md {up}/root/task.md", check=True)


def agent_cmd(name: str, a) -> str:
    pre = ""
    if a["family"] == "openai":
        pre = "mkdir -p /root/.codex && echo \"$CODEX_AUTH_B64\" | base64 -d > /root/.codex/auth.json && chmod 600 /root/.codex/auth.json && "
    env = ""
    if name == "python":
        env = "export BOARD_URL=\"$BOARD_URL\" GIT_URL=\"$GIT_URL\"; "
    prompt = ("Read /root/task.md. It is your complete brief: do the task it describes, in /work, and follow its delivery rules. "
              "You have no other instructions and no one to ask.")
    return (f"export HOME=/root PATH=/usr/local/bin:$PATH; {pre}{env}cd /work && "
            f"codex --no-daemon exec --json --skip-git-repo-check -C /work '{prompt}' </dev/null >/tmp/transcript.jsonl 2>/tmp/stderr.log")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref", type=Path, required=True)
    ap.add_argument("--out", type=Path, default=launch.WORK / "out")
    ap.add_argument("--mem", type=int, default=4096)
    ap.add_argument("--smp", type=int, default=2)
    ap.add_argument("--agent-timeout", type=int, default=2400, help="seconds each agent may run")
    ap.add_argument("--only", help="comma-separated agents to run (default all)")
    a = ap.parse_args()
    ref = a.ref.resolve()
    spec = (ref / "spec/SPEC.md").read_text()
    names = [n for n in AGENTS if not a.only or n in a.only.split(",")]
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run_dir = launch.WORK / "agent-runs" / stamp
    run_dir.mkdir(parents=True)
    zai = (Path.home() / ".akuma/z.ai/token").read_text().strip()
    auth_b64 = base64.b64encode((CODEX_HOME / "auth.json").read_bytes()).decode()

    d = launch.Demo(a.out, run_dir, a.mem, a.smp)
    # Each agent gets a SEPARATE brief and only its own secrets, through the metadata service.
    for n, spec_ in AGENTS.items():
        ad = d.mds[spec_["guest"]].guest.agents[n]
        ad.prompts = {"task.md": brief_for(n, spec)}
        ad.secrets = {"ZAI_API_KEY": zai} if spec_["family"] == "zai" else {"CODEX_AUTH_B64": auth_b64}
        (run_dir / f"brief-{n}.md").write_text(ad.prompts["task.md"])

    results = {}
    try:
        d.up()
        g = {1: d.guests[1], 2: d.guests[2]}
        time.sleep(3)
        for n in names:
            install_agent(g[AGENTS[n]["guest"]], n, AGENTS[n])
        print(f"[A] guests up, agents installed: {names}; run dir {run_dir}", flush=True)
        board_url, git_url = d.service_url(8082), d.service_url(8081)

        def run_agent(n, extra_env=""):
            t = time.monotonic()
            gg = g[AGENTS[n]["guest"]]
            cmd = extra_env + agent_cmd(n, AGENTS[n])
            q = "'" + cmd.replace("'", "'\\''") + "'"
            try:
                rc, out = gg.run(f"/sbin/eco-box exec {n} {n} {q}", timeout=a.agent_timeout)
            except subprocess.TimeoutExpired:
                rc, out = 124, "agent timed out"
            results[n] = dict(rc=rc, seconds=round(time.monotonic() - t), out=out[-1500:])
            print(f"[A] agent {n} finished rc={rc} after {results[n]['seconds']}s", flush=True)

        def watch(ns):
            """Print one progress line per agent per minute from the transcript in its box."""
            while any(t.is_alive() for t in threads):
                time.sleep(60)
                for n in ns:
                    gg = g[AGENTS[n]["guest"]]
                    try:
                        _, o = gg.run(f"f=/var/lib/box/containers/{n}/upper/tmp/transcript.jsonl; "
                                      f"echo $(wc -l < $f 2>/dev/null) events; tail -n 1 $f 2>/dev/null | cut -c1-160", timeout=20)
                        print(f"[A] {n}: {' '.join(o.split())[:200]}", flush=True)
                    except Exception as e:  # noqa: BLE001
                        print(f"[A] {n}: progress read failed: {e}", flush=True)

        # phase 1: the two service writers, concurrently
        svc = [n for n in ("rust", "scala") if n in names]
        threads = [threading.Thread(target=run_agent, args=(n,)) for n in svc]
        for t in threads:
            t.start()
        wt = threading.Thread(target=watch, args=(svc,), daemon=True)
        wt.start()
        for t in threads:
            t.join()

        # phase 2: A starts what the agents delivered
        g1 = g[1]
        services_up = {}
        if "rust" in svc:
            bx(g1, "rust", "rust", "mkdir -p /work/repos && for n in rust scala python; do git init -q --bare --initial-branch=main /work/repos/$n.git; done")
            g1.run("/sbin/eco-box daemon rust rust 'cd /work && PORT=8081 GIT_ROOT=/work/repos ./run.sh > /tmp/service.log 2>&1'")
        if "scala" in svc:
            g1.run("/sbin/eco-box daemon scala scala 'cd /work && PORT=8082 ./run.sh > /tmp/service.log 2>&1'")
        time.sleep(2)
        for label, url, path in (("git", git_url, "/healthz"), ("board", board_url, "/health")):
            ok = False
            for _ in range(int(os.environ.get("SERVICE_WAIT_ITERS", "120"))):
                r, o = g[2].run(f"curl -s -m 5 -o /dev/null -w '%{{http_code}}' {url}{path}")
                if o.strip() == "200":
                    ok = True
                    break
                time.sleep(5)
            services_up[label] = ok
            print(f"[A] {label} service from guest 2: {'UP' if ok else 'NOT answering'} ({url}{path})", flush=True)

        # phase 3: the tester
        if "python" in names:
            threads = [threading.Thread(target=run_agent, args=("python", f"export BOARD_URL={board_url} GIT_URL={git_url}; "))]
            threads[0].start()
            wt = threading.Thread(target=watch, args=(["python"],), daemon=True)
            wt.start()
            threads[0].join()

        # phase 4: the oracle, A's own run of the reference suite against the agents' services
        oracle = None
        if all(services_up.values()) and services_up:
            r, o = bx(g[2], "oracle", "python", f"cd /work && rm -rf tests && mkdir tests", timeout=60)
            tar = subprocess.Popen(["tar", "-C", str(ref), "-czf", "-", "tests"], stdout=subprocess.PIPE)
            subprocess.run(g[2].ssh_argv("tar xzf - -C /var/lib/box/containers/oracle/upper/work"), stdin=tar.stdout, check=False)
            tar.wait()
            r, o = bx(g[2], "oracle", "python", f"cd /work/tests && python3 test_remote.py --board-url {board_url} --git-url {git_url} >/tmp/oracle.out 2>&1; rc=$?; tail -40 /tmp/oracle.out; (exit $rc)", timeout=900)
            oracle = dict(rc=r, output=o)
            print(f"[A] reference suite against the agents' services: rc={r}\n{o}", flush=True)

        # phase 5: collect
        for n in names:
            gg = g[AGENTS[n]["guest"]]
            up = f"/var/lib/box/containers/{n}/upper"
            for f, dest in (("tmp/transcript.jsonl", "transcript.jsonl"), ("tmp/stderr.log", "stderr.log"), ("tmp/service.log", "service.log")):
                _, o = gg.run(f"cat {up}/{f} 2>/dev/null", timeout=120)
                (run_dir / f"{n}-{dest}").write_text(o)
            tarf = run_dir / f"{n}-workspace.tgz"
            with tarf.open("wb") as fh:
                subprocess.run(gg.ssh_argv(f"tar -C {up} -czf - --exclude=target --exclude=.scala-build --exclude=.bsp work"), stdout=fh, timeout=300)
        (run_dir / "result.json").write_text(json.dumps(dict(agents=results, services_up=services_up, oracle=oracle,
                                                              board_url=board_url, git_url=git_url), indent=1) + "\n")
    finally:
        d.down()
    print(f"[A] saved to {run_dir}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
