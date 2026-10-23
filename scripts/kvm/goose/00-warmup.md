# Task 00: prove the dev loop (no code changes)

You are working INSIDE Akuma on bare metal (the ryzen). Read `docs/archive/AKUMA_KVM_DEV_CYCLE.md` in the repo
(`/src/github.com/netoneko/akuma`) first: it is your manual. Do NOT use Firecracker; use `/dev/kvm0` guests started through herd.

Every shell you start has no environment: begin each command line with `. /etc/akuma-dev.env;` and use `export TERM=dumb GIT_PAGER=cat`.

Do exactly this, in order, and write what happened (commands, exit codes, wall times, the useful lines of output) to
`/root/dev-cycle/warmup-report.md` as you go:

1. `cd /src/github.com/netoneko/akuma && git status -sb && git log -3 --format='%h %s'`. You should be on branch `dev-base`.
   Create your branch: `git checkout -b cmos-fix`.
2. Build the kernel: `kbuild --features no-tests > /root/dev-cycle/kbuild.log 2>&1; echo rc=$?`. It is incremental and takes
   one to fifteen minutes (`-j1` is the default and the safe choice). Do not run `cargo clean`. If it fails, read the log and
   fix the cause of the failure only if it is in the environment, not in the source; report it either way.
3. Copy the result next to the other guest kernels: `cp /root/ktarget/x86_64-unknown-none/release/akuma-amd64 /boot/akuma-guest-dev`
   (never overwrite `/boot/akuma-guest`, `/boot/akuma-amd64` or `/boot/akuma-amd64.verify` in this task).
4. Make a writable root image for the test guest: `cp /boot/akuma-guest-root.img /boot/akuma-dev-root.img`.
5. Create the herd service file `/etc/herd/available/vm-dev.conf` with exactly:
   ```
   vm = true
   vm_type = kvm
   vm_kernel = /boot/akuma-guest-dev
   vm_disk = /boot/akuma-dev-root.img
   vm_ram_mib = 768
   vm_vcpus = 1
   vm_cmdline = init=/bin/busybox initargs=sh,/etc/guest.sh
   restart = false
   ```
6. Only one `/dev/kvm0` VM can exist: `herd stop vm-netbsd; herd stop vm-serve-mc` (ignore "not running"), then `herd enable vm-dev` (herd only starts services it has enabled) and `herd start vm-dev`.
   Wait up to three minutes, polling `grep -a -E 'AKUMA-GUEST|EXCEPTION|panic' /var/log/herd/vm-dev.log` every ten seconds.
   PASS = the log shows `AKUMA-GUEST-UP` and `AKUMA-GUEST-DONE` and the md5 `ebce43017d2cb316ea45e08374de7315  /bin/busybox`.
   Also run `dmesg | grep -a -E 'EXCEPTION|panic' | head` and report whether it is empty.
7. `herd stop vm-dev; herd disable vm-dev` (an enabled `vm-dev` would start at every boot and take the `kvm0` slot from
   `vm-serve-mc`), then restore: `herd start vm-serve-mc`.
8. Finish by writing `PASS` or `FAIL: <why>` as the last line of `/root/dev-cycle/warmup-report.md`, and stop. Do not reboot the
   machine, do not edit any source file, do not commit anything in this task.
