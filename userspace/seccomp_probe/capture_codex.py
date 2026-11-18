#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Capture an installed Codex's amd64 cBPF program on Linux."""

import argparse
import ctypes
import os
import platform
import shutil
import signal
import struct

PTRACE_TRACEME = 0
PTRACE_GETREGS = 12
PTRACE_SYSCALL = 24
PTRACE_SETOPTIONS = 0x4200
PTRACE_O_TRACESYSGOOD = 1
PTRACE_O_TRACEEXEC = 16
PTRACE_O_EXITKILL = 1 << 20
SYS_SECCOMP = 317
SET_MODE_FILTER = 1
ENOSYS = 38
PROFILE = (
    '{"type":"managed","file_system":{"type":"restricted","entries":'
    '[{"path":{"type":"special","value":{"kind":"root"}},"access":"read"}]},'
    '"network":"restricted"}'
)

libc = ctypes.CDLL(None, use_errno=True)
libc.ptrace.restype = ctypes.c_long


def ptrace(request, pid, data=0):
    ctypes.set_errno(0)
    result = libc.ptrace(request, pid, ctypes.c_void_p(0), ctypes.c_void_p(data))
    if result == -1 and (error := ctypes.get_errno()):
        raise OSError(error, os.strerror(error))


def capture(codex):
    child = os.fork()
    if child == 0:
        ptrace(PTRACE_TRACEME, 0)
        os.kill(os.getpid(), signal.SIGSTOP)
        os.execv(
            codex,
            [
                "codex-linux-sandbox",
                "--sandbox-policy-cwd", "/tmp",
                "--command-cwd", "/tmp",
                "--permission-profile", PROFILE,
                "--apply-seccomp-then-exec", "--", "/bin/true",
            ],
        )

    os.waitpid(child, 0)
    ptrace(PTRACE_SETOPTIONS, child, PTRACE_O_TRACESYSGOOD | PTRACE_O_TRACEEXEC | PTRACE_O_EXITKILL)
    deliver = 0
    program = None
    while True:
        ptrace(PTRACE_SYSCALL, child, deliver)
        _, status = os.waitpid(child, 0)
        if not os.WIFSTOPPED(status):
            if status != 0 or program is None:
                raise RuntimeError(f"one successful filter installation expected, wait status {status}")
            return program

        deliver = os.WSTOPSIG(status) if status >> 16 == 0 else 0
        if deliver != (signal.SIGTRAP | 0x80):
            continue
        deliver = 0
        regs = (ctypes.c_uint64 * 27)()
        ptrace(PTRACE_GETREGS, child, ctypes.addressof(regs))
        rax, rdx, rsi, rdi, orig_rax = regs[10], regs[12], regs[13], regs[14], regs[15]
        if (orig_rax, rdi, rsi, rax) != (SYS_SECCOMP, SET_MODE_FILTER, 0, (1 << 64) - ENOSYS):
            continue
        if program is not None:
            raise RuntimeError("multiple filters installed")
        with open(f"/proc/{child}/mem", "rb", buffering=0) as mem:
            mem.seek(rdx)
            count, instructions = struct.unpack("<H6xQ", mem.read(16))
            mem.seek(instructions)
            program = mem.read(count * 8)
        if not 1 <= count <= 4096 or len(program) != count * 8:
            raise RuntimeError(f"invalid sock_fprog at {rdx:#x}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=argparse.FileType("xb"))
    parser.add_argument("--codex", default=shutil.which("codex"))
    args = parser.parse_args()
    if platform.system() != "Linux" or platform.machine() != "x86_64" or os.geteuid() == 0:
        parser.error("run as an unprivileged user on amd64 Linux")
    if args.codex is None:
        parser.error("Codex was not found")
    result = capture(args.codex)
    args.output.write(result)
    print(f"Captured {len(result) // 8} instructions")
