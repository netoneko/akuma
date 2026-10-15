# Gentoo as box 0 on ryzen

- `akuma-init.sh` — `/akuma-init.sh` on p7: the kernel's `init=/usr/bin/bash
  initargs=/akuma-init.sh` runs it. wifi, OpenRC, Akuma's sshd on 2222, OpenSSH
  on 4444, diagnostics in `/var/log/{wifi,net.txt,dmesg.txt}`.
- `cycle_gentoo.py` — arm menu entry 17 and boot it unattended, retrying.

Menu entry: `overlays/ryzen/grub.cfg` (entry 17). Partition p7 and the whole story:
`docs/archive/AKUMA_GENTOO.md`.
- `deploy.py [entry] [--init]` — install the freshly built kernel (and with
  `--init` the `akuma-init.sh` here) and boot the entry; reboots the box to Pop
  first if it is up. Entry 18 is entry 17 + `strace_err`.
