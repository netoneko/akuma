//! Syscall wrappers over the box ABI — the memory layout the kernel reads
//! directly out of userspace (`src/syscall/proc.rs`), and the syscall numbers
//! that name it. This is the only place in userspace that should restate
//! either: previously `box` and `herd` each wrote their own copy, and nothing
//! checked that they still agreed (`docs/archive/HERD_PLUS_BOX.md`).
//!
//! `SpawnOptions` and the syscall numbers are plain data, so they build
//! either way; the wrapper functions call `libakuma::syscall` and are gated
//! behind the `akuma` feature like the rest of the binary-only half of this
//! crate. That split is what lets the layout test at the bottom of this file
//! run on the host without `libakuma`, which cannot link against std.

/// The kernel's spawn-with-options ABI. Layout is pinned on both sides of the
/// userspace/kernel boundary: this definition by the host test below, the
/// kernel's by a `const` assertion beside its own copy
/// (`src/syscall/proc.rs`). Either side changing shape fails to build on that
/// side, instead of silently handing the kernel a struct whose `box_id` lands
/// where it expects `stdin_len`.
#[repr(C)]
#[derive(Default)]
pub struct SpawnOptions {
    pub cwd_ptr: u64,
    pub cwd_len: usize,
    pub root_dir_ptr: u64,
    pub root_dir_len: usize,
    pub args_ptr: u64,
    pub args_len: usize,
    pub stdin_ptr: u64,
    pub stdin_len: usize,
    pub box_id: u64,
    /// A NULL-terminated `char *envp[]`, or 0 for "use the kernel's default
    /// environment". Built by [`spawn_ext`] from its `env` argument.
    ///
    /// Appended after `box_id` so the offsets above are unchanged: the kernel
    /// negotiates the struct's size (it is passed as `SPAWN_EXT`'s third
    /// argument), so a binary built against the older 72-byte layout still runs
    /// on a newer kernel and simply gets the default environment.
    pub env_ptr: u64,
    pub env_len: usize,
}

pub const SYSCALL_SPAWN_EXT: u64 = 315;
pub const SYSCALL_REGISTER_BOX: u64 = 316;
pub const SYSCALL_KILL_BOX: u64 = 317;
pub const SYSCALL_SET_BOX_STACK: u64 = 324;

#[cfg(feature = "akuma")]
mod calls {
    use super::{SpawnOptions, SYSCALL_KILL_BOX, SYSCALL_REGISTER_BOX, SYSCALL_SET_BOX_STACK, SYSCALL_SPAWN_EXT};
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use libakuma::SpawnResult;

    /// Register (or update) a box's name/root/primary-pid in the kernel's
    /// `/proc/boxes` table. Callers register once with `primary_pid` 0 to
    /// create the box's mount namespace before spawning into it, then again
    /// with the real pid once `spawn_ext` returns one.
    pub fn register_box(box_id: u64, name: &str, root_dir: &str, primary_pid: u32) {
        libakuma::syscall(
            SYSCALL_REGISTER_BOX,
            box_id,
            name.as_ptr() as u64,
            name.len() as u64,
            root_dir.as_ptr() as u64,
            root_dir.len() as u64,
            primary_pid as u64,
        );
    }

    /// Kill every process in a box and drop its registry entry. `box_id` 0 is
    /// the host; callers must reject that before calling this.
    pub fn kill_box(box_id: u64) -> bool {
        libakuma::syscall(SYSCALL_KILL_BOX, box_id, 0, 0, 0, 0, 0) == 0
    }

    /// Tell the kernel a box uses the NetBSD rump network stack (stack id 1),
    /// so the kernel routes that box's AF_INET syscalls to its rump_server.
    pub fn set_box_stack_rump(box_id: u64) {
        libakuma::syscall(SYSCALL_SET_BOX_STACK, box_id, 1, 0, 0, 0, 0);
    }

    /// Spawn `path` under `options`. `args` excludes `path` itself; the
    /// kernel wants a NUL-terminated argv pointer array
    /// (`[path\0, arg\0…, null]`), not a flat buffer — building that array is
    /// what this wraps.
    pub fn spawn_ext(
        path: &str,
        args: Option<&[&str]>,
        stdin: Option<&[u8]>,
        options: &mut SpawnOptions,
    ) -> Option<SpawnResult> {
        spawn_ext_env(path, args, None, stdin, options)
    }

    /// [`spawn_ext`] plus an explicit environment.
    ///
    /// `env` is the child's **whole** environment, already composed — an empty
    /// or absent list means "use the kernel's default", not "no variables". A
    /// caller passing only the user's `-e` overrides would silently drop `PATH`,
    /// which is the one variable an image's own `Env` always sets.
    pub fn spawn_ext_env(
        path: &str,
        args: Option<&[&str]>,
        env: Option<&[String]>,
        stdin: Option<&[u8]>,
        options: &mut SpawnOptions,
    ) -> Option<SpawnResult> {
        let mut argv = Vec::new();
        let path_terminated = format!("{}\0", path);
        argv.push(path_terminated.as_ptr());

        let mut args_terminated = Vec::new();
        if let Some(slice) = args {
            for a in slice {
                args_terminated.push(format!("{}\0", a));
            }
        }
        for s in &args_terminated {
            argv.push(s.as_ptr());
        }
        argv.push(core::ptr::null());

        options.args_ptr = argv.as_ptr() as u64;
        options.args_len = argv.len();

        // Same NUL-terminated, NULL-terminated shape as argv. Both the owning
        // Strings and the pointer array must outlive the syscall below, which is
        // why they are bound here rather than built inline.
        let env_terminated: Vec<String> = match env {
            Some(vars) if !vars.is_empty() => {
                vars.iter().map(|v| format!("{}\0", v)).collect()
            }
            _ => Vec::new(),
        };
        let mut envp: Vec<*const u8> = Vec::new();
        if !env_terminated.is_empty() {
            for v in &env_terminated {
                envp.push(v.as_ptr());
            }
            envp.push(core::ptr::null());
            options.env_ptr = envp.as_ptr() as u64;
            options.env_len = envp.len();
        }

        if let Some(s) = stdin {
            options.stdin_ptr = s.as_ptr() as u64;
            options.stdin_len = s.len();
        }

        let result = libakuma::syscall(
            SYSCALL_SPAWN_EXT,
            path_terminated.as_ptr() as u64,
            options as *const _ as u64,
            // The struct's size, so a kernel that knows more fields than this
            // build does reads only the ones actually written.
            core::mem::size_of::<SpawnOptions>() as u64,
            0,
            0,
            0,
        );

        if (result as i64) < 0 {
            return None;
        }
        Some(SpawnResult {
            pid: (result & 0xFFFF_FFFF) as u32,
            stdout_fd: ((result >> 32) & 0xFFFF_FFFF) as u32,
        })
    }
}

#[cfg(feature = "akuma")]
pub use calls::{kill_box, register_box, set_box_stack_rump, spawn_ext, spawn_ext_env};

#[cfg(test)]
mod tests {
    use super::SpawnOptions;

    // Kept in exact agreement with the `const _` assertion beside the
    // kernel's own `SpawnOptions` in `src/syscall/proc.rs` — see
    // docs/archive/HERD_PLUS_BOX.md, "Mechanics".
    #[test]
    fn matches_the_kernels_abi() {
        assert_eq!(core::mem::size_of::<SpawnOptions>(), 88);
        assert_eq!(core::mem::offset_of!(SpawnOptions, cwd_ptr), 0);
        assert_eq!(core::mem::offset_of!(SpawnOptions, cwd_len), 8);
        assert_eq!(core::mem::offset_of!(SpawnOptions, root_dir_ptr), 16);
        assert_eq!(core::mem::offset_of!(SpawnOptions, root_dir_len), 24);
        assert_eq!(core::mem::offset_of!(SpawnOptions, args_ptr), 32);
        assert_eq!(core::mem::offset_of!(SpawnOptions, args_len), 40);
        assert_eq!(core::mem::offset_of!(SpawnOptions, stdin_ptr), 48);
        assert_eq!(core::mem::offset_of!(SpawnOptions, stdin_len), 56);
        assert_eq!(core::mem::offset_of!(SpawnOptions, box_id), 64);
        // Appended, not inserted — every offset above must stay put, or a
        // `/bin/box` older than the kernel writes its `box_id` where the kernel
        // reads `stdin_len`.
        assert_eq!(core::mem::offset_of!(SpawnOptions, env_ptr), 72);
        assert_eq!(core::mem::offset_of!(SpawnOptions, env_len), 80);
    }
}
