# Taoxichuan stage 6, session C: get `apk.static add <file.apk>` to install offline (G3, G4)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Session B
(`taoxichuan-06b-spawn-then-apk.md`, your `RESULT-B.md` in `/root/dev-cycle/taoxichuan-s6/`) found and fixed the four layers
behind "sshd never spawned an external binary" and got G1 and G2 green. This session finishes G3 and G4. **Keep your context
small** (a fresh context costs about a tenth of a 220k one per call; quota is shared and finite). The log section "Stage 6 —
session B" in `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (on `origin`, merged into your branch tree if you
`git fetch`; otherwise this prompt is enough) has the details.

Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Box log timestamps are UTC (IDT - 3 h). Boot dance, fresh image
copy of `/boot/akuma-dev-root-apk.img.orig` before every boot, `vm-dev.conf` restore, stopping `vm-dev` at the end: as in 06b.

## The gates (reconstructed from `/pkgs/README` in the image; `EXPECTED.txt` has the hashes)

- G1: `ssh -p 2224 root@localhost 'sh -c "echo hi"'` -> `hi` from a real `/bin/sh`. (PASS in B.)
- G2: `apk.static --version` -> `apk-tools 3.0.8-r0, compiled for x86_64.` (PASS in B.)
- G3: install `/pkgs/ca-certificates-bundle-20261020-r0.apk` into `/` offline, exit 0.
- G4: `/etc/ssl/certs/ca-certificates.crt` has the sha256 in `EXPECTED.txt` (`5961c0a3...3f568b`) and the three symlinks listed
  there exist. A passing boot has zero `Fault|PANIC|sig!`.

## What the supervisor learned on the Mac (real Linux, same Alpine 3.23 `apk.static` 3.0.8-r0 and same package)

- `apk.static --root R --initdb --no-network --no-cache --allow-untrusted --force-non-repository add /pkgs/ca-certificates-bundle-20261020-r0.apk`
  installs and exits 0; `add ./ca-certificates-bundle-20261020-r0.apk` (after `cd /pkgs`) does too. **`--force-non-repository`
  is required with `--no-cache`**; without it apk refuses ("non-repository package ... lost on next reboot"). Use it in G3.
  A real file path as an `add` argument is documented and supported; "add by name" without an index fails on Linux too, so
  do not chase it.
- Your last failure was `unable to select packages: apk.static (no such package)`. The name apk complained about is its
  **own program name**, not the path. So the positional list apk's parser saw after `add` contained `apk.static`: suspect
  the argv the new image receives (an off-by-one in `build_stack` / the exec-replace argv copy, argv[0] repeated, or
  `argc` too large by one). `--version` ignores positionals, which is why G2 passed. **First experiment: print the new
  image's argc/argv from the kernel at `child_enter` (names, not just numbers) and compare with what ash passed to
  `execve`.** If argv is right, then look at what apk does with its positional: it `stat`s/`open`s the path first, and an
  open failure makes it treat the argument as a package name (but then the name in the error would be the path, so argv
  is the better suspect).
- The same package on the stock aarch64 kernel installs fine in both path forms, so the file, apk and the syscall
  semantics are not the problem: this is something this kernel does.

## Order of work

1. argv dump at `child_enter` / `exec_replace`; fix what it shows. One boot should tell you.
2. G3 with `--force-non-repository`. Fix whatever it hits next (add the `access`(21) arm that 4 calls per run need; ENOSYS
   is tolerated by apk but add it). Run only `apk.static` commands needed for the gate; do not explore.
3. Remove the temporary per-syscall child trace (`[sc.c] ...` at the top of `dispatch`) and the `child_enter` trace print
   before the clean G3/G4 boot, and run the gate once more without them. Cosmetic, only if cheap: sshd logs
   `Unknown argument: /bin/sshd` at boot (argv[0] leaking into its option parse).
4. G4: verify the sha256 and symlinks from inside the guest (`sha256sum` and `ls -l` over ssh), compare with `EXPECTED.txt`.

Rules as before: commit and `git push litter taoxichuan-s4` at least every 10 minutes, zero faults in a passing boot, never
push `origin`, gate arch-specific code (`cfg`) and keep amd64-only crates out of aarch64 default-members, **stop `vm-dev` when
you finish**.

## When to stop

Stop when G3 and G4 pass in one clean boot, when a gate fails in a way you cannot see past in two boots, when the quota
error appears, or after about 60 minutes. Then write `/root/dev-cycle/taoxichuan-s6/RESULT-C.md` (gates, the exact failing
step with console evidence, what the argv problem turned out to be, what is missing) and end the turn.
