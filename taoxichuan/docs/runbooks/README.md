# Runbooks

Action-first procedures. Each ends with a **Verify** section. If a step stops
working on the current tree, fix the runbook in the same commit or move it to
`docs/archive/` under an `UPPER_SNAKE` name.

| runbook | task |
|---|---|
| [build-and-boot.md](build-and-boot.md) | build the kernel, install it as the vm-dev guest, boot it under kvm0 |
| [gates.md](gates.md) | run the stage gates and read their console evidence |
