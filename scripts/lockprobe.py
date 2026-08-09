#!/usr/bin/env python3
"""Name the lock (and the fault) a wedged Akuma SMP VM is stuck on.

Boot the VM with `GDB=1` (scripts/cargo_runner.sh), then point this at its
gdbstub port. It works on a stock `--release`-style build: symbolisation comes
from `.symtab`, so **no debug info is needed** — which matters, because adding
DWARF changes the loaded image by ~100 KB and can move a timing-sensitive race.

    GDB=1 SMP=4 INSTANCE=2 scripts/cargo_runner.sh <elf>     # gdbstub on :1236
    scripts/lockprobe.py 1236 [-n SAMPLES] [-o OUT]

What it reports, per core:

  * PC / LR / SP symbolised, plus the EL1 syndrome registers (`ESR_EL1`,
    `ELR_EL1`, `FAR_EL1`) with the ESR exception class decoded. On a core stuck
    in a fault loop these name the faulting instruction and address outright.
  * every register that points at a named static — a kernel lock IS a static, so
    a register holding `&lock` resolves to its NAME. That is what identifies the
    lock when the PC only tells you "some spin loop".
  * the stack, symbolised, as a call-chain hint (no CFI: "these are on the path",
    not exact frames).

and decodes the BKL (`akuma_exec::bkl::KERNEL_LOCK`) into a verdict:

    owner != 0                    -> HELD by core owner-1; go read that core's ESR
    owner == 0, next != serving   -> LOST TICKET: free lock, waiters never served
    owner == 0, next == serving   -> BKL idle; the wedge is NOT the BKL

`KernelLock` is **not** `#[repr(C)]`, so its field order in memory is chosen by
the compiler and is NOT declaration order (measured once: barged[8] at +0x00,
owner at +0x08, next_ticket at +0x0c, now_serving at +0x10). Hard-coding those
offsets would silently rot on the next build, so they are recovered from the
binary itself by disassembling `KernelLock::release`. Guessing here is not a
theoretical risk: the first version of this script assumed declaration order and
reported a confident "LOST TICKET" for a lock that was plainly HELD.

With `-n` > 1 it samples repeatedly and reports whether each core's PC moved.
Read that carefully on a `-accel hvf` guest: HVF only syncs vCPU state on exit
to the hypervisor, so a core spinning tightly in guest mode can report frozen
registers. Frozen state is therefore evidence of a loop only when corroborated
(e.g. that vCPU still burning ~100% host CPU). A fault->handler->eret->refault
loop legitimately reproduces byte-identical state every iteration, because the
handler epilogue restores everything the prologue pushed.

Background: docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md
"""
import argparse
import bisect
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_ELF = os.path.join(REPO, "target/aarch64-unknown-none/release-smp-shared/akuma")
# Window used to reject absolute/host symbols and to sanity-check register values
# before symbolising them. It must cover the whole loaded image *including .bss* --
# `KERNEL_LOCK` itself is a .bss symbol, and when the image outgrew a too-tight
# IMG_HI every probe failed with a misleading "KERNEL_LOCK not found - wrong ELF?".
# 0x40100000 is KERNEL_PHYS_BASE; 8 MB of headroom keeps this from rotting again
# (release-smp-shared's .bss ended around 0x404d0000 when this was last widened).
IMG_LO, IMG_HI = 0x40100000, 0x40900000
BARGE_MAX_CORES = 8          # KernelLock::barged: [AtomicBool; BARGE_MAX_CORES]

# ESR_EL1 exception class (bits 31:26) -> meaning. Only the classes that show up
# in a kernel wedge are named; anything else prints its raw EC.
EC_NAMES = {
    0x00: "unknown reason",
    0x0E: "illegal execution state",
    0x15: "SVC (aarch64)",
    0x18: "trapped MSR/MRS/system insn",
    0x20: "instruction abort, lower EL",
    0x21: "instruction abort, SAME EL",
    0x22: "PC alignment fault",
    0x24: "data abort, lower EL",
    0x25: "DATA ABORT, SAME EL (kernel-mode fault)",
    0x26: "SP alignment fault",
    0x2F: "SError",
    0x30: "breakpoint, lower EL",
    0x31: "breakpoint, same EL",
    0x3C: "BRK (software breakpoint)",
}
DFSC_NAMES = {
    0x04: "translation fault L0", 0x05: "translation fault L1",
    0x06: "translation fault L2", 0x07: "translation fault L3",
    0x09: "access flag fault L1", 0x0A: "access flag fault L2",
    0x0B: "access flag fault L3",
    0x0C: "permission fault L0", 0x0D: "permission fault L1",
    0x0E: "permission fault L2", 0x0F: "PERMISSION FAULT L3",
    0x10: "synchronous external abort",
    0x21: "alignment fault",
}


def sh(argv, timeout=180):
    return subprocess.run(argv, capture_output=True, text=True,
                          timeout=timeout, cwd=REPO).stdout


class Symbols:
    """Kernel .symtab, minus ABSOLUTE symbols.

    The kernel defines page-table constants (PT_VALID=1, PT_TABLE=2, ...) as
    absolute symbols. Leaving them in makes every small integer in a register
    'resolve' to a name (X11=0x1 -> PT_VALID+0) and buries the real signal.
    """

    def __init__(self, elf):
        self.elf = elf
        self.syms = []
        for line in sh(["rust-nm", "-n", elf]).splitlines():
            parts = line.split(maxsplit=2)
            if len(parts) == 3 and re.fullmatch(r'[0-9a-fA-F]+', parts[0]):
                if parts[1] in 'aANUwW':
                    continue
                addr = int(parts[0], 16)
                if IMG_LO <= addr < IMG_HI:
                    self.syms.append((addr, parts[2].strip()))
        self.syms.sort()
        self.addrs = [s[0] for s in self.syms]

    def at(self, addr):
        if not (IMG_LO <= addr < IMG_HI):
            return None
        i = bisect.bisect_right(self.addrs, addr) - 1
        if i < 0:
            return None
        base, name = self.syms[i]
        return None if addr - base > 0x8000 else (name, addr - base)

    def find(self, substr):
        for a, n in self.syms:
            if substr in n:
                return a, n
        return None, None


def bkl_offsets(elf, syms):
    """Recover KernelLock's field offsets by disassembling `release`.

    `release` is the one function that touches three of the four fields with
    distinguishable instructions:
        owner       : ldxr / cmp / stlxr wzr      (compared to core+1, cleared)
        barged[]    : add x, x, w0 then ldxrb     (byte array indexed by core)
        now_serving : ldxr / add #1 / stlxr       (advanced by one)
    next_ticket is then the remaining 4-byte slot in the struct.
    """
    addr, _ = syms.find("4syncNtB5_10KernelLock7release")
    if addr is None:
        return None
    dis = sh(["rust-objdump", "-d",
              f"--start-address={hex(addr)}", f"--stop-address={hex(addr + 0x100)}", elf])
    base, _ = syms.find("3bkl11KERNEL_LOCK")
    page = base & ~0xFFF
    out, cur = {}, None
    for line in dis.splitlines():
        m = re.search(r'add\s+x(\d+), x\1, #(0x[0-9a-f]+)', line)
        if m:
            cur = page + int(m.group(2), 16)
            continue
        if cur is None:
            continue
        if 'stlxr' in line and 'wzr' in line:
            out['owner'] = cur - base
        elif 'ldxrb' in line:
            out['barged'] = cur - base
        elif re.search(r'add\s+w\d+, w\d+, #0x1', line):
            out['now_serving'] = cur - base
    if 'owner' not in out or 'now_serving' not in out:
        return None
    # next_ticket is the remaining u32 slot. `barged` is a [AtomicBool; 8] and so
    # covers EIGHT bytes — treating it as one 4-byte slot picks +0x4, which lands
    # INSIDE the bool array and reads two barge flags as a ticket counter.
    barged = out.get('barged')
    taken = {out['owner'], out['now_serving']}
    for cand in (0x0, 0x4, 0x8, 0xC, 0x10):
        if cand in taken:
            continue
        if barged is not None and barged <= cand < barged + BARGE_MAX_CORES:
            continue
        out['next_ticket'] = cand
        break
    return out


def gdb(port, cmds, elf, timeout=180):
    argv = ["aarch64-elf-gdb", "-batch", "-nx"]
    for c in cmds:
        argv += ["-ex", c]
    argv += [elf]
    r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, cwd=REPO)
    return r.stdout + "\n[gdb-stderr]\n" + r.stderr


def sample(port, elf, syms, bkl_base, offs):
    cmds = ["set pagination off", "set confirm off", "set height 0",
            f"target remote localhost:{port}",
            "echo @@THREADS\\n", "info threads",
            "echo @@SYSREGS\\n",
            "thread apply all p/x $ESR_EL1", "thread apply all p/x $ELR_EL1",
            "thread apply all p/x $FAR_EL1", "thread apply all p/x $SPSR_EL1",
            "echo @@REGS\\n", "monitor info registers -a",
            "echo @@STACKS\\n", "thread apply all x/96gx $sp",
            "echo @@LOCKS\\n"]
    # One `x` per field, each behind its own marker. A single `x/6wx` spans two
    # output lines and the second line begins with its own ADDRESS label — a
    # whitespace-tolerant regex happily reads that label as a data word (observed:
    # now_serving = 0x40328db0, i.e. the address of now_serving itself).
    if offs:
        for fname in ("owner", "next_ticket", "now_serving"):
            if fname in offs:
                cmds += [f"echo @@F {fname}\\n", f"x/1wx {hex(bkl_base + offs[fname])}"]
    # The allocator locks are the silent-wedge pair: every core spins on TALC/PMM
    # while (apparently) nobody holds either. Dump their raw bytes so "held with no
    # owner" — an orphaned flag — can be distinguished from a live critical section.
    # spinning_top's RawSpinlock is a single bool: nonzero = held, and it records no
    # owner, so this byte is the ONLY evidence available.
    for sub, label in [("9allocator4TALC", "allocator::TALC (heap lock byte)"),
                       ("3pmm3PMM", "pmm::PMM (page-alloc lock byte)"),
                       ("14COW_FAULT_LOCK", "pmm COW_FAULT_LOCK"),
                       ("21EXT2_WRITE_LOCK_OWNER", "ext2 write-lock owner"),
                       ("34KERNEL_LOCK_LOST_TICKET_RECOVERIES", "lost-ticket recoveries")]:
        a, _ = syms.find(sub)
        if a:
            cmds += [f"echo @@OTHER {label} @ {hex(a)}\\n", f"x/8xb {hex(a)}"]
    cmds.append("detach")
    return gdb(port, cmds, elf)


def decode_esr(val):
    ec = (val >> 26) & 0x3F
    name = EC_NAMES.get(ec, f"EC=0x{ec:02x}")
    if ec in (0x24, 0x25):
        dfsc = val & 0x3F
        wnr = "write" if (val >> 6) & 1 else "read"
        return f"{name}; {DFSC_NAMES.get(dfsc, f'DFSC=0x{dfsc:02x}')}; {wnr}"
    return name


def analyse(raw, syms, bkl_base, offs):
    out = []

    # A dead/absent target must NOT produce a verdict. gdb happily prints zeros for
    # memory it cannot read, so without this guard a probe against a VM that is
    # already gone reports a confident "BKL idle ... TALC free, PMM free" — the exact
    # opposite of the truth, in a file someone reads days later. Observed 2026-08-08.
    if "No threads." in raw or "@@REGS" not in raw or not re.search(r'^CPU#\d', raw, re.M):
        out.append("*** NO TARGET *** gdb attached to nothing (VM already dead, wrong "
                   "port, or stub refused). Every value below is unreadable memory "
                   "defaulting to zero — draw NO conclusions from it.")
        return "\n".join(out)

    fields = {}
    for m in re.finditer(r'@@F (\w+)\s*\n[^:\n]*:\s*(0x[0-9a-f]+)', raw):
        fields[m.group(1)] = int(m.group(2), 16)
    if fields and offs:
        owner = fields.get('owner')
        nxt = fields.get('next_ticket')
        serving = fields.get('now_serving')
        out.append(f"BKL @ {hex(bkl_base)} (offsets {offs}) owner={owner} "
                   f"next_ticket={nxt} now_serving={serving}")
        if owner:
            out.append(f"VERDICT: BKL HELD by core {owner - 1} — inspect that core's ESR/PC below")
        elif nxt is not None and serving is not None and nxt != serving:
            out.append(f"VERDICT: *** LOST TICKET *** free lock, {nxt - serving} waiter(s) unserved")
        else:
            out.append("VERDICT: BKL idle — the wedge is NOT the BKL")
    else:
        out.append("BKL: could not decode")

    # `echo` output can land on the SAME line as the preceding x/ dump, so the byte
    # values may follow the marker on that line or the next. Accept either, and if
    # nothing parses say UNKNOWN — an earlier version defaulted an unparsed value to
    # "HELD", which invented a lock state out of a formatting quirk.
    for m in re.finditer(r'@@OTHER ([^@\n]+?) @ (0x[0-9a-f]+)(.*?)(?=@@|\Z)', raw, re.S):
        label, addr = m.group(1).strip(), m.group(2)
        data = re.findall(r'0x[0-9a-f]{2}\b', m.group(3))
        if 'lock byte' not in label:
            out.append(f"{label} @ {addr}: {' '.join(data[:8]) if data else '<unparsed>'}")
            continue
        if not data:
            out.append(f"{label} @ {addr}: <UNPARSED — state UNKNOWN, do not assume>")
        else:
            held = '  <- HELD' if data[0] not in ('0x00',) else '  <- free'
            out.append(f"{label} @ {addr}: {' '.join(data[:8])}{held}")

    tid2cpu = dict(re.findall(r'^\*?\s*(\d+)\s+Thread \S+ \(CPU#(\d+)',
                              raw.split("@@SYSREGS")[0], re.M))
    sysregs = {}
    sysblk = raw.split("@@SYSREGS")[-1].split("@@REGS")[0]
    for name, chunk in zip(["ESR_EL1", "ELR_EL1", "FAR_EL1", "SPSR_EL1"],
                           re.split(r'(?=Thread \d+ \(Thread)', sysblk)[0:0] or []):
        pass
    for m in re.finditer(r'Thread (\d+) \(Thread[^\n]*\n\$\d+ = (0x[0-9a-f]+)', sysblk):
        sysregs.setdefault(m.group(1), []).append(int(m.group(2), 16))

    for i, (cpu, body) in enumerate(zip(*[iter(re.split(r'^CPU#(\d+)', raw, flags=re.M)[1:])] * 2)):
        out.append(f"\n--- CPU#{cpu} ---")
        pc = re.search(r'PC=([0-9a-f]+)', body)
        if pc:
            s = syms.at(int(pc.group(1), 16))
            out.append(f"  PC = 0x{pc.group(1)}  {s[0]}+{s[1]}" if s else f"  PC = 0x{pc.group(1)}")
        tid = next((t for t, c in tid2cpu.items() if c == cpu), None)
        vals = sysregs.get(tid or '', [])
        if len(vals) >= 3:
            out.append(f"  ESR_EL1=0x{vals[0]:x}  -> {decode_esr(vals[0])}")
            s = syms.at(vals[1])
            out.append(f"  ELR_EL1=0x{vals[1]:x}" + (f"  {s[0]}+{s[1]}" if s else "")
                       + "   (the faulting instruction)")
            out.append(f"  FAR_EL1=0x{vals[2]:x}   (the faulting address)")
        hits = []
        for rm in re.finditer(r'\b(X\d\d|SP|LR)=([0-9a-f]{16})', body):
            v = int(rm.group(2), 16)
            s = syms.at(v)
            if s and not (IMG_LO <= v < 0x40230000):
                hits.append((rm.group(1), v, s))
        if hits:
            out.append("  registers -> named statics (candidate locks):")
            for reg, v, s in hits:
                out.append(f"    {reg}=0x{v:x} -> {s[0]}+{s[1]}")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port")
    ap.add_argument("-n", "--samples", type=int, default=1)
    ap.add_argument("-i", "--interval", type=float, default=8.0)
    ap.add_argument("-o", "--out")
    ap.add_argument("-e", "--elf", default=DEFAULT_ELF)
    a = ap.parse_args()

    syms = Symbols(a.elf)
    bkl_base, _ = syms.find("3bkl11KERNEL_LOCK")
    if bkl_base is None:
        sys.exit("KERNEL_LOCK not found — wrong ELF?")
    offs = bkl_offsets(a.elf, syms)

    chunks, pcsets = [], []
    import time
    for n in range(a.samples):
        raw = sample(a.port, a.elf, syms, bkl_base, offs)
        chunks.append(f"\n{'#' * 70}\n# SAMPLE {n + 1}/{a.samples}\n{'#' * 70}\n"
                      + analyse(raw, syms, bkl_base, offs) + "\n\n[raw]\n" + raw)
        pcsets.append(re.findall(r'PC=([0-9a-f]+)', raw))
        if n + 1 < a.samples:
            time.sleep(a.interval)

    text = "\n".join(chunks)
    if len(pcsets) > 1:
        moved = ["MOVED" if len({p[i] for p in pcsets if i < len(p)}) > 1 else "frozen"
                 for i in range(len(pcsets[0]))]
        text += "\n\n" + "=" * 70 + "\nPC MOVEMENT across samples (per CPU): " + ", ".join(
            f"CPU#{i}={m}" for i, m in enumerate(moved))
        text += ("\n(frozen under -accel hvf is only evidence of a loop if that vCPU is "
                 "still burning host CPU: HVF syncs state on exit, and a "
                 "fault->handler->eret->refault loop restores identical state each pass)")

    if a.out:
        with open(a.out, "w") as f:
            f.write(text)
    print(text)


if __name__ == "__main__":
    main()
