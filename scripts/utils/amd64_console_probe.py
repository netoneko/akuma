#!/usr/bin/env python3
"""Does the amd64 console deliver keystrokes to a shell? — local QEMU probe.

Boots `amd64/run.sh` (PVH, `-M microvm`) with the serial line on a pipe, waits
for the kernel to hand the console to `init`, types a few commands and reports
what came back.

What this proves: the console pump -> line discipline -> `/bin/sh` -> echo path,
for whatever `INIT=` selects (`/bin/herd` with a console service, or
`/bin/busybox sh` directly). Input arrives through the 16550, so it exercises
`console::pump_once`, `TerminalState` and the shell, NOT `kbd.rs` (microvm has no
i8042) and NOT the framebuffer.

What it does not prove: a real USB keyboard through the firmware's PS/2
emulation, or anything drawn on a real screen. Those are the metal's claims.

    python3 scripts/utils/amd64_console_probe.py                 # INIT=/bin/herd
    INIT=/bin/busybox python3 scripts/utils/amd64_console_probe.py
    python3 scripts/utils/amd64_console_probe.py --log /tmp/probe.log --boot-timeout 900

Exit status 0 only when every typed command's marker came back. A silent boot
is `NO BOOT`, never a pass.
"""

import argparse
import os
import select
import socket
import subprocess
import sys
import time

# A marker the shell prints and the *typed line* does not contain, so seeing it
# proves the command ran rather than that the echo reflected our own keystrokes.
# `echo AB""CD` types `AB""CD` and prints `ABCD`.
CASES = [
    ('echo AKUMA""PROBE$((6*7))', "AKUMAPROBE42"),
    ("echo $0 | tr a-z A-Z", None),  # informational: which shell answered
    ('ls / | head -3; echo END""MARK', "ENDMARK"),
    # Line editing: type BAD, erase it with DEL (0x7f — what kbd.rs's Backspace
    # sends), type GOOD. Only the edited line can print GOODX.
    ('echo BAD\x7f\x7f\x7fGOOD""X', "GOODX"),
    # `stty rows/cols` on the console must stick (TIOCSWINSZ used to be dropped).
    # The typed line has no "30 90"; only `stty size`'s answer does.
    ("stty rows 30 cols 90; stty size", "30 90"),
    # The console shell sources /etc/console.rc ($ENV): where the printing area is set.
    ('echo RC${AKUMA_CONSOLE_RC}', "RCloaded"),
    ("echo T$TERM", "Txterm-256color"),
]

# `^C` must kill the foreground job and leave the shell alive: start a 100 s
# sleep, interrupt it, and ask for a marker. A dead ISIG path means the marker
# arrives ~100 s later, i.e. never inside --cmd-timeout.
INTERRUPT = ("sleep 100", "echo AFT""ER-INT", "AFTER-INT")


def usb_command(repo, mon_path, fbtrace):
    """The q35 + xHCI command line, after building the kernel and a USB disk.

    The disk is `amd64/mkdisk.sh`'s raw ext2 image behind a 1 MiB MBR gap with
    partition 1 at LBA 2048 — the layout `xhci::mbr_looks_right` demands, i.e.
    the trashcan's. The USB disk is declared **before** the keyboard so it lands
    on the lower root-hub port: `find_and_reset_port` takes the first connected
    port, and a keyboard there would be tried as the disk.
    """
    subprocess.run(["cargo", "build", "-p", "akuma-amd64", "--target", "x86_64-unknown-none", "--release"],
                   cwd=repo, check=True)
    ext2 = os.path.join(repo, "target/x86_64-unknown-none/release/amd64-root.img")
    subprocess.run(["sh", "amd64/mkdisk.sh", ext2, "128"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
    usb = os.path.join(repo, "target/x86_64-unknown-none/release/amd64-usb.img")
    with open(ext2, "rb") as f:
        body = f.read()
    mbr = bytearray(512)
    mbr[446 + 4] = 0x83                                   # Linux
    mbr[446 + 8:446 + 12] = (2048).to_bytes(4, "little")  # start LBA
    mbr[446 + 12:446 + 16] = (len(body) // 512).to_bytes(4, "little")
    mbr[510:512] = b"\x55\xaa"
    with open(usb, "wb") as f:
        f.write(mbr)
        f.write(b"\0" * (2048 * 512 - 512))
        f.write(body)
    kernel = os.path.join(repo, "target/x86_64-unknown-none/release/akuma-amd64")
    return [
        "qemu-system-x86_64", "-M", "q35,i8042=off", "-cpu", "max", "-m", "2048", "-smp", "1",
        "-kernel", kernel, "-append", "init=/bin/herd pci usbroot fbtrace",
        "-device", "qemu-xhci,id=xhci",
        "-drive", f"id=u0,file={usb},if=none,format=raw",
        "-device", "usb-storage,bus=xhci.0,drive=u0",
        "-device", "usb-kbd,bus=xhci.0",
        "-chardev", f"file,id=fbtrace,path={fbtrace}",
        "-device", "isa-debugcon,iobase=0xe9,chardev=fbtrace",
        "-serial", "mon:stdio", "-display", "none", "-no-reboot",
        "-monitor", f"unix:{mon_path},server,nowait",
    ]


def run_kbd(args, mon_path, buf, pump, wait_for) -> int:
    """Type with `sendkey` and require the shell to answer.

    Every key goes through QEMU's emulated i8042 as a set-1 scancode, so what is
    exercised is `kbd.rs`'s decode (shift, ctrl, backspace) and the console pump —
    the part of the real keyboard path that does not depend on firmware.
    """
    time.sleep(1)
    mon = socket.socket(socket.AF_UNIX)
    mon.connect(mon_path)
    mon.settimeout(2)
    try:
        mon.recv(4096)
    except OSError:
        pass

    def keys(*names):
        for n in names:
            mon.sendall(f"sendkey {n}\n".encode())
            time.sleep(0.15)
            try:
                mon.recv(4096)
            except OSError:
                pass

    def text(t):
        out = []
        for ch in t:
            out.append({" ": "spc", "$": "shift-4", "(": "shift-9", ")": "shift-0",
                        "*": "shift-8", "-": "minus"}.get(ch, ch))
        keys(*out)

    failures = 0

    def expect(label, marker, budget=40):
        nonlocal failures
        ok = wait_for(marker, budget, mark) >= 0
        print(f"  {'ok  ' if ok else 'FAIL'}  {label}")
        failures += 0 if ok else 1

    # `$((6*7))` — the typed line cannot contain 42, so only the shell can print it.
    mark = len(buf)
    text("echo $((6*7))")
    keys("ret")
    expect("typed `echo $((6*7))` on the keyboard -> 42", b"\n42")
    # Backspace (0x7f): type 99, erase both, type 5*9 -> 45.
    mark = len(buf)
    text("echo $((99")
    keys("backspace", "backspace")
    text("5*9))")
    keys("ret")
    expect("Backspace erases (echo $((99<BS><BS>5*9)) -> 45)", b"\n45")
    # ^C: sleep 100, interrupt, then a command must still run.
    mark = len(buf)
    text("sleep 100")
    keys("ret")
    time.sleep(3)
    keys("ctrl-c")
    time.sleep(1)
    text("echo $((6*8))")
    keys("ret")
    expect("Ctrl-C interrupts sleep 100 and the shell survives -> 48", b"\n48")
    # Up arrow = shell history: run once, press Up + Enter, the answer comes again.
    # The recalled line cannot contain "\n54", only a second execution can.
    text("echo $((6*9))")
    keys("ret")
    time.sleep(2)
    mark = len(buf)
    keys("up")
    keys("ret")
    expect("Up arrow recalls the previous command (history) -> 54 again", b"\n54")
    # Left arrow edits mid-line: type `echo $((9*9)`, Left, `)` -> `echo $((9*9))`.
    mark = len(buf)
    text("echo $((9*9)")
    keys("left")
    text(")")
    keys("ret")
    expect("Left arrow edits inside the line -> 81", b"\n81")
    # Software repeat: type a junk line, hold Backspace long enough to clear all of
    # it, then type a fresh command. Without repeat only ONE character is erased,
    # the junk stays on the line, and the answer is not a bare "56".
    mark = len(buf)
    text("echo zzzzzzzzzzzzzzzzzzzz")
    mon.sendall(b"sendkey backspace 2500\n")
    time.sleep(3.5)
    text("echo $((7*8))")
    keys("ret")
    expect("holding Backspace repeats and clears the line -> 56", b"\n56")
    raw = bytes(buf)
    print(f"  info  [kbd] scancode lines in the serial log: {raw.count(b'[kbd] sc=')}")
    print("RESULT:", "PASS" if failures == 0 else f"FAIL ({failures})")
    return 0 if failures == 0 else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", default="/tmp/amd64-console-probe.log")
    ap.add_argument("--boot-timeout", type=int, default=600)
    ap.add_argument("--cmd-timeout", type=int, default=60)
    ap.add_argument("--ready", default="-- running ", help="line that means init started")
    ap.add_argument("--kbd", action="store_true",
                    help="type through an emulated i8042 (QEMU `sendkey`) instead of the serial port: "
                         "exercises kbd.rs scancode decode + the pump, not the firmware")
    ap.add_argument("--usb", action="store_true",
                    help="boot q35 with a USB root disk and a USB keyboard (qemu-xhci), the way the "
                         "trashcan boots; implies --kbd. Exercises the native xHCI keyboard driver")
    ap.add_argument("--settle", type=float, default=8.0, help="seconds to let init print before typing")
    args = ap.parse_args()

    repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    env = dict(os.environ)
    env.setdefault("INIT", "/bin/herd")
    # What the framebuffer would show, via QEMU's port-0xE9 debug console (this
    # machine has no framebuffer; see `serial::set_fb_quiet`).
    fbtrace = args.log + ".fbtrace"
    if os.path.exists(fbtrace):
        os.remove(fbtrace)
    env["FBTRACE"] = fbtrace
    if args.usb:
        args.kbd = True
    extra = []
    # AF_UNIX paths are capped near 104 bytes, so not next to the log.
    mon_path = f"/tmp/akmon{os.getpid()}.sock"
    if args.kbd:
        if os.path.exists(mon_path):
            os.remove(mon_path)
        extra = ["-device", "i8042", "-monitor", f"unix:{mon_path},server,nowait"]
    if args.usb:
        cmd = usb_command(repo, mon_path, fbtrace)
    else:
        cmd = ["sh", "amd64/run.sh", *extra]
    p = subprocess.Popen(
        cmd, cwd=repo, env=env,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    log = open(args.log, "wb")
    buf = bytearray()

    def pump(timeout: float) -> bool:
        r, _, _ = select.select([p.stdout], [], [], timeout)
        if not r:
            return False
        chunk = os.read(p.stdout.fileno(), 65536)
        if not chunk:
            return False
        log.write(chunk)
        log.flush()
        buf.extend(chunk)
        return True

    def wait_for(needle: bytes, budget: float, start: int = 0) -> int:
        end = time.time() + budget
        while time.time() < end:
            i = buf.find(needle, start)
            if i >= 0:
                return i
            if p.poll() is not None and not pump(0.2):
                return -1
            pump(0.5)
        return buf.find(needle, start)

    try:
        if wait_for(args.ready.encode(), args.boot_timeout) < 0:
            print(f"NO BOOT: never saw {args.ready!r} (log: {args.log})")
            return 2
        print(f"init started after the kernel's own line; log: {args.log}")
        time.sleep(args.settle)
        while pump(0.5):
            pass
        failures = 0
        if args.kbd:
            return run_kbd(args, mon_path, buf, pump, wait_for)
        for line, marker in CASES:
            mark = len(buf)
            p.stdin.write(line.encode().decode("unicode_escape").encode("latin1") + b"\r")
            p.stdin.flush()
            if marker is None:
                time.sleep(3)
                while pump(0.5):
                    pass
                print(f"  info  {line!r} -> {bytes(buf[mark:])[-80:]!r}")
                continue
            # The marker must appear AFTER the echo of what we typed.
            i = wait_for(marker.encode(), args.cmd_timeout, mark)
            ok = i >= 0
            print(f"  {'ok  ' if ok else 'FAIL'}  {line!r} -> {marker!r}")
            failures += 0 if ok else 1
        # ^C.
        job, after, marker = INTERRUPT
        p.stdin.write(job.encode() + b"\r")
        p.stdin.flush()
        time.sleep(4)
        mark = len(buf)
        p.stdin.write(b"\x03")
        p.stdin.flush()
        time.sleep(1)
        p.stdin.write(after.encode() + b"\r")
        p.stdin.flush()
        ok = wait_for(marker.encode(), args.cmd_timeout, mark) >= 0
        print(f"  {'ok  ' if ok else 'FAIL'}  ^C interrupts `{job}` and the shell survives")
        failures += 0 if ok else 1
        # The TV filter. herd prints `[herd] Reloading config...` every 20 s, so
        # wait for one more after the prompt: it must be in the serial log (it
        # is what there is to hide) and absent from what the "TV" received.
        # Everything the shell printed must be on the TV.
        start = len(buf)
        wait_for(b"[herd] Reloading config", 45, start)
        time.sleep(1)
        while pump(0.3):
            pass
        tv = open(fbtrace, "rb").read()
        prompt = tv.find(b"/ # ")
        after = tv[prompt:] if prompt >= 0 else b""
        serial_after = bytes(buf[buf.find(b"/ # "):]) if b"/ # " in buf else b""
        ok = prompt >= 0
        print(f"  {'ok  ' if ok else 'FAIL'}  TV received the shell prompt")
        failures += 0 if ok else 1
        ok = b"[herd] Reloading config" in serial_after
        print(f"  {'ok  ' if ok else 'FAIL'}  control: serial log has herd chatter after the prompt")
        failures += 0 if ok else 1
        for tag in (b"[herd]", b"[BKL]", b"[bkls>]", b"[PSTATS]", b"[probe]"):
            ok = tag not in after
            print(f"  {'ok  ' if ok else 'FAIL'}  TV has no {tag.decode()} after the prompt")
            failures += 0 if ok else 1
        for m in (b"AKUMAPROBE42", b"GOODX", b"AFTER-INT"):
            ok = m in after
            print(f"  {'ok  ' if ok else 'FAIL'}  TV shows shell output {m.decode()}")
            failures += 0 if ok else 1
        print("RESULT:", "PASS" if failures == 0 else f"FAIL ({failures})")
        return 0 if failures == 0 else 1
    finally:
        if args.kbd and os.path.exists(mon_path):
            os.remove(mon_path)
        p.terminate()
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.kill()


if __name__ == "__main__":
    sys.exit(main())
