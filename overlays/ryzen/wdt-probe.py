#!/usr/bin/env python3
"""Read-only probe of the AMD FCH watchdog — run as root on ryzen, from Pop.

Reads (never writes) the registers Linux's `sp5100_tco` uses on an "EFCH"
(family 17h+) chipset, through /dev/mem: the PM block at 0xFED80300 and the
watchdog itself at 0xFED80B00. Answers, before any kernel code is written:
is the watchdog decoded, has firmware disabled it, what resolution and action
is it set to, and did it fire last boot.

Register meanings follow drivers/watchdog/sp5100_tco.{c,h}:
  PM 0x00 DECODEEN   bit 7  WDT_TMREN     watchdog timer MMIO decode enabled
  PM 0x03 DECODEEN3  bits 3:2 WATCHDOG_DISABLE (both set = disabled by firmware)
                     bits 1:0 resolution (3 = 1 s units)
  PM 0x04 ISACONTROL bit 1  MMIOEN         ACPI MMIO block (0xFED80000) decoded
  WDT +0 CONTROL     bit 0 RUN, bit 1 FIRED, bit 2 ACTION (1 = power off,
                     0 = reset), bit 3 DISABLED, bit 7 TRIGGER
  WDT +4 COUNT       low 16 bits, in resolution units
"""
import mmap
import os
import struct

BASE = 0xFED80000
PM = 0x300
WDT = 0xB00

fd = os.open("/dev/mem", os.O_RDONLY | os.O_SYNC)
m = mmap.mmap(fd, 0x1000, mmap.MAP_SHARED, mmap.PROT_READ, offset=BASE)


def b(off):
    return m[off]


def d(off):
    return struct.unpack("<I", m[off:off + 4])[0]


decodeen, decodeen3, isactl = b(PM + 0x00), b(PM + 0x03), b(PM + 0x04)
ctl, cnt = d(WDT + 0x00), d(WDT + 0x04)
print(f"PM DECODEEN  = 0x{decodeen:02x}  WDT_TMREN={(decodeen >> 7) & 1}")
print(f"PM DECODEEN3 = 0x{decodeen3:02x}  WATCHDOG_DISABLE={(decodeen3 >> 2) & 3} (3 = disabled) resolution={decodeen3 & 3} (3 = 1 s)")
print(f"PM ISACONTROL= 0x{isactl:02x}  MMIOEN={(isactl >> 1) & 1}")
print(f"WDT CONTROL  = 0x{ctl:08x}  run={ctl & 1} fired={(ctl >> 1) & 1} action={'poweroff' if (ctl >> 2) & 1 else 'reset'} disabled={(ctl >> 3) & 1}")
print(f"WDT COUNT    = 0x{cnt:08x}  ({cnt & 0xffff})")
if ctl == 0xFFFFFFFF:
    print("verdict: WDT block reads all-ones — not decoded")
elif (decodeen3 >> 2) & 3 == 3:
    print("verdict: DISABLED by firmware (DECODEEN3.WATCHDOG_DISABLE) — enabling is a PM write")
elif not (decodeen >> 7) & 1:
    print("verdict: idle — decode off (WDT_TMREN=0), so CONTROL reads DISABLED; Akuma's `wdt` (or Linux's sp5100_tco) enables it")
else:
    print("verdict: present and enabled")
if (ctl >> 1) & 1 and ctl != 0xFFFFFFFF:
    print("note: FIRED=1 — the last reset was this watchdog")
