# Direct EC2 bootstrap

This overlay targets a future direct-boot EC2 image, separate from the
Firecracker-on-AWS host overlay. The kernel must start `/bin/herd` as init;
herd runs `sshd` and the `instance-key` service. The service fetches the
Ed25519 launch key from instance metadata and installs it for sshd. sshd reads
the key file on login, so the services need no ordering. This is a metadata
bootstrap, not an implementation of cloud-init user-data.

Run `python3 -m unittest discover -s overlays/aws/tests -v` for local IMDSv2
tests. For a different test server, set `METADATA_URL` to `http://HOST:PORT`,
`AUTHORIZED_KEYS` to a disposable path, and optionally `INSTANCE_KEY_TRIES`.

For a VM test, build the amd64 kernel and provide an existing Akuma amd64 ext2
disk containing herd, sshd and busybox (for example one made with
`amd64/mkdisk.sh`):

```sh
cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless,no-tests
python3 overlays/aws/tests/e2e_vm.py --disk /path/to/akuma-amd64.img
```

The test copies the disk, installs this overlay, injects a metadata URL into
the herd service, starts a host metadata server and boots QEMU. It checks SSH
login with the metadata key and rejection of an unrelated key. The test server
uses IMDSv1 by default; `--imds-v2` requires a token and checks apk and curl
in the guest. It exercises virtio-net, not ENA or EC2 firmware. It requires
QEMU, debugfs, ssh and ssh-keygen.

## Stage apk and curl

`stage-rootfs.sh BASE_ROOT NEW_DEST x86_64|aarch64` creates a new directory
containing the AWS overlay, Alpine apk, curl, their dependencies, package keys
and CA certificates. It requires Docker and network access. The base must
already contain Akuma's herd, sshd and shell. The destination must not exist.
The script clears baked SSH keys. It stages a filesystem tree, not an
EC2-bootable disk or AMI.

```sh
overlays/aws/stage-rootfs.sh /path/to/base-root /path/to/new-root x86_64
truncate -s 1G /tmp/akuma-aws.img
mkfs.ext2 -F -b 4096 -d /path/to/new-root /tmp/akuma-aws.img
python3 overlays/aws/tests/e2e_vm.py --disk /tmp/akuma-aws.img --imds-v2
```

Choose a disk size large enough for the staged tree. On 2026-11-17 this path
passed on an x86_64 QEMU guest using an existing amd64 base disk as the source
tree: herd started, the guest obtained a launch key with an IMDSv2 token,
accepted that key over SSH, rejected another key, and ran the two version
commands. This does not establish that apk can install packages in the guest,
or that the AArch64 staging or direct EC2 boot has been tested.

Build an ext2 disk from the staged tree and add `--imds-v2` to the VM test to
require a metadata token and execute `apk --version` and `curl --version`
inside the guest after login.

The tree includes Akuma and AWS ENA license notices in `/usr/share/licenses`,
and `alpine-installed-packages` records the Alpine package versions, upstream
URLs and license identifiers. **That inventory is not a complete third-party
notice or source distribution.** Before distributing an AMI, collect the
copyright/license notices and corresponding source or other required materials
for every installed package and the supplied base image. In particular, Alpine's
apk-tools is GPL-2.0-only; the package database alone does not satisfy its terms.

An AMI builder must turn the staged tree into a bootable image and select herd
as init. ENA and direct EC2 boot remain under development.
