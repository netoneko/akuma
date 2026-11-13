import os
import sys, subprocess
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "utils"))
import hpbox
# usage: rzput.py LOCAL REMOTE [chmod]  |  rzput.py -c 'cmd'
if sys.argv[1] == "-c":
    rc, o, e = hpbox.ryzen_root(sys.argv[2], timeout=int(sys.argv[3]) if len(sys.argv) > 3 else 300)
    sys.stdout.write(o); sys.stderr.write(e); sys.exit(rc)
data = open(sys.argv[1], "rb").read()
r = subprocess.run(hpbox.RZ_ROOT + [f"cat > {sys.argv[2]}.new && chmod +x {sys.argv[2]}.new && mv {sys.argv[2]}.new {sys.argv[2]} && md5sum {sys.argv[2]}"], input=data, capture_output=True, timeout=300)
sys.stdout.write(r.stdout.decode()); sys.stderr.write(r.stderr.decode()); sys.exit(r.returncode)
