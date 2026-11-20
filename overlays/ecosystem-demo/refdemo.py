#!/usr/bin/env python3
"""Stage 2: build and run the reference services in the boxes, then run their acceptance suite.

    overlays/ecosystem-demo/refdemo.py --ref ../akuma-demo-reference-impl

Boots both guests (launch.Demo), checks the guests' and a box's internet access, copies
the reference tree into the boxes (git-server -> rust box, task-board -> scala box, tests -> python
box), builds them there with cargo and scala-cli/Maven Central over the real network, provisions
the bare repositories, starts both services and runs `tests/test_remote.py` from the python box on
guest 2 against the URLs a guest 2 process actually uses (the host alias + forwarded ports).

Nothing here is application code of ours: the sources are the user's reference implementation.
"""
import argparse
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ecolib as E
import launch
import verify
from verify import bx, check, info

# (scala-cli ships in the scala image) "https://github.com/VirtusLab/scala-cli/releases/download/v1.19.0/scala-cli-x86_64-pc-linux-static.gz"


def push_tree(g, box, ref: Path, name: str, exclude=("target", ".git")):
    """Copy ref/name into the box's /work over ssh (tar), without building anything on the host."""
    g.run(f"/sbin/eco-box exec {box} {box} true")        # create the container + /work
    ex = " ".join(f"--exclude={e}" for e in exclude)
    tar = subprocess.Popen(["tar", "-C", str(ref), "-czf", "-", *ex.split(), name], stdout=subprocess.PIPE)
    r = subprocess.run(g.ssh_argv(f"tar xzf - -C /var/lib/box/containers/{box}/upper/work"), stdin=tar.stdout,
                       capture_output=True, text=True)
    tar.wait()
    return r.returncode == 0 and tar.returncode == 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref", type=Path, required=True)
    ap.add_argument("--out", type=Path, default=launch.WORK / "out")
    ap.add_argument("--mem", type=int, default=3072)
    ap.add_argument("--smp", type=int, default=2)
    ap.add_argument("--git-server-bin", type=Path, help="reuse a previously built reference git-server binary; skips Cargo build")
    ap.add_argument("--scala-interpreted", action="store_true",
                    help="run scala-cli and its Java children with -Xint (Denver JVM workaround)")
    ap.add_argument("--scala-direct", action="store_true",
                    help="run the board with the scala-cli jar itself (no launcher workaround), to exercise `scala-cli run`'s own exec")
    ap.add_argument("--scala-assembly", action="store_true",
                    help="package the board and start it directly with java")
    a = ap.parse_args()
    ref = a.ref.resolve()
    java_env = "JAVA_TOOL_OPTIONS=-Xint " if a.scala_interpreted else ""
    run_dir = Path(tempfile.mkdtemp(prefix="eco-ref-", dir=launch.WORK))
    d = launch.Demo(a.out, run_dir, a.mem, a.smp)
    rc = 1
    try:
        d.up()
        g1, g2 = d.guests[1], d.guests[2]
        time.sleep(3)

        # --- internet, from the guest and from a box ------------------------------------------------
        for who, run in (("guest 1 host root", lambda c: g1.run(c)), ("rust box", lambda c: bx(g1, "rust", "rust", c)),
                         ("scala box", lambda c: bx(g1, "scala", "scala", c)), ("python box (guest 2)", lambda c: bx(g2, "python", "python", c))):
            for _ in range(5):          # the first lookup can race the guest's DHCP/DNS settling
                r, o = run("curl -s -m 20 -o /dev/null -w 'https=%{http_code}' https://example.com; echo")
                if "https=200" in o:
                    break
                time.sleep(4)
            check(f"{who}: https://example.com answers (DNS + TLS through QEMU's NAT)", "https=200" in o, o)
        r, o = bx(g1, "rust", "rust", "curl -s -m 20 -o /dev/null -w 'crates=%{http_code}' https://index.crates.io/config.json; "
                  "curl -s -m 20 -o /dev/null -w ' maven=%{http_code}' https://repo1.maven.org/maven2/; echo")
        check("rust box reaches crates.io's index and Maven Central", "crates=200" in o and "maven=200" in o, o)

        # --- stage the reference tree -------------------------------------------------------------
        check("reference git-server staged into the rust box", push_tree(g1, "rust", ref, "git-server"))
        check("reference task-board staged into the scala box", push_tree(g1, "scala", ref, "task-board"))
        check("reference tests staged into the python box", push_tree(g2, "python", ref, "tests"))

        # --- build ---------------------------------------------------------------------------------
        if a.git_server_bin:
            binary = a.git_server_bin.resolve()
            if not binary.is_file():
                raise RuntimeError(f"missing git server binary: {binary}")
            target = "/var/lib/box/containers/rust/upper/work/git-server/target/release/akuma-demo-git-server"
            g1.run("mkdir -p /var/lib/box/containers/rust/upper/work/git-server/target/release", check=True)
            with binary.open("rb") as src:
                subprocess.run(g1.ssh_argv(f"cat > {target} && chmod 755 {target}"), stdin=src, check=True, timeout=600)
            info("reused git-server binary (Cargo build skipped)", E.sha256_hex(binary.read_bytes()))
        else:
            t = time.monotonic()
            r, o = bx(g1, "rust", "rust", "cd /work/git-server && export CARGO_NET_GIT_FETCH_WITH_CLI=true && "
                      "cargo build --release --locked 2>&1", timeout=3000)
            if r != 0 or "Finished" not in o:
                info("cargo build attempt 1 failed (kept for the record)", o[-600:])
                r, o = bx(g1, "rust", "rust", "cd /work/git-server && export CARGO_NET_GIT_FETCH_WITH_CLI=true && "
                          "cargo build --release --locked 2>&1", timeout=3000)
            info("cargo build --release --locked (git-server), wall", f"{time.monotonic() - t:.0f}s")
            check("rust box: the git server builds with cargo (crates.io + the pinned git dependency)",
                  r == 0 and "Finished" in o and "error" not in o.lower().replace("error:", "ERR:").split("finished")[0][-300:], o)

        t = time.monotonic()
        r, o = bx(g1, "scala", "scala", f"{java_env}scala-cli version --cli-version 2>&1 | head -3", timeout=600)
        check("scala box: scala-cli (JVM jar, v1.19.0) runs", r == 0 and "1.19" in o, o)
        r, o = bx(g1, "scala", "scala", f"cd /work/task-board && {java_env}scala-cli compile Main.scala --server=false 2>&1", timeout=3000)
        info("scala-cli compile (task-board; Scala 3.3.6 + ujson from Maven Central), wall", f"{time.monotonic() - t:.0f}s")
        check("scala box: the task board compiles with scala-cli (dependencies from Maven Central)", r == 0 and "rror" not in o and "xception" not in o, o)
        if a.scala_assembly:
            r, o = bx(g1, "scala", "scala", f"cd /work/task-board && {java_env}scala-cli --power package Main.scala --server=false --assembly -o /work/board.jar 2>&1", timeout=3000)
            check("scala box: task board assembly is packaged", r == 0 and "Wrote" in o, o)

        # --- run -----------------------------------------------------------------------------------
        bx(g1, "rust", "rust", "mkdir -p /work/repos && for n in rust scala python; do git init -q --bare --initial-branch=main /work/repos/$n.git; done; echo provisioned")
        g1.run("/sbin/eco-box daemon rust rust 'cd /work && GIT_ROOT=/work/repos PORT=8081 /work/git-server/target/release/akuma-demo-git-server > /work/git.log 2>&1'")
        board_command = ("java -jar /work/board.jar" if a.scala_assembly else
                         "java -Dos.name=Linux -jar /opt/scala-cli/scala-cli.jar run Main.scala --server=false" if a.scala_direct else
                         "scala-cli run Main.scala --server=false")
        g1.run(f"/sbin/eco-box daemon scala scala 'cd /work/task-board && {java_env}{board_command} > /work/board.log 2>&1'")
        # The services live in the boxes `gitsrv` / `board`: same boxes as the builds (a container id is its writable layer); nothing may `eco-box exec` them afterwards, it closes the box.
        time.sleep(1)
        board_url, git_url = d.service_url(8082), d.service_url(8081)
        info("URLs as guest 2 uses them", f"board {board_url}  git {git_url}")
        ok_git = ok_board = False
        for i in range(240):
            if i and i % 60 == 0:
                _, lg = g1.run("tail -c 600 /var/lib/box/containers/scala/upper/work/board.log")
                info(f"board.log after {i * 5}s", lg)
            r1, o1 = g2.run(f"curl -s -m 5 {git_url}/healthz -o /dev/null -w '%{{http_code}}'")
            r2, o2 = g2.run(f"curl -s -m 5 {board_url}/health")
            ok_git, ok_board = o1.strip() == "200", '"ok"' in o2
            if ok_git and ok_board:
                break
            time.sleep(5)
        if not (ok_git and ok_board):
            for box, f in (("scala", "board.log"), ("rust", "git.log")):
                _, lg = g1.run(f"tail -c 1500 /var/lib/box/containers/{box}/upper/work/{f}; echo; ps | grep -E 'java|scala|akuma-demo' | grep -v grep | head -5")
                info(f"{box} {f} (service did not answer)", lg)
        check("guest 2 reaches the git service /healthz via the host alias", ok_git, o1)
        check("guest 2 reaches the task board /health via the host alias", ok_board, o2)

        # --- the reference acceptance suite, from guest 2 ----------------------------------------------
        r, o = bx(g2, "python", "python", f"cd /work/tests && python3 test_remote.py --board-url {board_url} --git-url {git_url} 2>&1 | tail -25", timeout=900)
        check("reference acceptance suite passes from the python box on guest 2", r == 0 and "OK" in o.split("\n")[-3:].__str__(), o)
        # hairpin: a box on guest 1 pushes to the git service on guest 1 through the same host address.
        r, o = bx(g1, "hp", "scala", f"cd /tmp && rm -rf hp && git init -q hp && cd hp && git config user.name x && git config user.email x@demo.invalid && "
                  f"echo hairpin > h.txt && git add h.txt && git commit -q -m hairpin && git push -q {git_url}/scala.git HEAD:main 2>&1 | tail -3; echo push-rc=$?")
        check("scala box (guest 1) pushes to the git service on its own guest via the host alias", "push-rc=0" in o and "rror" not in o, o)
        # host review: clone as the operator would.
        hp = g1.forwards[8081]
        with tempfile.TemporaryDirectory(dir=run_dir) as td:
            cl = subprocess.run(["git", "clone", "-q", f"http://127.0.0.1:{hp}/scala.git", td + "/c"], capture_output=True, text=True)
            lg = subprocess.run(["git", "-C", td + "/c", "log", "--oneline"], capture_output=True, text=True)
            check("the host clones an agent's repository from the git service and sees its commits",
                  cl.returncode == 0 and bool(lg.stdout.strip()), cl.stderr + lg.stdout)
    finally:
        d.down()
    bad = [r for r in verify.RESULTS if not r["ok"]]
    print(f"\n{len(verify.RESULTS) - len(bad)} ok/info, {len(bad)} failed")
    (run_dir / "refdemo-report.json").write_text(json.dumps(verify.RESULTS, indent=1) + "\n")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
