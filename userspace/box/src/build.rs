//! `box build` — Docker-compatible image builds. Design: `docs/reference/box-build.md`
//! (history and rationale in `docs/archive/BOX_BUILD.md`).
//!
//! The builder runs in box 0, the role `buildkitd` plays for Docker. Each `RUN`
//! is a short-lived box over the layers built so far; its writable upper is
//! committed as the next layer (`commit.rs`). `COPY`/`ADD` write into a fresh
//! upper from the host. Metadata instructions only edit the image config.
//!
//! The pure parts — parsing, expansion, config writing, path arithmetic, layer
//! stacking — are in `boxlib` with host tests; this file is the I/O around them.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{chmod, exit, lchown, mkdir_p, print, println, symlink};
use sha2::{Digest, Sha256};

use boxlib::buildopts::{self, BuildArgsError};
use boxlib::copyplan::{self, Dest};
use boxlib::dockerfile::{self, Cmd, CopyArgs, Dockerfile, Instr, Stage, Step};
use boxlib::dockerignore::{match_glob, Ignore};
use boxlib::imgconfig::{rfc3339, ImageConfig};
use boxlib::paths;
use boxlib::spec;
use boxlib::stack::{LayerFs, Node, Stack};
use boxlib::sys;

use crate::commit::{self, Committed};
use crate::fsx::{self, S_IFDIR, S_IFLNK, S_IFMT};
use crate::images;

const USAGE: &str = "Usage: box build -t <name> [-f Dockerfile] [--build-arg K[=V]] [--target stage] [--no-cache] <context>\n";
/// The kernel's overlay accepts this many lower directories (`MAX_LOWER_LAYERS`);
/// one is the container's runtime layer.
const MAX_IMAGE_LAYERS: usize = 31;

// ───────────────────────────── state ─────────────────────────────

#[derive(Clone)]
struct StageState {
    config: ImageConfig,
    /// Digests, base-first.
    layers: Vec<String>,
    sizes: Vec<Option<u64>>,
    /// `ARG`s declared in this stage, with their values.
    args: Vec<(String, String)>,
    /// Whether `CMD` was set in *this* stage (a later `ENTRYPOINT` then keeps it).
    cmd_set: bool,
}

impl StageState {
    fn scratch() -> Self {
        Self { config: ImageConfig::scratch(), layers: Vec::new(), sizes: Vec::new(), args: Vec::new(), cmd_set: false }
    }

    fn from_image(store: &str) -> Result<Self, String> {
        let json = images::load_config(store).ok_or_else(|| format!("image '{store}' has no readable config"))?;
        let layers = images::load_layers(store);
        let sizes = images::layer_sizes(store, &layers);
        Ok(Self { config: ImageConfig::parse(&json), layers, sizes, args: Vec::new(), cmd_set: false })
    }

    /// `ENV` shadows `ARG`, as in Docker.
    fn var(&self, key: &str) -> Option<String> {
        for e in self.config.env.iter().rev() {
            if let Some((k, v)) = e.split_once('=') {
                if k == key {
                    return Some(String::from(v));
                }
            }
        }
        self.args.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn workdir(&self) -> &str {
        if self.config.working_dir.is_empty() {
            "/"
        } else {
            &self.config.working_dir
        }
    }

    fn parent_id(&self) -> &str {
        self.layers.last().map_or("scratch", String::as_str)
    }

    fn push_layer(&mut self, c: &Committed, created_by: &str) {
        self.layers.push(c.digest.clone());
        self.sizes.push(Some(c.size));
        self.config.diff_ids.push(c.diff_id.clone());
        self.config.push_history(&rfc3339(fsx::now_secs()), created_by, true);
    }

    fn push_meta(&mut self, created_by: &str) {
        self.config.push_history(&rfc3339(fsx::now_secs()), created_by, false);
    }

    /// Environment for a `RUN`: the image's `Env` plus declared `ARG`s that
    /// `ENV` does not shadow.
    fn run_env(&self) -> Vec<String> {
        let mut env = self.config.env.clone();
        for (k, v) in &self.args {
            if !env.iter().any(|e| spec::env_key(e) == k) {
                env.push(format!("{k}={v}"));
            }
        }
        env
    }
}

/// The layers of a stage as an on-disk `LayerFs` (index 0 = topmost).
struct DiskFs {
    dirs: Vec<String>,
}

impl LayerFs for DiskFs {
    fn node(&self, layer: usize, rel: &str) -> Option<Node> {
        let full = if rel.is_empty() { self.dirs[layer].clone() } else { format!("{}/{}", self.dirs[layer], rel) };
        let st = fsx::lstat(&full).ok()?;
        Some(if st.st_mode & S_IFMT == S_IFDIR { Node::Dir } else { Node::Other })
    }
    fn list(&self, layer: usize, rel: &str) -> Vec<String> {
        let full = if rel.is_empty() { self.dirs[layer].clone() } else { format!("{}/{}", self.dirs[layer], rel) };
        fsx::list_sorted(&full).into_iter().map(|(n, _)| n).collect()
    }
}

enum Kind {
    Dir,
    File,
    Symlink(String),
    /// `ADD` of a local tar: unpack into `target`.
    Archive { gzip: bool },
}

struct Item {
    /// Host path of the source (unused for `Dir`).
    src: String,
    /// Absolute path in the image.
    target: String,
    kind: Kind,
    mode: u32,
}

// ───────────────────────────── builder ─────────────────────────────

struct Builder<'a> {
    df: &'a Dockerfile,
    ctx: String,
    ignore: Ignore,
    cli_args: Vec<(String, Option<String>)>,
    used_cli: Vec<String>,
    no_cache: bool,
    seq: u32,
    done: Vec<Option<StageState>>,
    global: Vec<(String, String)>,
}

fn fail<T>(msg: impl Into<String>) -> Result<T, String> {
    Err(msg.into())
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        String::from(b)
    } else {
        format!("{a}/{b}")
    }
}

fn hex(h: Sha256) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    for b in h.finalize() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn key_of(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    hex(h)
}

impl<'a> Builder<'a> {
    // ── cache ──

    fn cache_get(&self, key: &str) -> Option<Committed> {
        if self.no_cache {
            return None;
        }
        let rec = fsx::read_text(&paths::cache_path(key))?;
        let mut it = rec.split_whitespace();
        let (digest, diff_id, size) = (it.next()?, it.next()?, it.next()?);
        if !fsx::exists(&images::layer_dir(digest)) || !fsx::exists(&images::blob_path(digest)) {
            return None;
        }
        Some(Committed { digest: digest.into(), diff_id: diff_id.into(), size: size.parse().ok()? })
    }

    fn cache_put(&self, key: &str, c: &Committed) {
        let _ = fsx::write_file(&paths::cache_path(key), format!("{} {} {}\n", c.digest, c.diff_id, c.size).as_bytes());
    }

    fn fresh_name(&mut self, what: &str) -> String {
        self.seq += 1;
        format!("build-{what}-{}-{}", libakuma::uptime(), self.seq)
    }

    // ── ARG handling ──

    fn arg_value(&mut self, name: &str, default: Option<String>, inherited: Option<String>) -> Option<String> {
        if let Some((_, v)) = self.cli_args.iter().find(|(k, _)| k == name).cloned() {
            if !self.used_cli.iter().any(|u| u == name) {
                self.used_cli.push(String::from(name));
            }
            return match v {
                Some(v) => Some(v),
                None => libakuma::env(name).map(String::from).or(default),
            };
        }
        inherited.or(default)
    }

    fn init_globals(&mut self) {
        let mut global: Vec<(String, String)> = Vec::new();
        for step in &self.df.global_args {
            let Instr::Arg(raw) = &step.instr else { continue };
            let look = |k: &str| global.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            let decls = dockerfile::arg_decls(raw, &look);
            for (name, default) in decls {
                if let Some(v) = self.arg_value(&name, default, None) {
                    global.push((name, v));
                }
            }
        }
        self.global = global;
    }

    // ── stage start ──

    fn start_stage(&mut self, stage: &Stage) -> Result<StageState, String> {
        let global = self.global.clone();
        let look = |k: &str| global.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let image = dockerfile::word(&stage.image, &look);
        print("FROM ");
        println(&image);
        if image == "scratch" {
            return Ok(StageState::scratch());
        }
        if let Some(i) = self.df.stage_index(&image).filter(|&i| i < stage.index) {
            return match &self.done[i] {
                Some(s) => {
                    let mut s = s.clone();
                    s.args.clear();
                    s.cmd_set = false;
                    Ok(s)
                }
                None => fail(format!("stage '{image}' was not built")),
            };
        }
        let store = images::sanitize_name(&image);
        if !images::image_exists(&store) {
            crate::oci::pull_image(&image, false)?;
        }
        StageState::from_image(&store)
    }

    // ── steps ──

    fn run_stage(&mut self, stage: &Stage) -> Result<StageState, String> {
        let mut st = self.start_stage(stage)?;
        let total = stage.steps.len();
        for (i, step) in stage.steps.iter().enumerate() {
            print("Step ");
            libakuma::print_dec(i + 1);
            print("/");
            libakuma::print_dec(total);
            print(" : ");
            println(&step.text);
            self.step(&mut st, step).map_err(|e| format!("line {}: {}", step.line, e))?;
        }
        Ok(st)
    }

    fn step(&mut self, st: &mut StageState, step: &Step) -> Result<(), String> {
        let text = step.text.as_str();
        match &step.instr {
            Instr::From { .. } => unreachable!("FROM starts a stage"),
            Instr::Arg(raw) => {
                let decls = dockerfile::arg_decls(raw, &|k| st.var(k));
                for (name, default) in decls {
                    let inherited = self.global.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone());
                    if let Some(v) = self.arg_value(&name, default, inherited) {
                        st.args.retain(|(k, _)| *k != name);
                        st.args.push((name, v));
                    }
                }
                st.push_meta(text);
            }
            Instr::Env(raw) => {
                for (k, v) in dockerfile::env_pairs(raw, &|k| st.var(k))? {
                    st.config.set_env(&k, &v);
                }
                st.push_meta(text);
            }
            Instr::Label(raw) => {
                for (k, v) in dockerfile::env_pairs(raw, &|k| st.var(k))? {
                    st.config.labels.insert(k, v);
                }
                st.push_meta(text);
            }
            Instr::User(raw) => {
                st.config.user = dockerfile::word(raw, &|k| st.var(k));
                st.push_meta(text);
            }
            Instr::Expose(raw) => {
                for p in dockerfile::words(raw, &|k| st.var(k)) {
                    let p = buildopts::normalize_port(&p);
                    if !st.config.exposed_ports.contains(&p) {
                        st.config.exposed_ports.push(p);
                    }
                }
                st.push_meta(text);
            }
            Instr::Volume(raw) => {
                let vols = dockerfile::json_string_array(raw).unwrap_or_else(|| dockerfile::words(raw, &|k| st.var(k)));
                for v in vols {
                    if !st.config.volumes.contains(&v) {
                        st.config.volumes.push(v);
                    }
                }
                st.push_meta(text);
            }
            Instr::StopSignal(raw) => {
                st.config.stop_signal = dockerfile::word(raw, &|k| st.var(k));
                st.push_meta(text);
            }
            Instr::Shell(v) => {
                st.config.shell = v.clone();
                st.push_meta(text);
            }
            Instr::Healthcheck(raw) => {
                st.config.healthcheck = Some(buildopts::parse_healthcheck(raw)?);
                st.push_meta(text);
            }
            Instr::Cmd(c) => {
                st.config.cmd = Self::argv_of(st, c);
                st.cmd_set = true;
                st.push_meta(text);
            }
            Instr::Entrypoint(c) => {
                st.config.entrypoint = Self::argv_of(st, c);
                // An inherited CMD was written for the old entrypoint.
                if !st.cmd_set {
                    st.config.cmd.clear();
                }
                st.push_meta(text);
            }
            Instr::Workdir(raw) => self.step_workdir(st, raw, text)?,
            Instr::Run(c) => self.step_run(st, c, text)?,
            Instr::Copy(a) => self.step_copy(st, a, false, text)?,
            Instr::Add(a) => self.step_copy(st, a, true, text)?,
        }
        Ok(())
    }

    fn argv_of(st: &StageState, c: &Cmd) -> Vec<String> {
        match c {
            Cmd::Exec(v) => v.clone(),
            Cmd::Shell(s) => {
                let mut v = if st.config.shell.is_empty() {
                    alloc::vec![String::from("/bin/sh"), String::from("-c")]
                } else {
                    st.config.shell.clone()
                };
                v.push(s.clone());
                v
            }
        }
    }

    // ── WORKDIR ──

    fn step_workdir(&mut self, st: &mut StageState, raw: &str, text: &str) -> Result<(), String> {
        let wanted = dockerfile::word(raw, &|k| st.var(k));
        let abs = copyplan::absolutize(st.workdir(), &wanted);
        st.config.working_dir = abs.clone();

        let dirs = paths::lowerdirs(&st.layers);
        let fs = DiskFs { dirs: dirs.clone() };
        let stack = Stack::new(&fs, dirs.len());
        let rel = abs.trim_start_matches('/');
        if rel.is_empty() || stack.resolve(rel).is_some() {
            st.push_meta(text);
            return Ok(());
        }
        let key = key_of(&["WORKDIR", st.parent_id(), &abs]);
        let c = match self.cache_get(&key) {
            Some(c) => {
                println(" ---> Using cache");
                c
            }
            None => {
                let name = self.fresh_name("wd");
                let upper = images::container_upper(&name);
                if !mkdir_p(&upper) {
                    return fail(format!("cannot create {upper}"));
                }
                ensure_dir(&upper, &stack, &dirs, &abs, 0o755, (0, 0))?;
                let c = commit::commit_upper(&upper)?;
                fsx::remove_tree(&images::container_dir(&name));
                self.cache_put(&key, &c);
                c
            }
        };
        st.push_layer(&c, text);
        Ok(())
    }

    // ── RUN ──

    fn step_run(&mut self, st: &mut StageState, cmd: &Cmd, text: &str) -> Result<(), String> {
        let argv = Self::argv_of(st, cmd);
        let env = st.run_env();
        let key = key_of(&[
            "RUN",
            st.parent_id(),
            &argv.join("\x1f"),
            &env.join("\x1f"),
            st.workdir(),
            &st.config.user,
        ]);
        let c = match self.cache_get(&key) {
            Some(c) => {
                println(" ---> Using cache");
                c
            }
            None => {
                let c = self.exec_run(st, &argv, env)?;
                self.cache_put(&key, &c);
                c
            }
        };
        st.push_layer(&c, text);
        Ok(())
    }

    fn exec_run(&mut self, st: &StageState, argv: &[String], env: Vec<String>) -> Result<Committed, String> {
        if st.layers.len() > MAX_IMAGE_LAYERS {
            return fail(format!(
                "the image already has {} layers; the overlay root takes at most {MAX_IMAGE_LAYERS} plus the runtime layer",
                st.layers.len()
            ));
        }
        let name = self.fresh_name("run");
        let croot = images::container_dir(&name);
        let upper = images::container_upper(&name);
        let runtime = images::container_runtime(&name);
        let result = (|| -> Result<Committed, String> {
            if !mkdir_p(&upper) || !mkdir_p(&runtime) {
                return fail(format!("cannot create {croot}"));
            }
            crate::run::inject_runtime_files(&runtime, &name);

            let lowers = paths::lowerdirs(&st.layers);
            for d in &lowers {
                if !images::dir_exists(d) {
                    return fail(format!("missing layer {d}"));
                }
            }
            let box_id = spec::box_id_for(&name);
            sys::register_box(box_id, &name, &croot, 0);
            let mut mount_lowers = alloc::vec![runtime.clone()];
            mount_lowers.extend(lowers.iter().cloned());
            let rc = libakuma::mount_overlay_root(box_id, &mount_lowers, &upper);
            if rc != 0 {
                sys::kill_box(box_id);
                return fail(format!("overlay mount failed: errno {}", -rc));
            }
            let _ = libakuma::mount_in_ns(box_id, "/proc", "proc", None);

            let outcome = self.spawn_and_wait(st, argv, env, box_id, &upper, &lowers);
            sys::kill_box(box_id);
            let code = outcome?;
            if code != 0 {
                return fail(format!("The command '{}' returned a non-zero code: {code}", argv.join(" ")));
            }
            commit::commit_upper(&upper)
        })();
        fsx::remove_tree(&croot);
        result
    }

    fn spawn_and_wait(
        &self,
        st: &StageState,
        argv: &[String],
        env: Vec<String>,
        box_id: u64,
        upper: &str,
        lowers: &[String],
    ) -> Result<i32, String> {
        let user = &st.config.user;
        let run_as = spec::resolve_user(
            user,
            &crate::run::read_in_container(upper, lowers, "/etc/passwd"),
            &crate::run::read_in_container(upper, lowers, "/etc/group"),
        )?;
        let env = spec::compose_env(&env, &[]);
        let path = crate::run::resolve_in_container(upper, lowers, &argv[0]);
        let rest: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        let workdir = String::from(st.workdir());

        let mut options = sys::SpawnOptions {
            cwd_ptr: workdir.as_ptr() as u64,
            cwd_len: workdir.len(),
            box_id,
            ..Default::default()
        };
        let groups: Vec<u32> = run_as.as_ref().map(|r| r.groups.clone()).unwrap_or_default();
        if let Some(r) = &run_as {
            options.set_creds(r.uid, r.gid, &groups);
        }
        let rest_opt = if rest.is_empty() { None } else { Some(rest.as_slice()) };
        let Some(res) = sys::spawn_ext_env(&path, rest_opt, Some(&env), None, &mut options) else {
            return fail(format!("cannot start {path} in the build container"));
        };
        if libakuma::reattach(res.pid, false) != 0 {
            return fail("reattach failed");
        }
        loop {
            if let Some(ws) = libakuma::waitpid_status(res.pid) {
                return Ok(ws.shell_code());
            }
            libakuma::sleep_ms(50);
        }
    }

    // ── COPY / ADD ──

    fn expand_src(&self, pat: &str) -> Result<Vec<String>, String> {
        let rel = copyplan::context_relative(pat).ok_or_else(|| format!("'{pat}' is outside the build context"))?;
        if !rel.contains(['*', '?', '[']) {
            return Ok(alloc::vec![rel]);
        }
        let mut current = alloc::vec![String::new()];
        for seg in rel.split('/') {
            let mut next = Vec::new();
            for base in &current {
                if seg.contains(['*', '?', '[']) {
                    for (n, _) in fsx::list_sorted(&join(&self.ctx, base)) {
                        if match_glob(seg, &n) {
                            next.push(join(base, &n));
                        }
                    }
                } else if fsx::exists(&join(&self.ctx, &join(base, seg))) {
                    next.push(join(base, seg));
                }
            }
            current = next;
        }
        if current.is_empty() {
            return fail(format!("no source files match '{pat}'"));
        }
        Ok(current)
    }

    fn plan_ctx(&self, rel: &str, dest: &Dest, add: bool, items: &mut Vec<Item>) -> Result<(), String> {
        let full = join(&self.ctx, rel);
        let st = fsx::lstat(&full).map_err(|_| format!("'{rel}' not found in the build context"))?;
        if !rel.is_empty() && self.ignore.is_ignored(rel) {
            return fail(format!("'{rel}' is excluded by .dockerignore"));
        }
        let name = rel.rsplit('/').next().unwrap_or("");
        match st.st_mode & S_IFMT {
            S_IFDIR => {
                items.push(Item { src: full, target: dest.path.clone(), kind: Kind::Dir, mode: st.st_mode & 0o7777 });
                self.plan_ctx_dir(rel, "", dest, items)
            }
            S_IFLNK => {
                let t = libakuma::readlink(&full).unwrap_or_default();
                items.push(Item {
                    src: full,
                    target: copyplan::target_for(dest, false, "", name),
                    kind: Kind::Symlink(t),
                    mode: 0o777,
                });
                Ok(())
            }
            _ => {
                if add && copyplan::is_unpackable_archive(name) {
                    items.push(Item {
                        src: full,
                        target: dest.path.clone(),
                        kind: Kind::Archive { gzip: !name.ends_with(".tar") },
                        mode: 0o755,
                    });
                } else {
                    items.push(Item {
                        src: full,
                        target: copyplan::target_for(dest, false, "", name),
                        kind: Kind::File,
                        mode: st.st_mode & 0o7777,
                    });
                }
                Ok(())
            }
        }
    }

    fn plan_ctx_dir(&self, rel: &str, within: &str, dest: &Dest, items: &mut Vec<Item>) -> Result<(), String> {
        for (n, _) in fsx::list_sorted(&join(&self.ctx, rel)) {
            let crel = join(rel, &n);
            if self.ignore.is_ignored(&crel) {
                continue;
            }
            let full = join(&self.ctx, &crel);
            let st = fsx::lstat(&full).map_err(|e| format!("stat {full}: errno {e}"))?;
            let w = join(within, &n);
            let target = copyplan::target_for(dest, true, &w, "");
            match st.st_mode & S_IFMT {
                S_IFDIR => {
                    items.push(Item { src: full, target, kind: Kind::Dir, mode: st.st_mode & 0o7777 });
                    self.plan_ctx_dir(&crel, &w, dest, items)?;
                }
                S_IFLNK => {
                    let t = libakuma::readlink(&full).unwrap_or_default();
                    items.push(Item { src: full, target, kind: Kind::Symlink(t), mode: 0o777 });
                }
                _ => items.push(Item { src: full, target, kind: Kind::File, mode: st.st_mode & 0o7777 }),
            }
        }
        Ok(())
    }

    fn plan_stack(
        &self,
        stack: &Stack<DiskFs>,
        dirs: &[String],
        rel: &str,
        within: &str,
        dest: &Dest,
        items: &mut Vec<Item>,
    ) -> Result<(), String> {
        let found = stack.resolve(rel).ok_or_else(|| format!("'/{rel}' not found in the source stage"))?;
        let full = if rel.is_empty() { dirs[found.top].clone() } else { join(&dirs[found.top], rel) };
        let st = fsx::lstat(&full).map_err(|e| format!("stat {full}: errno {e}"))?;
        let name = rel.rsplit('/').next().unwrap_or("");
        match found.node {
            Node::Dir => {
                let target = copyplan::target_for(dest, true, within, "");
                items.push(Item { src: full, target, kind: Kind::Dir, mode: st.st_mode & 0o7777 });
                for n in stack.list(rel) {
                    self.plan_stack(stack, dirs, &join(rel, &n), &join(within, &n), dest, items)?;
                }
            }
            Node::Other => {
                let target = if within.is_empty() {
                    copyplan::target_for(dest, false, "", name)
                } else {
                    copyplan::target_for(dest, true, within, "")
                };
                if st.st_mode & S_IFMT == S_IFLNK {
                    let t = libakuma::readlink(&full).unwrap_or_default();
                    items.push(Item { src: full, target, kind: Kind::Symlink(t), mode: 0o777 });
                } else {
                    items.push(Item { src: full, target, kind: Kind::File, mode: st.st_mode & 0o7777 });
                }
            }
        }
        Ok(())
    }

    fn step_copy(&mut self, st: &mut StageState, a: &CopyArgs, add: bool, text: &str) -> Result<(), String> {
        let look = |k: &str| st.var(k);
        let mut words = dockerfile::words(&a.raw, &look);
        let dest_raw = words.pop().ok_or("missing destination")?;
        let srcs = words;
        if srcs.is_empty() {
            return fail("missing source");
        }
        let dest = copyplan::plan_dest(st.workdir(), &dest_raw, srcs.len());
        let chmod_override = match &a.chmod {
            Some(m) => Some(copyplan::parse_chmod(&dockerfile::word(m, &look)).ok_or("bad --chmod")?),
            None => None,
        };
        let own_dirs = paths::lowerdirs(&st.layers);
        let owner = match &a.chown {
            Some(o) => {
                let spec_s = dockerfile::word(o, &look);
                let r = spec::resolve_user(
                    &spec_s,
                    &crate::run::read_in_container("", &own_dirs, "/etc/passwd"),
                    &crate::run::read_in_container("", &own_dirs, "/etc/group"),
                )?;
                r.map_or((0, 0), |r| (r.uid, r.gid))
            }
            None => (0, 0),
        };

        let mut items: Vec<Item> = Vec::new();
        let mut cacheable = true;
        if let Some(from) = &a.from {
            let from = dockerfile::word(from, &look);
            let layers = match self.df.stage_index(&from).and_then(|i| self.done.get(i).and_then(Option::as_ref)) {
                Some(s) => s.layers.clone(),
                None => {
                    let store = images::sanitize_name(&from);
                    if !images::image_exists(&store) {
                        crate::oci::pull_image(&from, false)?;
                    }
                    images::load_layers(&store)
                }
            };
            let dirs = paths::lowerdirs(&layers);
            let fs = DiskFs { dirs: dirs.clone() };
            let stack = Stack::new(&fs, dirs.len());
            for s in &srcs {
                let rel = copyplan::absolutize("/", s);
                let rel = rel.trim_start_matches('/');
                self.plan_stack(&stack, &dirs, rel, "", &dest, &mut items)?;
            }
        } else {
            for s in &srcs {
                if add && copyplan::is_url(s) {
                    cacheable = false;
                    let name = s.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("download");
                    let tmp = format!("{}/dl-{}", paths::CACHE_BASE, self.fresh_name("url"));
                    download(s, &tmp)?;
                    items.push(Item {
                        src: tmp,
                        target: copyplan::target_for(&dest, false, "", name),
                        kind: Kind::File,
                        mode: 0o600,
                    });
                    continue;
                }
                for rel in self.expand_src(s)? {
                    self.plan_ctx(&rel, &dest, add, &mut items)?;
                }
            }
        }
        if items.is_empty() {
            return fail("nothing to copy");
        }

        // Cache key covers every byte and attribute that will land in the layer.
        let mut h = Sha256::new();
        h.update(st.parent_id().as_bytes());
        h.update(format!("{add}|{owner:?}|{chmod_override:?}").as_bytes());
        for it in &items {
            h.update(it.target.as_bytes());
            h.update(format!("|{}|", it.mode).as_bytes());
            match &it.kind {
                Kind::Dir => h.update(b"d"),
                Kind::Symlink(t) => h.update(format!("l{t}").as_bytes()),
                Kind::File | Kind::Archive { .. } => {
                    h.update(fsx::sha256_file(&it.src).unwrap_or_default().as_bytes());
                }
            }
            h.update([0u8]);
        }
        let key = hex(h);

        let cached = if cacheable { self.cache_get(&key) } else { None };
        let c = match cached {
            Some(c) => {
                println(" ---> Using cache");
                c
            }
            None => {
                let c = self.exec_copy(st, &items, owner, chmod_override)?;
                if cacheable {
                    self.cache_put(&key, &c);
                }
                c
            }
        };
        for it in &items {
            if it.src.starts_with(paths::CACHE_BASE) {
                libakuma::unlink(&it.src);
            }
        }
        st.push_layer(&c, text);
        Ok(())
    }

    fn exec_copy(
        &mut self,
        st: &StageState,
        items: &[Item],
        owner: (u32, u32),
        chmod_override: Option<u32>,
    ) -> Result<Committed, String> {
        let name = self.fresh_name("copy");
        let upper = images::container_upper(&name);
        if !mkdir_p(&upper) {
            return fail(format!("cannot create {upper}"));
        }
        let dirs = paths::lowerdirs(&st.layers);
        let fs = DiskFs { dirs: dirs.clone() };
        let stack = Stack::new(&fs, dirs.len());

        let result = (|| -> Result<Committed, String> {
            for it in items {
                let mode = chmod_override.unwrap_or(it.mode);
                match &it.kind {
                    Kind::Dir => ensure_dir(&upper, &stack, &dirs, &it.target, mode, owner)?,
                    Kind::File => {
                        ensure_dir(&upper, &stack, &dirs, fsx::parent_of(&it.target), 0o755, owner)?;
                        let dst = format!("{upper}{}", it.target);
                        fsx::copy_file(&it.src, &dst)?;
                        chmod(&dst, mode);
                        lchown(&dst, owner.0, owner.1);
                    }
                    Kind::Symlink(t) => {
                        ensure_dir(&upper, &stack, &dirs, fsx::parent_of(&it.target), 0o755, owner)?;
                        let dst = format!("{upper}{}", it.target);
                        libakuma::unlink(&dst);
                        if symlink(t, &dst) < 0 {
                            return fail(format!("cannot create symlink {dst}"));
                        }
                        lchown(&dst, owner.0, owner.1);
                    }
                    Kind::Archive { gzip } => {
                        ensure_dir(&upper, &stack, &dirs, &it.target, 0o755, owner)?;
                        let opts = akuma_tar::ExtractOptions { gzip: *gzip, verbose: false, ..Default::default() };
                        akuma_tar::extract_file(&it.src, &format!("{upper}{}", it.target), &opts)
                            .map_err(|e| format!("ADD {}: {}", it.src, e.describe()))?;
                    }
                }
            }
            commit::commit_upper(&upper)
        })();
        fsx::remove_tree(&images::container_dir(&name));
        result
    }
}

/// Create `path` (absolute, in the image) and its ancestors inside `upper`.
///
/// A directory the layers below already have must keep **its** mode and owner:
/// overlay semantics let an upper directory's metadata win, so a naive `mkdir`
/// of `/tmp` in a COPY layer would silently turn `1777` into `0755`. A directory
/// that is new gets `new_mode` and `owner`.
fn ensure_dir(
    upper: &str,
    stack: &Stack<DiskFs>,
    dirs: &[String],
    path: &str,
    new_mode: u32,
    owner: (u32, u32),
) -> Result<(), String> {
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur.push('/');
        cur.push_str(seg);
        let dst = format!("{upper}{cur}");
        if fsx::exists(&dst) {
            continue;
        }
        if libakuma::mkdir(&dst) < 0 && !fsx::exists(&dst) {
            return fail(format!("cannot create directory {cur}"));
        }
        let rel = cur.trim_start_matches('/');
        let existing = stack.resolve(rel).filter(|f| f.node == Node::Dir);
        match existing.and_then(|f| fsx::stat(&join(&dirs[f.top], rel)).ok()) {
            Some(st) => {
                chmod(&dst, st.st_mode & 0o7777);
                lchown(&dst, st.st_uid, st.st_gid);
            }
            None => {
                chmod(&dst, new_mode);
                lchown(&dst, owner.0, owner.1);
            }
        }
    }
    Ok(())
}

fn download(url: &str, dest: &str) -> Result<(), String> {
    libakuma_tls::download_file(url, dest).map_err(|e| format!("download of {url} failed: {e:?}"))
}

// ───────────────────────────── entry point ─────────────────────────────

pub fn cmd_build(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    let opts = match buildopts::parse_build_args(&argv) {
        Ok(o) => o,
        Err(e) => {
            match e {
                BuildArgsError::MissingValue(f) => {
                    print("box build: ");
                    print(f);
                    print(" requires a value\n");
                }
                BuildArgsError::Unknown(f) => {
                    print("box build: unknown option ");
                    println(&f);
                }
                BuildArgsError::ExtraArgument(a) => {
                    print("box build: unexpected argument ");
                    println(&a);
                }
                BuildArgsError::NoContext => {}
            }
            print(USAGE);
            exit(1);
        }
    };
    if opts.tags.is_empty() {
        print("box build: at least one -t <name> is required\n");
        print(USAGE);
        exit(1);
    }

    let ctx = String::from(opts.context.trim_end_matches('/'));
    let ctx = if ctx.is_empty() { String::from("/") } else { ctx };
    let df_path = opts.file.clone().unwrap_or_else(|| format!("{ctx}/Dockerfile"));
    let Some(df_text) = fsx::read_text(&df_path) else {
        print("box build: cannot read ");
        println(&df_path);
        exit(1);
    };
    let df = match dockerfile::parse(&df_text) {
        Ok(d) => d,
        Err(e) => {
            print("box build: ");
            print(&df_path);
            print(":");
            libakuma::print_dec(e.line);
            print(": ");
            println(&e.message);
            exit(1);
        }
    };
    let target = match &opts.target {
        Some(t) => match df.stage_index(t) {
            Some(i) => i,
            None => {
                print("box build: no stage named '");
                print(t);
                print("'\n");
                exit(1);
            }
        },
        None => df.stages.len() - 1,
    };
    let ignore_text = fsx::read_text(&format!("{df_path}.dockerignore"))
        .or_else(|| fsx::read_text(&format!("{ctx}/.dockerignore")))
        .unwrap_or_default();

    images::ensure_base_dir();
    let mut b = Builder {
        df: &df,
        ctx,
        ignore: Ignore::parse(&ignore_text),
        cli_args: opts.build_args.clone(),
        used_cli: Vec::new(),
        no_cache: opts.no_cache,
        seq: 0,
        done: (0..df.stages.len()).map(|_| None).collect(),
        global: Vec::new(),
    };
    b.init_globals();

    let mut final_state = None;
    for i in df.closure(target) {
        match b.run_stage(&df.stages[i]) {
            Ok(s) => {
                if i == target {
                    final_state = Some(s.clone());
                }
                b.done[i] = Some(s);
            }
            Err(e) => {
                print("box build: ");
                println(&e);
                exit(1);
            }
        }
    }
    let st = final_state.expect("target stage built");

    for (k, _) in &b.cli_args {
        if !b.used_cli.contains(k) {
            print("[Warning] one or more build-args [");
            print(k);
            print("] were not consumed\n");
        }
    }

    let mut config = st.config.clone();
    config.created = rfc3339(fsx::now_secs());
    for tag in &opts.tags {
        let store = images::sanitize_name(tag);
        if let Err(e) = images::write_image(&store, &config, &st.layers, &st.sizes) {
            print("box build: ");
            println(&e);
            exit(1);
        }
    }
    let id = boxlib::layer::sha256_digest(config.to_json().as_bytes());
    print("Successfully built ");
    println(&id[7..19]);
    for tag in &opts.tags {
        print("Successfully tagged ");
        println(tag);
    }
    exit(0);
}
