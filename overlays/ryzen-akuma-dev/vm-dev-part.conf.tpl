# The development guest: this kernel (or taoxichuan) booting under Akuma's own hypervisor
# (/dev/kvm0), its root the second NVMe partition IN PLACE (writes are the partition's).
# Rendered by guest-up.sh: @PART@ and @EPOCH@ are filled in; a git-tracked template cannot be
# clobbered by hand edits the way /etc/herd/available/*.conf was (docs/archive/AKUMA_KVM_DEV_CYCLE.md).
# One kvm0 VM at a time. The hypervisor refuses the host's own partition and any raw device
# that is not a GPT partition (akuma-vm-spec).
vm = true
vm_type = kvm
vm_kernel = @KERNEL@
vm_disk = /dev/nvme0n1p@PART@
vm_ram_mib = 1024
vm_vcpus = 2
vm_forward = @SSH_PORT@:22
vm_outbound = true
vm_cmdline = init=/bin/sshd initargs=--port,22 smp=2 epoch=@EPOCH@
restart = false
