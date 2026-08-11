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

use boxlib::spec::{self, ImageProcess, RunArgsError};
use boxlib::sys::{self, SpawnOptions, spawn_ext};

use crate::images;

const USAGE: &str =
    "Usage: box run [--rm] [-d] [-i] [--name X] [-w dir] <image> [cmd [args...]]\n";

/// What a pulled image says to run. An image with no readable config yields an
/// empty process, which `cmd_run` reports rather than spawning.
pub fn image_process(store: &str) -> ImageProcess {
    images::load_config(store)
        .map(|json| spec::image_process_from_config(&json))
        .unwrap_or_default()
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

pub fn cmd_run(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    let parsed = match spec::parse_run_args(&argv) {
        Ok(p) => p,
        Err(RunArgsError::MissingValue(flag)) => {
            print("box run: ");
            print(flag);
            print(" requires a value\n");
            exit(1);
        }
        Err(RunArgsError::NoImage) => {
            print(USAGE);
            exit(1);
        }
    };

    let image_ref = parsed.image;
    let rm = parsed.rm;
    let detached = parsed.detached;
    let interactive = parsed.interactive;
    let cmd_argv = parsed.argv;

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
    let container = parsed
        .name
        .clone()
        .unwrap_or_else(|| format!("{}-{}", store, libakuma::uptime()));
    let box_id = spec::box_id_for(&container);

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
    sys::register_box(box_id, &container, &croot, 0);

    let rc = libakuma::mount_overlay_root(box_id, &lowerdirs, &upper);
    if rc != 0 {
        print("box run: overlay mount failed: errno ");
        println(&format!("{}", -rc));
        sys::kill_box(box_id);
        exit(1);
    }

    // Every image ships an empty /proc and expects something mounted there —
    // without it `ps` fails and even `ls /` complains about the entry. Mounting
    // is host-only, so it happens here, from box 0, before the container starts.
    if libakuma::mount_in_ns(box_id, "/proc", "proc", None) != 0 {
        print("box run: warning: could not mount /proc in the container\n");
    }

    let mut image_proc = image_process(&store);
    if let Some(ep) = &parsed.entrypoint {
        image_proc.override_entrypoint(ep);
    }
    let argv = image_proc.argv_with(&cmd_argv);
    if argv.is_empty() {
        print("box run: image has no Entrypoint or Cmd, and no command was given\n");
        sys::kill_box(box_id);
        exit(1);
    }
    let working_dir = parsed.workdir.clone().unwrap_or(image_proc.working_dir);

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
            sys::kill_box(box_id);
            remove_tree(&croot);
        }
        exit(1);
    };

    sys::register_box(box_id, &container, &croot, res.pid);

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
        sys::kill_box(box_id);
        remove_tree(&croot);
    }
    exit(code);
}
