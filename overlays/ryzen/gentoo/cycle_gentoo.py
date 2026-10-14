"""Boot the Gentoo root on ryzen (menu entry 17) unattended, retrying.

Arms entry 17 from Pop, reboots, and waits for Akuma's sshd (2222) and
OpenSSH (4444) on the Gentoo box. If the box bounces back to Pop (the init
script reboots when the radio never joins: see akuma-init.sh), it re-arms and
tries again, up to 8 times. Needs Pop reachable through scripts/utils/hpbox.py.
Docs: docs/archive/AKUMA_GENTOO.md.
"""
import sys, time, socket, base64
import os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..', '..', 'scripts', 'utils'))
import hpbox
ARM='''D=/boot/efi/EFI/akuma
grub-editenv $D/grubenv set next_entry=17
bootctl set-oneshot akuma.conf
sync
(sleep 2; systemctl reboot) >/dev/null 2>&1 &
'''
def up(port):
    s=socket.socket(); s.settimeout(1)
    try: s.connect(("192.168.1.159",port)); return True
    except Exception: return False
    finally: s.close()
b=base64.b64encode(ARM.encode()).decode()
for attempt in range(1,9):
    r=hpbox.ryzen_root(f"echo {b} | base64 -d > /root/arm.sh; sh /root/arm.sh")
    print("attempt",attempt,"armed",r[0],flush=True)
    t0=time.time(); ok=False
    time.sleep(20)
    while time.time()-t0<330:
        if up(2222) and up(4444): ok=True; break
        # bounced back to Pop?
        if time.time()-t0>60 and hpbox.ryzen_root('true')[0]==0: break
        time.sleep(5)
    print("  network up:",ok,int(time.time()-t0),"s",flush=True)
    if ok: break
    # wait for Pop to be back before re-arming
    while hpbox.ryzen_root('true')[0]!=0: time.sleep(8)
