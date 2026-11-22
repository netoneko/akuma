#!/usr/bin/env python3
"""Run a fake-model Codex command tool inside a Denver Scala box, without credentials."""
import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import ecolib as E
import launch


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--codex", type=Path, default=Path.home() / ".local/bin/codex")
    ap.add_argument("--host", type=Path, default=Path.home() / ".local/bin/codex-code-mode-host")
    ap.add_argument("--models", type=Path, default=Path.home() / ".codex/models_cache.json")
    ap.add_argument("--out", type=Path, default=launch.WORK / "out")
    args = ap.parse_args()
    for p in (args.codex, args.host, args.models):
        if not p.is_file():
            ap.error(f"missing input: {p}")
    run_dir = Path(tempfile.mkdtemp(prefix="eco-box-codex-", dir=launch.WORK))
    port = E.free_port()
    model_log = run_dir / "model.log"
    server = subprocess.Popen([sys.executable, str(E.ROOT / "scripts/codex/fakemodel.py"), str(port), str(model_log),
                               'printf DENVER_BOX_TOOL > /work/proof; read v < /work/proof; printf "%s" "$v"'])
    d = launch.Demo(args.out, run_dir)
    try:
        d.up((1,))
        g = d.guests[1]
        rc, out = g.run("/sbin/eco-box exec scala scala true", timeout=120)
        if rc:
            raise RuntimeError(f"could not create scala box: {out}")
        upper = "/var/lib/box/containers/scala/upper"
        g.run(f"mkdir -p {upper}/usr/local/bin {upper}/root/.codex", check=True)
        def put(src, dest, executable=False):
            command = f"cat > {upper}/{dest}" + (f" && chmod 755 {upper}/{dest}" if executable else "")
            with src.open("rb") as f:
                subprocess.run(g.ssh_argv(command), stdin=f, check=True, timeout=600)
        put(args.codex, "usr/local/bin/codex", True)
        put(args.host, "usr/local/bin/codex-code-mode-host", True)
        put(args.models, "root/.codex/models_cache.json")
        config = f'''model = "gpt-6.1-terra"
model_provider = "fake"
sandbox_mode = "danger-full-access"
approval_policy = "never"
[features]
code_mode_host = true
[model_providers.fake]
name = "fake"
base_url = "http://{E.HOST_ALIAS}:{port}"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
'''
        subprocess.run(g.ssh_argv(f"cat > {upper}/root/.codex/config.toml"), input=config, text=True, check=True)
        rc, output = g.run("/sbin/eco-box exec scala scala 'HOME=/root PATH=/usr/local/bin:/usr/bin:/bin codex exec --skip-git-repo-check -C /work go </dev/null'", timeout=300)
        print(output)
        logs = model_log.read_text().splitlines() if model_log.exists() else []
        result = False
        for line in logs:
            if line.startswith("TOOL_OUTPUT "):
                for block in json.loads(line.removeprefix("TOOL_OUTPUT ")):
                    try:
                        item = json.loads(block.get("text", ""))
                        result |= item.get("exit_code") == 0 and item.get("output") == "DENVER_BOX_TOOL"
                    except (ValueError, AttributeError):
                        pass
        if rc != 0 or not result:
            raise RuntimeError(f"boxed Codex tool response failed (rc={rc}); logs: {run_dir}")
        print(f"RESULT PASS: boxed Codex command tool with fake model (sandbox disabled); logs: {run_dir}")
        return 0
    finally:
        d.down()
        server.terminate()
        server.wait(timeout=10)


if __name__ == "__main__":
    sys.exit(main())
