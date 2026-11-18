#!/usr/bin/env python3
"""Convert Akuma's raw Ed25519 key file to a standard OpenSSH private key.

Akuma's sshd keeps its key (`/etc/sshd/id_ed25519`) as the **raw 32-byte Ed25519 seed**
(`userspace/sshd/src/keys.rs`); its `ssh` client reads that format or a standard unencrypted OpenSSH key
(`userspace/sshd/src/client/keys.rs`). This writes the standard form, so the same identity can be used by OpenSSH here.

    akuma_key_to_openssh.py <raw-key-file | -> <out> [--force]

  <out>        e.g. ~/.ssh/late_sh.ed25519; written mode 0600, and <out>.pub beside it (mode 0644)
  --force      overwrite an existing <out> (refused otherwise)

A file that already is an OpenSSH private key is copied as it is. The key itself is never printed, only the public key.
"""
import os
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


def load(data: bytes) -> Ed25519PrivateKey:
    if data.lstrip().startswith(b"-----BEGIN OPENSSH PRIVATE KEY-----"):
        key = serialization.load_ssh_private_key(data, password=None)
        if not isinstance(key, Ed25519PrivateKey):
            sys.exit("not an Ed25519 key")
        return key
    if len(data) != 32:
        sys.exit(f"expected Akuma's raw 32-byte seed or an OpenSSH private key, got {len(data)} bytes")
    return Ed25519PrivateKey.from_private_bytes(data)


def main() -> None:
    args = [a for a in sys.argv[1:] if a != "--force"]
    force = "--force" in sys.argv[1:]
    if len(args) != 2:
        sys.exit(__doc__)
    src, out = args[0], os.path.expanduser(args[1])
    data = sys.stdin.buffer.read() if src == "-" else open(src, "rb").read()
    key = load(data)
    private = key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.OpenSSH, serialization.NoEncryption()
    )
    public = key.public_key().public_bytes(serialization.Encoding.OpenSSH, serialization.PublicFormat.OpenSSH)
    comment = os.path.basename(out)
    if os.path.exists(out) and not force:
        sys.exit(f"{out} exists; pass --force to replace it")
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    fd = os.open(out, flags, 0o600)  # 0600 from creation, never wider
    with os.fdopen(fd, "wb") as f:
        f.write(private)
    os.chmod(out, 0o600)
    with open(out + ".pub", "wb") as f:
        f.write(public + b" " + comment.encode() + b"\n")
    os.chmod(out + ".pub", 0o644)
    print(f"wrote {out} (0600) and {out}.pub")
    print((public + b" " + comment.encode()).decode())


if __name__ == "__main__":
    main()
