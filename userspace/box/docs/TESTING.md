# Box Test Suite

Two layers, split by what each can actually answer.

| | Host (`cargo test`) | Target (`box test`) |
|---|---|---|
| Runs | `boxlib` — JSON, refs, manifests, paths, argv, `/proc/boxes` | The same code built for `aarch64-unknown-none`, plus HTTPS |
| Needs | A host toolchain, seconds | A booted VM; `--net` needs the network |
| Catches | Logic bugs, in the second you make them | Target/libakuma/TLS-specific breakage, registry drift |

Anything that is a decision over strings and bytes belongs in the host layer.
The on-target suite is deliberately small — it exists for what a host cannot see.

## Host tests

```bash
cd userspace
cargo test -p box --lib --no-default-features \
    --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

`--no-default-features` drops `libakuma` (and `libakuma-tls`, `akuma-tar`), whose
`#[panic_handler]` and `#[global_allocator]` collide with std's — that is the
whole reason the crate is split into a `boxlib` library and a `box` binary. Same
arrangement as `userspace/sshd`. `--lib` keeps cargo from trying to build the
binary, which always needs those crates.

Coverage, by module:

- **`json.rs`** — the path-addressed layer over `picojson`: walk order, array
  indices, sibling keys, escapes (including `\uXXXX`), braces inside strings,
  malformed input, wildcard vs literal-index patterns, nesting depth. The key
  property under test is that `["config", "Cmd", "*"]` reaches only the
  *top-level* `config` — an image config's `container_config` has the same
  member names and holds the build's last command, not the image's.
- **`oci_ref.rs`** — reference parsing (`busybox`, `ubuntu:22.04`,
  `user/app:v1`, `ghcr.io/o/r`, `localhost:5000/img:dev` — a port is not a tag)
  and the registry URLs each produces.
- **`manifest.rs`** — manifest-list detection (including the no-`mediaType`
  case), `linux/arm64` selection, attestation entries skipped, and the
  correlation rule: architecture and os must come from the *same* array element,
  or a list containing amd64/linux and arm64/plan9 selects a manifest that runs
  on neither.
- **`paths.rs`** — store layout, store-name canonicalisation (`busybox` =
  `library/busybox` = `docker.io/library/busybox`), the layers file round trip,
  and overlay order being the reverse of registry order.
- **`spec.rs`** — image config → `ImageProcess`, docker's argv rules
  (user args replace **Cmd**, keep the **Entrypoint**), `--entrypoint` dropping
  Cmd, and `box run` flag parsing — in particular that everything after the
  image name belongs to the container, so `box run img sh -c ls` does not read
  `-c` as a box flag.
- **`boxes.rs`** — `/proc/boxes` rows, truncated rows, decimal/hex ids, and
  name-vs-id resolution.

## On-target tests

```
box test            # smoke tests only
box test --net      # + network integration (downloads from Docker Hub)
```

- **smoke** — `json_parse`, `image_ref`, `image_argv`, `http_find_headers_end`.
  These re-run a slice of the host logic where the allocator is libakuma's and
  the target is bare-metal aarch64.
- **busybox_manifest** *(--net)* — Docker Hub still answers `library/busybox`
  with a manifest list containing a `linux/arm64` entry. This is a check on the
  *registry*, which no host test can make.
- **busybox_layer_size** *(--net)* — the whole download pipeline: resolve the
  platform manifest, download the layer blob, and verify the file on disk is
  exactly the size the manifest declared.

## TLS buffer truncation bug (fixed)

`busybox_layer_size` exists because of this one, and is why it asserts on an
exact size rather than "some bytes arrived".

**Symptom**: `box pull busybox` downloaded only ~217 KB of a ~1.9 MB layer.

**Root cause**: `TLS_RECORD_SIZE` was 16384 bytes, but a TLS 1.3 record on the
wire can be up to 16406 (5-byte header + 16384 plaintext + 1-byte content type +
16-byte AES-GCM tag). When the CDN sent a full-size record, `embedded-tls` could
not fit it and returned an error; `TlsStream::read` maps every error to
`IoError`, so the retry loop simply exhausted its 200-attempt budget and the
download stopped — silently, with a short file.

**Fix**: `TLS_RECORD_SIZE` 16384 → 17408 in `libakuma-tls/src/lib.rs`.
