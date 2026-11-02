# Runbooks

Action-first procedures. Each ends with a **Verify** section. If a step stops
working on the current tree, fix the runbook in the same commit or move it to
`docs/archive/` under an `UPPER_SNAKE` name.

| runbook | task |
|---|---|
| [build-and-boot.md](build-and-boot.md) | build the kernel; boot it on the Mac (QEMU TCG) or as the vm-dev guest under kvm0 |
| [gates.md](gates.md) | the TCG rig's battery, the CoW leak gate, the probes, and the ryzen fixed point and census |
