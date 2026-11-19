"""Stage-1 verification: toolchains, isolation, bootstrap, cross-guest networking, capacity.

Everything runs against the real images on the main amd64 kernel. No application code: the
hello-worlds are toolchain checks and the cross-guest check serves a directory with stock
`busybox httpd`. Imported by launch.py (`launch.py verify`).
"""
import concurrent.futures as cf
import json
import re
import subprocess
import sys
import time
from pathlib import Path

import ecolib as E

RESULTS = []


def check(name, ok, detail=""):
    RESULTS.append({"name": name, "ok": bool(ok), "detail": detail if not ok else ""})
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok else "\n      " + str(detail)[-700:]), flush=True)
    return bool(ok)


def info(key, value):
    RESULTS.append({"name": key, "ok": True, "info": value})
    print(f"INFO  {key}: {value}", flush=True)


def bx(g, name, image, cmd, timeout=900):
    """Run `cmd` in box `name` (image `image`) on guest `g`: (rc, output)."""
    q = "'" + cmd.replace("'", "'\\''") + "'"
    return g.run(f"/sbin/eco-box exec {name} {image} {q}", timeout=timeout)


def main(args, run_dir, Demo):
    out = args.out
    man = json.loads((out / "MANIFEST.json").read_text())
    for k, v in man["images"].items():
        info(f"image {k}", f"{v['diff_id']} ({v['packages']} packages)")
    for k, v in man["disks"].items():
        info(f"disk {k}", v["sha256"])
    info("source revision", man["source_revision"] + (" (dirty tree)" if man["source_tree_dirty"] else ""))

    d = Demo(out, run_dir, args.mem, args.smp, tamper=True)
    try:
        # --- bootstrap integrity: guest 2 first with a WRONG digest -------------------------
        d.mirror = E.MirrorServer(__import__("launch").WORK / "cache/mirror", run_dir)
        for m in d.mds.values():
            m.start()
        t0 = time.monotonic()
        for n in (1, 2):
            d.start_guest(n)
        for n in (1, 2):
            d.guests[n].wait_ready(300)
        info("boot to ssh-ready (both guests, parallel)", f"{time.monotonic() - t0:.1f}s")
        g1, g2 = d.guests[1], d.guests[2]
        time.sleep(3)
        rc, o = g2.run("cat /run/eco/bootstrap.rc 2>&1; ls /var/lib/agents 2>&1 | wc -l; ls /run/eco")
        check("tampered bootstrap script (wrong sha256) is refused: no bootstrap.rc success, no agent data",
              not o.startswith("0") and "python" not in g2.run("ls /var/lib/agents")[1] and "bootstrap.sh" not in o, o)
        g2.stop()
        d.mds[2].guest.bootstrap_sha256 = d.script_sha
        d.mds[2].guest.log.clear()
        g2 = d.start_guest(2)
        g2.wait_ready(300)
        time.sleep(3)

        # --- bootstrap result ----------------------------------------------------------------
        for g, agents in ((g1, ["rust", "scala"]), (g2, ["python"])):
            rc, o = g.run("test -s /var/run/herd/control.token && stat -c %a /var/run/herd/control.token")
            check(f"guest{g.n}: herd control token exists and is root-only", rc == 0 and o.strip() == "600", o)
            rc, o = g.run("cat /run/eco/bootstrap.rc; ls -l /var/lib/agents; for a in /var/lib/agents/*; do ls -l $a/secrets $a/prompts; done; "
                          "grep -c ssh-ed25519 /etc/sshd/authorized_keys")
            check(f"guest{g.n}: bootstrap script verified and run (rc 0), agents installed", o.startswith("0") and all(a in o for a in agents), o)
            rc, o = g.run("stat -c '%a %U %n' /var/lib/agents /var/lib/agents/*/secrets /var/lib/agents/*/secrets/*")
            lines = [l for l in o.splitlines() if l.strip()]
            check(f"guest{g.n}: secrets are root-only (dirs 700, files 600)",
                  lines and all(l.split()[0] in ("700", "600") and l.split()[1] == "root" for l in lines), o)
            log = d.md_log(g.n)
            gets = [p for (_, p, c) in log if c == 200]
            check(f"guest{g.n}: each secret fetched exactly once", all(
                sum(1 for p in gets if p.endswith(f"/agents/{a}/secrets/AGENT_TOKEN")) == 1 for a in agents), log)
        
        # --- the toolchains, each in its own box ----------------------------------------------
        common = ("bash --version | head -1; git --version; apk --version; curl --version | head -1; "
                  "ls --version | head -1; ")
        tools = {
            (g1, "rust", "rust"): ("rustc --version; cargo --version; " + common, ["rustc 1.96", "cargo 1.96", "GNU bash", "git version", "apk-tools", "curl 8", "GNU coreutils"]),
            (g1, "scala", "scala"): ("java -version 2>&1 | head -1; scala -version 2>&1 | head -1; " + common, ["openjdk version \"21", "Scala", "GNU bash", "git version", "apk-tools", "curl 8", "GNU coreutils"]),
            (g2, "python", "python"): ("python3 --version; pytest --version 2>&1 | head -1; " + common, ["Python 3.12", "pytest 8", "GNU bash", "git version", "apk-tools", "curl 8", "GNU coreutils"]),
        }
        for (g, name, image), (cmd, want) in tools.items():
            t = time.monotonic()
            rc, o = bx(g, name, image, cmd)
            check(f"{name} box: tool versions run ({', '.join(w.split()[0] for w in want[:2])}, bash, git, apk, curl, coreutils)",
                  rc == 0 and all(w in o for w in want), o)
            info(f"{name} box first-run time", f"{time.monotonic() - t:.1f}s")

        hello = {
            "rust": "cd /tmp && echo 'fn main(){println!(\"hello rust\");}' > h.rs && rustc h.rs -o h && ./h",
            "scala": "cd /tmp && echo 'object H { def main(a: Array[String]): Unit = println(\"hello scala\") }' > H.scala && scalac H.scala && scala -cp . H",
            "python": "python3 -c 'print(\"hello python\")'",
        }
        for (g, name, image) in tools:
            t = time.monotonic()
            rc, o = bx(g, name, image, hello[name])
            check(f"{name} box: hello-world compile/run (toolchain check)", rc == 0 and f"hello {name}" in o, o)
            info(f"{name} hello time", f"{time.monotonic() - t:.1f}s")

        if not man.get("rust_cargo_cache"):
            rc, o = bx(g1, "rust", "rust", "mkdir -p /tmp/dep && cd /tmp/dep && cargo init --name dep -q 2>&1 && cargo run -q 2>&1 | tail -3")
            check("rust box: cargo init + run of a std-only crate (no cargo cache in this image)", rc == 0 and "Hello, world" in o, o)
        else:
          rc, o = bx(g1, "rust", "rust",
                   "mkdir -p /tmp/dep && cd /tmp/dep && cargo init --name dep -q --offline 2>&1 && "
                   "printf 'serde_json = \"1\"\\ntiny_http = \"0.12\"\\n' >> Cargo.toml && "
                   "printf 'fn main(){ let v: serde_json::Value = serde_json::from_str(\"{\\\\\"a\\\\\":1}\").unwrap(); println!(\"dep {}\", v[\"a\"]); }\\n' > src/main.rs && "
                   "cargo run -q --offline 2>&1 | tail -5")
          check("rust box: offline registry cache builds a crate with serde_json + tiny_http (no network)", rc == 0 and "dep 1" in o, o)
        rc, o = bx(g1, "scala", "scala",
                   "cd /tmp && echo 'object G { def main(a: Array[String]): Unit = println(new com.google.gson.Gson().toJson(Array(1,2))) }' > G.scala && "
                   "scalac -cp /opt/deps/gson-2.11.0.jar G.scala && scala -cp /opt/deps/gson-2.11.0.jar:. G")
        check("scala box: dependency cache (/opt/deps gson jar) compiles and runs", rc == 0 and "[1,2]" in o, o)
        rc, o = bx(g2, "python", "python", "cd /tmp && printf 'def test_ok():\\n    assert 1+1 == 2\\n' > test_x.py && pytest -q test_x.py 2>&1 | tail -3")
        check("python box: pytest runs a test", rc == 0 and "1 passed" in o, o)

        for (g, name, image) in tools:
            rc, o = bx(g, name, image, "cd /work && git status --short | wc -l && git log --oneline 2>&1 | head -1; echo hi > f.txt && git add f.txt && "
                       "git commit -q -m 'first' && git log --oneline && git config user.name")
            check(f"{name} box: empty workspace is a git repo; add + commit works with the throwaway identity",
                  rc == 0 and "first" in o and "demo" in o and "does not have any commits" in o, o)
            rc, o = bx(g, name, image, "apk add jq 2>&1 | tail -2; jq --version")
            check(f"{name} box: `apk add jq` from the pinned local mirror", rc == 0 and "jq-" in o, o)

        # --- recreate from the image ---------------------------------------------------------
        t = time.monotonic()
        g1.run("/sbin/eco-box destroy rust")
        rc, o = bx(g1, "rust", "rust", "cd /work && git log --oneline 2>&1 | head -1; ls -A /work; ls /tmp | wc -l")
        dt = time.monotonic() - t
        check("rust box destroyed and recreated from its image: workspace empty again, in seconds",
              rc == 0 and "does not have any commits" in o and "f.txt" not in o and dt < 30, o)
        info("destroy + recreate + first command", f"{dt:.1f}s")

        # --- isolation ------------------------------------------------------------------------
        tok = d.secret_values
        before = len(d.md_log(1))
        rc, o = g1.run("curl -s -m 5 -o /dev/null -w '%{http_code}' -X PUT http://169.254.169.254/latest/api/token")
        check("control: the host root (uid 0, host box) CAN reach the metadata address", o.strip().endswith("200"), o)
        mid = len(d.md_log(1))
        rc, o = bx(g1, "rust", "rust", "curl -s -m 5 -o /dev/null -w 'http=%{http_code}' -X PUT http://169.254.169.254/latest/api/token; echo \" curl_rc=$?\"")
        after = len(d.md_log(1))
        check("from inside a box the metadata address is unreachable (no HTTP answer; the server saw nothing)",
              "http=200" not in o and "http=000" in o and after == mid, f"{o} log {mid}->{after}")
        rc, o = bx(g1, "scala", "scala", "curl -s -m 5 -o /dev/null -w 'http=%{http_code}' http://169.254.169.254/latest/meta-data/agents/rust/secrets/AGENT_TOKEN")
        check("from inside a box the other agent's secret path on the metadata address is unreachable", "http=200" not in o, o)

        # Secrets: own env yes, other agent's no, host files not visible.
        g1.run("/sbin/eco-box daemon scala scala 'sleep 120'")
        rc, o = bx(g1, "rust", "rust", "env | grep -c ^AGENT_TOKEN=; env | grep ^AGENT_TOKEN= | sha256sum | cut -c1-8")
        own = o.split()[0] if o.split() else ""
        rc, o2 = bx(g1, "rust", "rust", "echo $AGENT_TOKEN")
        check("rust box sees its own secret as an environment variable", o2.strip() == tok["rust"], "mismatch (values not shown)")
        rc, o3 = bx(g1, "rust", "rust",
                    "echo $AGENT_TOKEN > /dev/null; "
                    "cat /proc/[0-9]*/environ 2>/dev/null | tr '\\0' '\\n' | grep -c ^AGENT_TOKEN= ; "
                    "ls /var/lib/agents 2>&1 | head -1; ls /var/lib/box 2>&1 | head -1; "
                    "(cat /var/lib/agents/scala/secrets/AGENT_TOKEN 2>&1 | head -1)")
        info("rust box probes (own-environ count / host paths)", o3.replace(tok["scala"], "<scala-secret>").replace(tok["rust"], "<rust-secret>").strip())
        check("rust box cannot read the scala agent's secret from the host's /var/lib/agents",
              tok["scala"] not in o3 and "No such file" in o3, "probe output hides values")
        rc, o4 = bx(g1, "rust", "rust", "cat /proc/[0-9]*/environ 2>/dev/null | tr '\\0' '\\n' | grep ^AGENT_TOKEN= | cut -d= -f2")
        check("rust box cannot read the scala agent's secret from other processes' /proc/*/environ while scala runs",
              tok["scala"] not in o4, "other agent's secret visible via /proc")
        rc, o5 = bx(g1, "rust", "rust",
                    f"grep -rl {tok['scala']} /etc /root /work /tmp /opt/cargo-vendor /var /home /srv 2>/dev/null | head -3")
        check("the scala secret appears nowhere in the rust box's filesystem", tok["scala"] not in o5 and not o5.strip(), o5.replace(tok["scala"], "<s>"))
        g1.run("/sbin/eco-box destroy scala")
        # Secret hygiene: not in images, serial logs, launcher logs.
        leak = []
        for f in [out / "guest1.img", out / "guest2.img"]:
            for who, v in tok.items():
                if subprocess.run(["grep", "-a", "-q", "-F", v, str(f)]).returncode == 0:
                    leak.append((f.name, who))
        for f in run_dir.glob("*.log"):
            for who, v in tok.items():
                if subprocess.run(["grep", "-a", "-q", "-F", v, str(f)]).returncode == 0:
                    leak.append((f.name, who))
        check("no secret value is in either disk image or any launcher/serial log", not leak, leak)

        # --- cross-guest networking with stock tools -------------------------------------------
        for port, name, image in ((8081, "rust", "rust"), (8082, "scala", "scala")):
            bx(g1, name, image, f"mkdir -p /work/site && echo 'served-by-{name}' > /work/site/index.html")
            g1.run(f"/sbin/eco-box daemon {name} {image} 'busybox-extras httpd -f -p 0.0.0.0:{port} -h /work/site'")
        time.sleep(3)
        for port, name in ((8081, "rust"), (8082, "scala")):
            rc, o = g2.run(f"curl -s -m 10 {d.service_url(port)}/index.html")
            check(f"guest 2 reaches guest 1 :{port} (busybox-extras httpd in the {name} box)", f"served-by-{name}" in o, o)
        rc, o = bx(g2, "python", "python", f"curl -s -m 10 {d.service_url(8081)}/index.html; curl -s -m 10 {d.service_url(8082)}/index.html")
        check("guest 2's python box reaches both services too", "served-by-rust" in o and "served-by-scala" in o, o)
        g1.run("/sbin/eco-box destroy rust; /sbin/eco-box destroy scala")

        # --- capacity ----------------------------------------------------------------------------
        cap(g1, d)
    finally:
        d.down()
    bad = [r for r in RESULTS if not r["ok"]]
    print(f"\n{len(RESULTS) - len(bad)} ok/info, {len(bad)} failed")
    (args.report or (out / "verify-report.json")).write_text(json.dumps(RESULTS, indent=1) + "\n")
    return 1 if bad else 0


def cap(g1, d):
    """A JVM + a Scala build + a Codex-sized process count on this kernel (448-thread table)."""
    rc, o = g1.run("grep -c . /proc/cpuinfo >/dev/null; echo ok")
    src = ("object T { def main(a: Array[String]): Unit = { val n = a(0).toInt; val ts = (1 to n).map(i => new Thread(() => Thread.sleep(120000))); "
           "ts.foreach(_.start()); println(s\"threads started $n\"); ts.foreach(_.join()) } }")
    rc, o = bx(g1, "jvm-load", "scala", f"cd /tmp && echo '{src}' > T.scala && scalac T.scala")
    check("capacity: JVM load program compiles in its own box", rc == 0, o)
    # The load box has its own writable layer; compile the program there before starting it.
    g1.run("/sbin/eco-box daemon jvm-load scala 'cd /tmp && scala -cp . T 150 > /tmp/t.out 2>&1'")
    g1.run("/sbin/eco-box daemon sleep-load rust 'for i in $(seq 1 60); do sleep 120 & done; wait'")
    time.sleep(8)
    rc, load = g1.run("cat /var/lib/box/containers/jvm-load/upper/tmp/t.out 2>/dev/null; ps | grep -c '[s]leep 120'")
    lines = load.strip().splitlines()
    try:
        sleepers = int(lines[-1])
    except (ValueError, IndexError):
        sleepers = 0
    check("capacity: JVM started 150 threads and 60 sleeper processes are running",
          "threads started 150" in load and sleepers >= 60, load)
    t = time.monotonic()
    rc, o = bx(g1, "scala", "scala", "cd /tmp && mkdir -p b && cd b && for i in 1 2 3; do echo \"object B$i { def f = $i }\" > B$i.scala; done && scalac *.scala && echo scala-build-ok")
    check("capacity: a Scala build completes beside a JVM with 150 threads and 60 sleeper processes", rc == 0 and "scala-build-ok" in o, o)
    info("capacity scala build wall time", f"{time.monotonic() - t:.1f}s")
    rc, o = bx(g1, "rust", "rust", "mkdir -p /tmp/cap && cd /tmp/cap && cargo init --name cap -q 2>&1; cargo build -q -j4 2>&1 | tail -3; echo cargo-rc=$?")
    check("capacity: a cargo build completes under the same load", "cargo-rc=0" in o and "error" not in o.lower(), o)
    g1.run("/sbin/eco-box destroy jvm-load; /sbin/eco-box destroy sleep-load")
    peak = 0
    for m in re.finditer(r"new high-water: (\d+) live user threads \(terminated=\d+ free=(\d+) ceiling=(\d+)\)", Path(g1.serial).read_text(errors="replace")):
        peak = max(peak, int(m.group(1)))
        ceil = m.group(3)
    info("capacity: peak live user threads in the kernel's table", f"{peak} of {ceil if peak else '?'}")
