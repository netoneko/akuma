#!/usr/bin/env python3
"""Generate, replay and reduce mmaplazy traces on Linux and optionally Akuma."""

import argparse
import pathlib
import random
import shlex
import subprocess
import tempfile
import uuid

import mem_suite


def generate(seed):
    # mmaplazy maps 24 pages. Read the head before splitting, then leave
    # both the surviving tail and the restored middle cold until after unmap.
    rng = random.Random(seed)
    split = rng.randint(5, 15)
    width = rng.randint(2, 4)
    first_hole = split - rng.randint(1, 2)
    hole_end = split + rng.randrange(1, width)
    cold_tail = rng.randrange(split + width, 24)
    return [
        f"C:{rng.randrange(2)}:1",
        f"N:{split}:{width}",
        f"U:{first_hole}:{hole_end - first_hole}",
        f"R:{hole_end}:{split + width - hole_end}",
        f"C:{cold_tail}:1",
        f"C:{rng.randrange(hole_end, split + width)}:1",
        "C:23:1",
    ]


def outcome(output, rc):
    if rc == 0 and "TRACE PASS" in output.splitlines():
        return "pass", ""
    if rc == 1:
        for line in output.splitlines():
            if line.startswith("TRACE MISMATCH "):
                return "mismatch", line
    return "error", f"exit {rc}: {output.strip()}"


def reduce_trace(trace, reproduces):
    # Restart after every deletion. The trace is tiny, and replay checks the
    # page-state preconditions and the exact mismatch before keeping a deletion.
    while len(trace) > 1:
        for i in range(len(trace)):
            candidate = trace[:i] + trace[i + 1:]
            if reproduces(candidate):
                trace = candidate
                break
        else:
            break
    return trace


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=pathlib.Path, help="compiled static mmaplazy probe")
    parser.add_argument("--port", type=int, help="also run the same binary in the Akuma guest")
    parser.add_argument("--identity", help="private SSH key for the guest")
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--cases", type=int, default=24)
    parser.add_argument("--replay", nargs="+", metavar="OP", help="run this concrete trace")
    args = parser.parse_args()
    if args.cases < 1:
        parser.error("--cases must be positive")
    if args.identity:
        mem_suite.SSH_EXTRA.extend(["-i", args.identity, "-o", "IdentitiesOnly=yes"])
    binary = args.binary.resolve(strict=True)
    guest_binary = f"/tmp/mmaplazy-{uuid.uuid4().hex}"

    with tempfile.TemporaryDirectory(prefix="mmaplazy-") as directory:
        def linux(trace):
            try:
                p = subprocess.run([binary, "--trace", directory, *trace],
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, timeout=10, cwd=directory)
            except subprocess.TimeoutExpired:
                return "error", "timed out"
            return outcome(p.stdout, p.returncode)

        def akuma(trace):
            command = shlex.join([guest_binary, "--trace", "/tmp", *trace])
            output, rc = mem_suite.ssh(args.port, command, timeout=15)
            # A dropped SSH connection can produce exit 255 before the probe runs.
            if rc == 255 and not output.strip():
                output, rc = mem_suite.ssh(args.port, command, timeout=15)
            return outcome(output, rc)

        cases = [("replay", args.replay)] if args.replay else [
            (str(seed), generate(seed)) for seed in range(args.seed, args.seed + args.cases)
        ]
        arms = [("Linux", linux)]
        try:
            if args.port:
                mem_suite.push(args.port, binary, guest_binary)
                arms.append(("Akuma", akuma))
            for seed, trace in cases:
                for name, run in arms:
                    result = run(trace)
                    if result[0] == "pass":
                        continue
                    print(f"{name} {seed}: {result[1]}")
                    print(f"Original ({len(trace)} ops): {shlex.join(trace)}")
                    if result[0] == "mismatch":
                        def reproduces(candidate):
                            return (run(candidate) == result and
                                    (name == "Linux" or linux(candidate)[0] == "pass"))

                        reduced = reduce_trace(trace, reproduces)
                        print(f"Reduced ({len(reduced)} ops): {shlex.join(reduced)}")
                    return 1
        finally:
            if args.port:
                mem_suite.ssh(args.port, f"rm -f {guest_binary}", timeout=10)
        print(f"PASS: {len(cases)} traces on {'Linux and Akuma' if args.port else 'Linux'}")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
