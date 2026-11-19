#!/usr/bin/env python3
"""Build the ecosystem-demo images: one root per language box plus two guest disks.

    build-images.py fetch      download + lock the pinned Alpine packages and tool sources
    build-images.py layers     build the three roots (rust, scala, python) + the host root
    build-images.py disks      assemble guest1.img (rust + scala boxes) and guest2.img (python)
    build-images.py all        fetch, layers, disks, MANIFEST.json
    build-images.py repro      `all` twice into separate directories; compare every digest

Inputs are pinned, not floating: Alpine's packages come from a local mirror
(`$ECO_WORK/cache/mirror`, the same tree the launcher serves to the guests as
their apk repository), every file in it is recorded in `images/apk-lock.json` with
its sha256 and a build refuses a file that no longer matches. The akuma userspace
binaries (herd, sshd, box ...) are an input artifact with a recorded sha256.

A root's identity is the sha256 of its **uncompressed, sorted tar**
(`--sort=name`, owner 0, mtime = spec.epoch). A disk image is built from those
tars with a fixed UUID and `SOURCE_DATE_EPOCH`; its own digest is recorded and
compared too (`repro`) because ext2 inode order is a property of the tool.

No root, no docker: apk.static installs into a plain directory (`--usermode`),
`fakeroot` gives the ext2 populate step uid 0.
"""
import argparse
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
SPEC = json.loads((HERE / "images/spec.json").read_text())
WORK = Path(os.environ.get("ECO_WORK", "/var/tmp/akuma-eco"))
CACHE = WORK / "cache"
MIRROR = CACHE / "mirror"
BRANCH, ARCH, EPOCH = SPEC["alpine"], SPEC["arch"], SPEC["epoch"]
CDN = "https://dl-cdn.alpinelinux.org/alpine"
LOCK = HERE / "images/apk-lock.json"
REPOS = ["main", "community"]
ENV_PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"


def log(*a):
    print("[build-images]", *a, flush=True)


def run(*args, **kw):
    kw.setdefault("check", True)
    return subprocess.run([str(a) for a in args], **kw)


def sha256_file(p: Path) -> str:
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def download(url: str, dest: Path, sha256: str = ""):
    if dest.exists() and (not sha256 or sha256_file(dest) == sha256):
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_name(dest.name + ".part")
    log("download", url)
    with urllib.request.urlopen(url, timeout=120) as r, open(tmp, "wb") as f:
        shutil.copyfileobj(r, f)
    if sha256 and sha256_file(tmp) != sha256:
        tmp.unlink()
        raise SystemExit(f"sha256 mismatch for {url}")
    tmp.rename(dest)


# ----------------------------------------------------------------- apk mirror


def repo_dir(repo: str) -> Path:
    return MIRROR / BRANCH / repo / ARCH


def parse_index(repo: str) -> dict:
    """name -> version for one repository's APKINDEX."""
    out, name = {}, None
    with tarfile.open(repo_dir(repo) / "APKINDEX.tar.gz") as t:
        text = t.extractfile("APKINDEX").read().decode()
    for line in text.splitlines():
        if line.startswith("P:"):
            name = line[2:]
        elif line.startswith("V:") and name:
            out[name] = line[2:]
    return out


def tools():
    """apk.static and the Alpine signing keys, from the pinned mirror."""
    for repo in REPOS:
        download(f"{CDN}/{BRANCH}/{repo}/{ARCH}/APKINDEX.tar.gz", repo_dir(repo) / "APKINDEX.tar.gz")
    idx = parse_index("main")
    td = CACHE / "tools"
    apk = td / "apk.static"
    keys = td / "keys"
    if not apk.exists() or not keys.exists():
        td.mkdir(parents=True, exist_ok=True)
        for pkg in ("apk-tools-static", "alpine-keys"):
            f = repo_dir("main") / f"{pkg}-{idx[pkg]}.apk"
            download(f"{CDN}/{BRANCH}/main/{ARCH}/{f.name}", f)
            sub = td / pkg
            sub.mkdir(exist_ok=True)
            run("tar", "xzf", f, "-C", sub, check=False, stderr=subprocess.DEVNULL)
        shutil.copy(td / "apk-tools-static/sbin/apk.static", apk)
        apk.chmod(0o755)
        keys.mkdir(exist_ok=True)
        for k in list((td / "alpine-keys").rglob("*.rsa.pub")):
            shutil.copy(k, keys / k.name)
    return apk, keys


def apk_args(root: Path, keys: Path, local: bool):
    a = ["--root", root, "--keys-dir", keys, "--arch", ARCH, "--no-cache", "--no-progress"]
    if local:
        a += ["--no-network"] + sum([["-X", f"file://{MIRROR}/{BRANCH}/{r}"] for r in REPOS], [])
    else:
        a += sum([["-X", f"{CDN}/{BRANCH}/{r}"] for r in REPOS], [])
    return a


def all_packages() -> list:
    pk = set(SPEC["common"]) | set(SPEC["host"]) | set(SPEC["extras"])
    for im in SPEC["images"].values():
        pk |= set(im["packages"])
    return sorted(pk)


def fetch(relock=False):
    apk, keys = tools()
    scratch = CACHE / "fetchroot"
    shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir(parents=True)
    run(apk, *apk_args(scratch, keys, False), "--initdb", "add", "--initdb", "--no-scripts", "--simulate", *all_packages(),
        stdout=subprocess.DEVNULL, check=False)
    dl = CACHE / "dl"
    shutil.rmtree(dl, ignore_errors=True)
    dl.mkdir(parents=True)
    run(apk, *apk_args(scratch, keys, False), "fetch", "--recursive", "-o", dl, *all_packages())
    idx = {r: parse_index(r) for r in REPOS}
    for f in sorted(dl.glob("*.apk")):
        m = re.match(r"(.+)-([^-]+-r\d+)\.apk$", f.name)
        name, ver = m.group(1), m.group(2)
        for r in REPOS:
            if idx[r].get(name) == ver:
                shutil.move(f, repo_dir(r) / f.name)
                break
        else:
            raise SystemExit(f"{f.name}: not in any index")
    shutil.rmtree(dl)
    shutil.rmtree(scratch)
    # Tool sources outside Alpine.
    s = SPEC["scala"]
    download(s["url"], CACHE / "src" / Path(s["url"]).name, s["sha256"])
    for d in SPEC["scala_deps"]:
        download(d["url"], CACHE / "src" / Path(d["url"]).name, d["sha256"])
    for k in ("rust_toolchain", "scala_cli"):
        download(SPEC[k]["url"], CACHE / "src" / Path(SPEC[k]["url"]).name, SPEC[k]["sha256"])
    lock_check(relock)


def lock_check(relock=False):
    files = {}
    for r in REPOS:
        for f in sorted(repo_dir(r).glob("*")):
            files[f"{BRANCH}/{r}/{ARCH}/{f.name}"] = sha256_file(f)
    if relock or not LOCK.exists():
        LOCK.write_text(json.dumps({"alpine": BRANCH, "files": files}, indent=1, sort_keys=True) + "\n")
        log("wrote", LOCK.name, len(files), "files")
        return
    locked = json.loads(LOCK.read_text())["files"]
    bad = [k for k, v in files.items() if k in locked and locked[k] != v]
    new = [k for k in files if k not in locked]
    if bad:
        raise SystemExit(f"cache differs from images/apk-lock.json: {bad[:3]}")
    if new:
        raise SystemExit(f"{len(new)} cached files are not in apk-lock.json (run `fetch --relock` after review)")


# ----------------------------------------------------------------------- roots


def install(root: Path, pkgs: list):
    apk, keys = tools()
    root.mkdir(parents=True, exist_ok=True)
    r = run(apk, *apk_args(root, keys, True), "--initdb", "add", "--no-scripts", *pkgs,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, check=False)
    # apk 2.14 has no --usermode: unprivileged, it installs everything and then reports
    # that it could not chown/chmod directories. Ownership is normalized when the layer is
    # tarred, so that one error class is expected; anything else is a real failure.
    bad = [l for l in r.stderr.splitlines() if l.strip() and "updating directory permissions" not in l]
    if r.returncode and bad:
        raise SystemExit("apk add failed:\n" + "\n".join(bad[:20]))
    if not (root / "lib/apk/db/installed").exists():
        raise SystemExit("apk add left no database:\n" + r.stderr[-1500:])


def busybox_links(root: Path):
    bb = root / "bin/busybox.static"
    (root / "bin/busybox").unlink(missing_ok=True)
    os.rename(bb, root / "bin/busybox")
    names = subprocess.run([root / "bin/busybox", "--list-full"], capture_output=True, text=True, check=True).stdout.split()
    for n in sorted(names):
        p = root / n
        if p.exists() or p.is_symlink():
            continue
        p.parent.mkdir(parents=True, exist_ok=True)
        os.symlink("/bin/busybox", p)


def common_overlay(root: Path):
    shutil.copytree(HERE / "common", root, dirs_exist_ok=True, symlinks=True)
    (root / "etc/apk").mkdir(parents=True, exist_ok=True)
    (root / "etc/apk/repositories").write_text(
        "".join(f"http://169.254.0.2:8000/{BRANCH}/{r}\n" for r in REPOS))
    (root / "etc/apk/arch").write_text(ARCH + "\n")
    for d in ("tmp", "work", "root", "run", "proc", "dev", "sys"):
        (root / d).mkdir(exist_ok=True)
    # Denver cannot create /var/run/herd through Alpine's /var/run -> /run symlink.
    # Keep herd's control token in a real directory on both guest kernels.
    var_run = root / "var/run"
    if var_run.is_symlink():
        var_run.unlink()
    var_run.mkdir(parents=True, exist_ok=True)
    (root / "tmp").chmod(0o1777)


def finish_busybox(root: Path):
    busybox_links(root)
    ssl = root / "etc/ssl/certs/ca-certificates.crt"
    if ssl.exists():
        (root / "etc/ssl/cert.pem").unlink(missing_ok=True)
        os.symlink("certs/ca-certificates.crt", root / "etc/ssl/cert.pem")


def rust_extras(root: Path):
    vend = CACHE / "vendor"
    lockf = HERE / "images/rust-deps/Cargo.lock"
    stamp = vend / ".stamp"
    want = sha256_file(lockf)
    if not (stamp.exists() and stamp.read_text() == want):
        shutil.rmtree(vend, ignore_errors=True)
        run("cargo", "vendor", "--locked", "--manifest-path", HERE / "images/rust-deps/Cargo.toml", vend,
            stdout=subprocess.DEVNULL)
        stamp.write_text(want)
    shutil.copytree(vend, root / "opt/cargo-vendor", symlinks=True)
    (root / "opt/cargo-vendor/.stamp").unlink()
    (root / "root/.cargo").mkdir(parents=True, exist_ok=True)
    (root / "root/.cargo/config.toml").write_text(
        '[source.crates-io]\nreplace-with = "vendored-sources"\n\n'
        '[source.vendored-sources]\ndirectory = "/opt/cargo-vendor"\n\n[net]\noffline = true\n')
    shutil.copy(HERE / "images/rust-deps/Cargo.toml", root / "opt/cargo-vendor.Cargo.toml")
    shutil.copy(HERE / "images/rust-deps/Cargo.lock", root / "opt/cargo-vendor.Cargo.lock")


def rust_toolchain(root: Path):
    """The official rustc/cargo/rust-std (musl host) in /usr/local: Alpine's rust lags the crates the demo pins."""
    t = SPEC["rust_toolchain"]
    tmp = CACHE / "rust-unpack"
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True)
    run("tar", "xJf", CACHE / "src" / Path(t["url"]).name, "-C", tmp, "--strip-components=1")
    run("sh", tmp / "install.sh", f"--prefix={root}/usr/local", "--disable-ldconfig",
        "--components=rustc,cargo,rust-std-x86_64-unknown-linux-musl")
    shutil.rmtree(tmp)


def java_cacerts(root: Path):
    """Fill /etc/ssl/certs/java/cacerts from the root's CA bundle, deterministically.

    Alpine builds that store in a trigger (update-ca-certificates + p11-kit `trust`), and triggers do not
    run in these roots, so the JVM had NO trust anchors: every https download in scala-cli/Maven failed
    ("the trustAnchors parameter must be non-empty"). The JKS format is simple: magic, version 2, entries
    (tag 2 = trusted cert), then SHA-1 over the UTF-16BE password, a fixed string and everything before it.
    Timestamps are the fixed epoch, so the file is byte-reproducible.
    """
    import base64, struct
    pem = (root / "etc/ssl/certs/ca-certificates.crt").read_text()
    certs = [base64.b64decode("".join(m.split())) for m in
             re.findall(r"-----BEGIN CERTIFICATE-----(.*?)-----END CERTIFICATE-----", pem, re.S)]
    body = struct.pack(">IIi", 0xFEEDFEED, 2, len(certs))
    for i, der in enumerate(certs):
        alias = f"ca{i:04d}".encode()
        body += struct.pack(">I", 2) + struct.pack(">H", len(alias)) + alias + struct.pack(">q", EPOCH * 1000)
        body += struct.pack(">H", 5) + b"X.509" + struct.pack(">I", len(der)) + der
    pw = "changeit".encode("utf-16-be")
    body += hashlib.sha1(pw + b"Mighty Aphrodite" + body).digest()
    dest = root / "etc/ssl/certs/java/cacerts"
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(body)
    log("java cacerts:", len(certs), "certificates")


def scala_cli(root: Path):
    """scala-cli as its JVM jar behind a launcher: the native image fails on this kernel (GraalVM isolate, code 8)."""
    c = SPEC["scala_cli"]
    d = root / "opt/scala-cli"
    d.mkdir(parents=True, exist_ok=True)
    shutil.copy(CACHE / "src" / Path(c["url"]).name, d / "scala-cli.jar")
    w = root / "usr/local/bin/scala-cli"
    w.parent.mkdir(parents=True, exist_ok=True)
    w.write_text('#!/bin/sh\n# os.name comes from the kernel uname sysname ("Akuma"); the coursier directories library refuses it.\nexec java -Dos.name=Linux -jar /opt/scala-cli/scala-cli.jar "$@"\n')
    w.chmod(0o755)


def scala_extras(root: Path):
    s = SPEC["scala"]
    tgz = CACHE / "src" / Path(s["url"]).name
    (root / "opt").mkdir(exist_ok=True)
    run("tar", "xzf", tgz, "-C", root / "opt")
    # A real directory, not `scala -> scala3-X`: inside a box a path THROUGH a relative
    # directory symlink in an overlay layer does not resolve (`cd /opt/scala/bin` ENOENT), and
    # the launcher scripts `cd "$(dirname $0)/.."`. See docs/archive/AKUMA_ECOSYSTEM_DEMO.md.
    os.rename(root / "opt" / s["dir"], root / "opt/scala")
    (root / "usr/local/bin").mkdir(parents=True, exist_ok=True)
    for tool in ("scala", "scalac", "scaladoc"):
        os.symlink(f"/opt/scala/bin/{tool}", root / "usr/local/bin" / tool)
    deps = root / "opt/deps"
    deps.mkdir()
    for d in SPEC["scala_deps"]:
        shutil.copy(CACHE / "src" / Path(d["url"]).name, deps)
    jvm = sorted((root / "usr/lib/jvm").glob("java-*-openjdk"))
    if jvm:
        os.symlink(jvm[0].name, root / "usr/lib/jvm/default-jvm")
        for tool in ("java", "javac", "jar"):
            link = root / "usr/bin" / tool
            if not (link.exists() or link.is_symlink()):
                os.symlink(f"/usr/lib/jvm/default-jvm/bin/{tool}", link)


def normalize(root: Path):
    for dp, dns, fns in os.walk(root):
        for n in dns + fns:
            os.utime(os.path.join(dp, n), (EPOCH, EPOCH), follow_symlinks=False)
    os.utime(root, (EPOCH, EPOCH))
    # Modes the build user could not have set differently: make dirs writable by owner so cleanup works.
    for dp, dns, fns in os.walk(root):
        for n in dns:
            p = os.path.join(dp, n)
            if not os.path.islink(p):
                os.chmod(p, os.stat(p).st_mode | 0o700)


def tar_layer(root: Path, out_gz: Path) -> str:
    """Deterministic tar of `root`; returns sha256 of the UNCOMPRESSED stream."""
    out_gz.parent.mkdir(parents=True, exist_ok=True)
    tar = subprocess.Popen(
        ["tar", "--sort=name", f"--mtime=@{EPOCH}", "--owner=0", "--group=0", "--numeric-owner",
         "--format=gnu", "-C", str(root), "-cf", "-", "."], stdout=subprocess.PIPE)
    h = hashlib.sha256()
    with open(out_gz, "wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=6, mtime=0) as gz:
        for chunk in iter(lambda: tar.stdout.read(1 << 20), b""):
            h.update(chunk)
            gz.write(chunk)
    if tar.wait():
        raise SystemExit("tar failed")
    return h.hexdigest()


def build_roots(out: Path) -> dict:
    fetch_if_needed()
    info = {}
    for name in ["host", *SPEC["images"]]:
        root = out / "trees" / name
        shutil.rmtree(root, ignore_errors=True)
        pk = SPEC["host"] if name == "host" else sorted(set(SPEC["common"]) | set(SPEC["images"][name]["packages"]) | set(SPEC["extras"]))
        log("root", name, len(pk), "requested packages")
        install(root, pk)
        finish_busybox(root)
        if name != "host":
            common_overlay(root)
        if name == "rust":
            rust_toolchain(root)
        if name == "rust" and SPEC.get("rust_cargo_cache"):
            rust_extras(root)   # opt-in: --with-cargo-cache (or "rust_cargo_cache": true in spec.json)
        if name == "scala":
            scala_extras(root)
            java_cacerts(root)
            scala_cli(root)
        if name == "host":
            userspace_into(root)
        normalize(root)
        digest = tar_layer(root, out / "layers" / f"{name}.tar.gz")
        installed = (root / "lib/apk/db/installed").read_text()
        pkgs = re.findall(r"^P:(.+)\nV:(.+)$", installed, re.M)
        info[name] = {"diff_id": "sha256:" + digest, "packages": len(pkgs),
                      "size_bytes": sum(p.stat().st_size for p in root.rglob("*") if p.is_file() and not p.is_symlink())}
        log(name, info[name]["diff_id"], info[name]["packages"], "packages")
        shutil.rmtree(root)   # the tar is the artifact; trees are rebuilt from it
    return info


def fetch_if_needed():
    if not (CACHE / "tools/apk.static").exists() or not LOCK.exists():
        fetch()
    else:
        for k in ("rust_toolchain", "scala_cli"):      # sources added after the cache was first filled
            download(SPEC[k]["url"], CACHE / "src" / Path(SPEC[k]["url"]).name, SPEC[k]["sha256"])
        lock_check()


# ------------------------------------------------------------------- userspace


def userspace_into(root: Path):
    """The akuma binaries (herd, sshd, ...) from a mkdisk image: an input artifact, hashed."""
    src = userspace_dir()
    (root / "bin").mkdir(exist_ok=True)
    for n in SPEC["userspace"]:
        shutil.copy(src / n, root / "bin" / n)
        (root / "bin" / n).chmod(0o755)
    shutil.copytree(HERE / "guest", root, dirs_exist_ok=True, symlinks=True)
    for d in ("tmp", "run", "proc", "dev", "sys", "root", "var/lib/box/layers", "var/lib/box/images",
              "var/lib/box/containers", "var/lib/agents"):
        (root / d).mkdir(parents=True, exist_ok=True)
    # Denver cannot create /var/run/herd through Alpine's /var/run -> /run symlink.
    # Keep herd's control token in a real directory on both guest kernels.
    var_run = root / "var/run"
    if var_run.is_symlink():
        var_run.unlink()
    var_run.mkdir(parents=True, exist_ok=True)
    (root / "tmp").chmod(0o1777)
    (root / "var/lib/agents").chmod(0o700)


def userspace_dir() -> Path:
    d = WORK / "userspace"
    img = Path(os.environ.get("ECO_BASE_IMG", WORK / "base.img"))
    if not all((d / n).exists() for n in SPEC["userspace"]):
        shutil.rmtree(d, ignore_errors=True)
        if not img.exists():
            run(ROOT / "amd64/mkdisk.sh", img, "1024", cwd=ROOT)
        d.mkdir(parents=True)
        for n in SPEC["userspace"]:
            run("debugfs", "-R", f"dump /bin/{n} {d / n}", img, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if not (d / n).exists() or (d / n).stat().st_size == 0:
                raise SystemExit(f"{n} missing from {img}")
    return d


# ----------------------------------------------------------------------- disks


def oci_config(name: str) -> str:
    env = [f"PATH={ENV_PATH}", "HOME=/root", "TERM=xterm", "LANG=C.UTF-8"]
    if name == "scala":
        env += ["JAVA_HOME=/usr/lib/jvm/default-jvm", "SCALA_HOME=/opt/scala"]
    if name == "rust":
        env += ["CARGO_HOME=/root/.cargo"]
    return json.dumps({"architecture": "amd64", "os": "linux",
                       "config": {"Env": env, "Cmd": ["/bin/sh"], "WorkingDir": "/work"}}, indent=1)


def build_disk(out: Path, guest: int, info: dict) -> Path:
    """One fakeroot session: extract the host root and each box layer, then populate ext2."""
    members = [n for n, im in SPEC["images"].items() if im["guest"] == guest]
    stage = out / f"stage-guest{guest}"
    img = out / f"guest{guest}.img"
    meta = out / f"meta-guest{guest}"
    shutil.rmtree(stage, ignore_errors=True)
    shutil.rmtree(meta, ignore_errors=True)
    stage.mkdir(parents=True)
    img.unlink(missing_ok=True)
    sh = ["set -e", "umask 022", f"tar xzf {out / 'layers/host.tar.gz'} -C {stage}"]
    for n in members:
        hexd = info[n]["diff_id"].split(":")[1]
        idir = meta / "var/lib/box/images" / n
        idir.mkdir(parents=True)
        (idir / "oci-config.json").write_text(oci_config(n))
        (idir / "layers").write_text(info[n]["diff_id"] + "\n")
        ld = stage / "var/lib/box/layers" / f"sha256-{hexd}"
        sh += [f"mkdir -p {ld}", f"tar xzf {out / 'layers' / (n + '.tar.gz')} -C {ld}"]
    sh += [f"cp -a {meta}/var/lib/box/images/. {stage}/var/lib/box/images/" if members else "true"]
    uuid = f"0000ec00-0000-4000-8000-00000000000{guest}"
    sh += [
        f"MB=$(( $(du -sm {stage} | cut -f1) * 13 / 10 + 400 ))",
        f"truncate -s ${{MB}}M {img}",
        f"mkfs.ext2 -q -F -b 4096 -i 8192 -L eco-guest{guest} -U {uuid} "
        f"-E root_owner=0:0,hash_seed={uuid},lazy_itable_init=0 -d {stage} {img}",
    ]
    env = dict(os.environ, SOURCE_DATE_EPOCH=str(EPOCH), E2FSPROGS_FAKE_TIME=str(EPOCH))
    run("fakeroot", "--", "sh", "-c", "\n".join(sh), env=env)
    shutil.rmtree(stage)
    shutil.rmtree(meta)
    return img


def build_all(out: Path) -> dict:
    out.mkdir(parents=True, exist_ok=True)
    info = build_roots(out)
    disks = {}
    for g in sorted({im["guest"] for im in SPEC["images"].values()}):
        img = build_disk(out, g, info)
        disks[f"guest{g}"] = {"sha256": sha256_file(img), "size_bytes": img.stat().st_size,
                              "boxes": [n for n, im in SPEC["images"].items() if im["guest"] == g]}
        log(img.name, disks[f"guest{g}"]["sha256"])
    rev = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    dirty = bool(subprocess.run(["git", "-C", ROOT, "status", "--porcelain", "--", "overlays/ecosystem-demo", "userspace", "crates"],
                                capture_output=True, text=True).stdout.strip())
    man = {"source_revision": rev, "source_tree_dirty": dirty, "spec_sha256": sha256_file(HERE / "images/spec.json"),
           "rust_cargo_cache": bool(SPEC.get("rust_cargo_cache")),
           "apk_lock_sha256": sha256_file(LOCK), "alpine": BRANCH, "epoch": EPOCH,
           "userspace_inputs": {n: sha256_file(userspace_dir() / n) for n in SPEC["userspace"]},
           "images": info, "disks": disks}
    (out / "MANIFEST.json").write_text(json.dumps(man, indent=1, sort_keys=True) + "\n")
    return man


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["fetch", "layers", "disks", "all", "repro"])
    ap.add_argument("--out", type=Path, default=WORK / "out")
    ap.add_argument("--relock", action="store_true")
    ap.add_argument("--with-cargo-cache", action="store_true",
                    help="vendor images/rust-deps into the rust root (off by default)")
    a = ap.parse_args()
    if a.with_cargo_cache:
        SPEC["rust_cargo_cache"] = True
    if a.cmd == "fetch":
        fetch(a.relock)
    elif a.cmd == "layers":
        print(json.dumps(build_roots(a.out), indent=1))
    elif a.cmd == "disks":
        info = {n: {"diff_id": json.loads((a.out / "MANIFEST.json").read_text())["images"][n]["diff_id"]} for n in SPEC["images"]}
        for g in sorted({im["guest"] for im in SPEC["images"].values()}):
            build_disk(a.out, g, info)
    elif a.cmd == "all":
        build_all(a.out)
    elif a.cmd == "repro":
        m1 = build_all(a.out.with_name(a.out.name + "-a"))
        m2 = build_all(a.out.with_name(a.out.name + "-b"))
        ok = True
        for k in sorted(m1["images"]):
            same = m1["images"][k]["diff_id"] == m2["images"][k]["diff_id"]
            ok &= same
            print(f"root {k:7s} {m1['images'][k]['diff_id']} {'SAME' if same else 'DIFFERENT ' + m2['images'][k]['diff_id']}")
        for k in sorted(m1["disks"]):
            same = m1["disks"][k]["sha256"] == m2["disks"][k]["sha256"]
            print(f"disk {k:7s} {m1['disks'][k]['sha256']} {'SAME' if same else 'DIFFERENT ' + m2['disks'][k]['sha256']}")
            ok &= same
        sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
