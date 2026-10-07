#!/usr/bin/env python3
"""Compile a `CHAN=1 w0-trace.sh` run into one replayable segment per channel.

    python3 overlays/ryzen/w2-chans.py ~/.akuma/w0/<run> crates/akuma-rtw89/seq

For each `mark: chan N` phase of the run (Linux in monitor mode, `iw set
channel N`, so mac80211 calls rtw89's own set_channel and nothing else
happens) it runs `w2-merge.py --phase "chan N"` and `w2-seqgen.py` and writes
`chanNN.seq`. A channel switch is register-only (no H2C), so no MAC or BSSID is
involved and nothing private is in the output. Run order matters little: each
segment writes the full channel state (channel fields, the TX power tables, the
gain offsets), not a delta from the previous channel.
"""
import os
import subprocess
import sys

here = os.path.dirname(os.path.abspath(__file__))
run, outdir = sys.argv[1], sys.argv[2]
chans = [int(c) for c in sys.argv[3:]] or range(1, 14)
for c in chans:
    txt = os.path.join(outdir, f".chan{c:02}.txt")
    with open(txt, "w") as f:
        subprocess.run([sys.executable, f"{here}/w2-merge.py", run, "--phase", f"chan {c}",
                        "--collapse", "--no-fwdl", "--ts"], stdout=f, check=True)
    subprocess.run([sys.executable, f"{here}/w2-seqgen.py", txt,
                    os.path.join(outdir, f"chan{c:02}.seq")], check=True)
    os.remove(txt)
