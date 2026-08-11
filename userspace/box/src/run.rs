//! `box run` — start a container from a pulled OCI image.
//!
//! The image's layers stay read-only and shared; the container gets a private
//! writable directory stacked on top of them by the kernel's overlay
//! filesystem. Nothing a container does can reach the image.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{
    close, exit, mkdir_p, open, open_flags, print, println, read_dir, read_fd, waitpid, write_fd,
};

use crate::images;
use crate::json;
use crate::{SpawnOptions, spawn_ext};

const SYSCALL_REGISTER_BOX: u64 = 316;
const SYSCALL_KILL_BOX: u64 = 317;

/// What the image's config says to run. Entrypoint and Cmd stay separate
/// because they are overridden separately.
pub struct ImageProcess {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub working_dir: String,
}

impl ImageProcess {
    /// Compose the command line the way `docker run` does: arguments on the
    /// command line replace **Cmd** and are passed to the Entrypoint. Only an
    /// image with no Entrypoint at all treats them as the program to run.
    pub fn argv_with(&self, user_args: &[String]) -> Vec<String> {
        let mut argv = self.entrypoint.clone();
        if user_args.is_empty() {
            argv.extend(self.cmd.iter().cloned());
        } else {
            argv.extend(user_args.iter().cloned());
        }
        argv
    }
}

pub fn image_process(store: &str) -> ImageProcess {
    let mut entrypoint = Vec::new();
    let mut cmd = Vec::new();
    let mut working_dir = String::from("/");

    if let Some(config_json) = images::load_config(store) {
        if let Some(config_obj) = json::extract_object(&config_json, "config") {
            if let Some(ep) = json::extract_string_array(config_obj, "Entrypoint") {
                entrypoint = ep;
            }
            if let Some(c) = json::extract_string_array(config_obj, "Cmd") {
                cmd = c;
            }
            if let Some(wd) = json::extract_string(config_obj, "WorkingDir") {
                if !wd.is_empty() {
                    working_dir = wd;
                }
            }
        }
    }

    ImageProcess { entrypoint, cmd, working_dir }
}

/// Resolve a bare program name against the container's `PATH`, the way a shell
/// would. An image's Entrypoint is usually just `curl`, and the kernel's spawn
/// takes a path, not a name.
fn resolve_in_container(upper: &str, lowerdirs: &[String], program: &str) -> String {
    if program.contains('/') {
        return String::from(program);
    }
    for dir in ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"] {
        let candidate = format!("{}{}/{}", upper, dir, program);
        if images::path_exists(&candidate) {
            return format!("{}/{}", dir, program);
        }
        for lower in lowerdirs {
            let candidate = format!("{}{}/{}", lower, dir, program);
            if images::path_exists(&candidate) {
                return format!("{}/{}", dir, program);
            }
        }
    }
    String::from(program)
}

fn copy_file(src: &str, dst: &str) -> bool {
    let sfd = open(src, open_flags::O_RDONLY);
    if sfd < 0 {
        return false;
    }
    let mut body = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = read_fd(sfd, &mut tmp);
        if n <= 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n as usize]);
    }
    close(sfd);

    let dfd = open(dst, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if dfd < 0 {
        return false;
    }
    write_fd(dfd, &body);
    close(dfd);
    true
}

/// Give the container the two files an OCI image never ships but every
/// networked program expects. Docker injects these the same way.
fn inject_runtime_files(upper: &str, hostname: &str) {
    let etc = format!("{}/etc", upper);
    mkdir_p(&etc);

    let resolv = format!("{}/resolv.conf", etc);
    if !copy_file("/etc/resolv.conf", &resolv) {
        // No host resolver config to inherit; a sane default beats no file at
        // all, since musl treats a missing resolv.conf as "no nameservers".
        let fd = open(&resolv, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
        if fd >= 0 {
            write_fd(fd, b"nameserver 10.0.2.3\n");
            close(fd);
        }
    }

    let hosts = format!("{}/hosts", etc);
    let fd = open(&hosts, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd >= 0 {
        write_fd(fd, format!("127.0.0.1 localhost {}\n", hostname).as_bytes());
        close(fd);
    }
}

/// Depth-first delete. Used only for `--rm` on a container directory, whose
/// contents this process created.
fn remove_tree(path: &str) {
    if let Some(entries) = read_dir(path) {
        for entry in entries {
            let child = format!("{}/{}", path, entry.name);
            if entry.is_dir {
                remove_tree(&child);
            } else {
                libakuma::unlink(&child);
            }
        }
    }
    libakuma::rmdir(path);
}

fn box_id_for(name: &str) -> u64 {
    let mut id = 0u64;
    for b in name.as_bytes() {
        id = id.wrapping_mul(31).wrapping_add(u64::from(*b));
    }
    if id == 0 { 1 } else { id }
}

pub fn cmd_run(args: libakuma::Args) -> ! {
    let mut args = args.peekable();

    let mut rm = false;
    let mut detached = false;
    let mut interactive = false;
    let mut name: Option<String> = None;
    let mut workdir_override: Option<String> = None;
    let mut entrypoint_override: Option<String> = None;
    let mut image_ref: Option<String> = None;
    let mut cmd_argv: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg {
            "--rm" => rm = true,
            "-d" | "--detached" => detached = true,
            "-i" | "-I" | "-it" | "--interactive" => interactive = true,
            "--name" => {
                name = Some(String::from(args.next().unwrap_or_else(|| {
                    print("box run: --name requires a value\n");
                    exit(1);
                })));
            }
            "--entrypoint" => {
                entrypoint_override = Some(String::from(args.next().unwrap_or_else(|| {
                    print("box run: --entrypoint requires a value\n");
                    exit(1);
                })));
            }
            "-w" | "--workdir" => {
                workdir_override = Some(String::from(args.next().unwrap_or_else(|| {
                    print("box run: --workdir requires a value\n");
                    exit(1);
                })));
            }
            _ => {
                image_ref = Some(String::from(arg));
                for a in args {
                    cmd_argv.push(String::from(a));
                }
                break;
            }
        }
    }

    let Some(image_ref) = image_ref else {
        print("Usage: box run [--rm] [-d] [-i] [--name X] [-w dir] <image> [cmd [args...]]\n");
        exit(1);
    };

    let store = images::sanitize_name(&image_ref);
    if !images::image_exists(&store) {
        print("box run: image '");
        print(&image_ref);
        print("' not found. Run 'box pull ");
        print(&image_ref);
        print("' first.\n");
        exit(1);
    }

    let lowerdirs = images::overlay_lowerdirs(&store);
    if lowerdirs.is_empty() {
        print("box run: image '");
        print(&store);
        print("' has no layer list — re-pull it to rebuild the layer store.\n");
        exit(1);
    }
    for dir in &lowerdirs {
        if !images::dir_exists(dir) {
            print("box run: missing layer ");
            println(dir);
            print("Re-pull the image.\n");
            exit(1);
        }
    }

    // A container id that is stable when named and unique when not.
    let container = name.clone().unwrap_or_else(|| format!("{}-{}", store, libakuma::uptime()));
    let box_id = box_id_for(&container);

    let croot = images::container_dir(&container);
    let upper = images::container_upper(&container);
    images::ensure_base_dir();
    if !mkdir_p(&upper) {
        print("box run: failed to create ");
        println(&upper);
        exit(1);
    }
    inject_runtime_files(&upper, &container);

    // The box's root is the container directory, which is what the kernel
    // validates and jails to; the overlay then replaces that jail with the
    // union of the image layers over the container's upper directory.
    libakuma::syscall(
        SYSCALL_REGISTER_BOX,
        box_id,
        container.as_ptr() as u64,
        container.len() as u64,
        croot.as_ptr() as u64,
        croot.len() as u64,
        0,
    );

    let rc = libakuma::mount_overlay_root(box_id, &lowerdirs, &upper);
    if rc != 0 {
        print("box run: overlay mount failed: errno ");
        println(&format!("{}", -rc));
        libakuma::syscall(SYSCALL_KILL_BOX, box_id, 0, 0, 0, 0, 0);
        exit(1);
    }

    // Every image ships an empty /proc and expects something mounted there —
    // without it `ps` fails and even `ls /` complains about the entry. Mounting
    // is host-only, so it happens here, from box 0, before the container starts.
    if libakuma::mount_in_ns(box_id, "/proc", "proc", None) != 0 {
        print("box run: warning: could not mount /proc in the container\n");
    }

    let mut image_proc = image_process(&store);
    if let Some(ep) = entrypoint_override {
        // Docker's `--entrypoint`: replace the entrypoint outright and let the
        // command line supply its arguments. The image's own Cmd is dropped,
        // since it was written for a different program.
        image_proc.entrypoint = alloc::vec![ep];
        image_proc.cmd = Vec::new();
    }
    let argv = image_proc.argv_with(&cmd_argv);
    if argv.is_empty() {
        print("box run: image has no Entrypoint or Cmd, and no command was given\n");
        libakuma::syscall(SYSCALL_KILL_BOX, box_id, 0, 0, 0, 0, 0);
        exit(1);
    }
    let working_dir = workdir_override.unwrap_or(image_proc.working_dir);

    let path = resolve_in_container(&upper, &lowerdirs, &argv[0]);
    let rest: Vec<&str> = argv[1..].iter().map(String::as_str).collect();

    let mut options = SpawnOptions {
        cwd_ptr: working_dir.as_ptr() as u64,
        cwd_len: working_dir.len(),
        root_dir_ptr: 0,
        root_dir_len: 0,
        args_ptr: 0,
        args_len: 0,
        stdin_ptr: 0,
        stdin_len: 0,
        box_id,
    };

    print("box: running '");
    print(&path);
    print("' in ");
    print(&container);
    print(" (");
    libakuma::print_dec(lowerdirs.len());
    print(" layers, ID=");
    libakuma::print_hex(box_id as usize);
    print(")\n");

    let rest_opt = if rest.is_empty() { None } else { Some(rest.as_slice()) };
    let Some(res) = spawn_ext(&path, rest_opt, None, &mut options) else {
        print("box run: failed to spawn ");
        println(&path);
        if rm {
            libakuma::syscall(SYSCALL_KILL_BOX, box_id, 0, 0, 0, 0, 0);
            remove_tree(&croot);
        }
        exit(1);
    };

    libakuma::syscall(
        SYSCALL_REGISTER_BOX,
        box_id,
        container.as_ptr() as u64,
        container.len() as u64,
        croot.as_ptr() as u64,
        croot.len() as u64,
        u64::from(res.pid),
    );

    if detached {
        println(&format!("Started PID {} in {} (detached)", res.pid, container));
        exit(0);
    }

    // Attaching is what pumps the child's stdout to this terminal, so a
    // foreground run always does it — `-i` only matters for keeping stdin
    // hooked up. Without this the container runs to completion in silence.
    let _ = interactive;
    if libakuma::reattach(res.pid) != 0 {
        print("box run: reattach failed\n");
        exit(1);
    }

    let code = loop {
        if let Some((_, code)) = waitpid(res.pid) {
            break code;
        }
        libakuma::sleep_ms(50);
    };

    if rm {
        libakuma::syscall(SYSCALL_KILL_BOX, box_id, 0, 0, 0, 0, 0);
        remove_tree(&croot);
    }
    exit(code);
}
