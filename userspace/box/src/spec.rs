//! What to run, and what the command line asked for.
//!
//! Two halves of the same decision: the image config says what the image wants
//! to run, `box run`'s flags say what the user wants instead, and
//! [`ImageProcess::argv_with`] resolves the two the way `docker run` does.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::json;

/// What the image's config says to run. Entrypoint and Cmd stay separate
/// because they are overridden separately.
#[derive(Debug, PartialEq, Eq)]
pub struct ImageProcess {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub working_dir: String,
    /// The image's `config.Env`, in the order the image lists it.
    pub env: Vec<String>,
    /// The image's `config.User` — `""` for root, else `user`, `uid`,
    /// `user:group`, `uid:gid` and the mixed forms. Resolved by
    /// [`resolve_user`] against the image's own `/etc/passwd` and `/etc/group`.
    pub user: String,
}

impl Default for ImageProcess {
    /// Nothing to run, starting at the root — an image whose config could not
    /// be read must not end up with `""` as its working directory.
    fn default() -> Self {
        Self {
            entrypoint: Vec::new(),
            cmd: Vec::new(),
            working_dir: String::from("/"),
            env: Vec::new(),
            user: String::new(),
        }
    }
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

    /// Docker's `--entrypoint`: replace the entrypoint outright and drop the
    /// image's Cmd, which was written as arguments for a different program.
    pub fn override_entrypoint(&mut self, program: &str) {
        self.entrypoint = alloc::vec![String::from(program)];
        self.cmd = Vec::new();
    }
}

/// Read an image config blob.
///
/// Only the top-level `config` object is consulted. Images built before
/// Docker 1.10 also carry a `container_config` with the same member names,
/// holding the *builder's* last command — running that instead is how an image
/// ends up executing `/bin/sh -c #(nop) CMD …`.
pub fn image_process_from_config(config_json: &str) -> ImageProcess {
    let working_dir = json::string_at(config_json, &["config", "WorkingDir"])
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| String::from("/"));

    ImageProcess {
        entrypoint: json::strings_at(config_json, &["config", "Entrypoint", "*"]),
        cmd: json::strings_at(config_json, &["config", "Cmd", "*"]),
        working_dir,
        env: json::strings_at(config_json, &["config", "Env", "*"]),
        user: json::string_at(config_json, &["config", "User"]).unwrap_or_default(),
    }
}

/// Who a container's first process runs as.
#[derive(Debug, PartialEq, Eq)]
pub struct RunAs {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups: every `/etc/group` entry naming the user.
    pub groups: Vec<u32>,
}

/// One `/etc/passwd` line's name, uid and primary gid.
fn passwd_entries(passwd: &str) -> impl Iterator<Item = (&str, u32, u32)> {
    passwd.lines().filter_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        let _pw = f.next()?;
        Some((name, f.next()?.parse().ok()?, f.next()?.parse().ok()?))
    })
}

/// One `/etc/group` line's name, gid and member list.
fn group_entries(group: &str) -> impl Iterator<Item = (&str, u32, &str)> {
    group.lines().filter_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        let _pw = f.next()?;
        Some((name, f.next()?.parse().ok()?, f.next().unwrap_or("")))
    })
}

/// Resolve an OCI `User` (`config.User`, or `box run -u`) the way runc does,
/// against the **image's** `passwd` and `group` files — never the host's: a
/// name means whatever the image says it means.
///
/// - `""` is root, unchanged: `Ok(None)`.
/// - `name` / `uid`: the uid, and the primary gid from that user's `passwd`
///   line. A numeric uid with no line gets gid 0, as in runc.
/// - `…:group` / `…:gid`: that group instead of the passwd one.
/// - Supplementary groups are the `group` entries listing the user's *name*,
///   so a numeric uid with no passwd line has none.
///
/// A name that is not in the image is an error, not root: starting a container
/// as root because its `User` could not be found is the one wrong answer.
pub fn resolve_user(user: &str, passwd: &str, group: &str) -> Result<Option<RunAs>, String> {
    let user = user.trim();
    if user.is_empty() {
        return Ok(None);
    }
    let (u, g) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    let entry = match u.parse::<u32>() {
        Ok(n) => passwd_entries(passwd).find(|e| e.1 == n),
        Err(_) => Some(
            passwd_entries(passwd)
                .find(|e| e.0 == u)
                .ok_or_else(|| format!("user '{u}' not found in the image's /etc/passwd"))?,
        ),
    };
    let uid = u.parse::<u32>().ok().or(entry.map(|e| e.1)).unwrap_or(0);
    let gid = match g {
        Some(g) => match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => group_entries(group)
                .find(|e| e.0 == g)
                .map(|e| e.1)
                .ok_or_else(|| format!("group '{g}' not found in the image's /etc/group"))?,
        },
        None => entry.map_or(0, |e| e.2),
    };
    let mut groups = Vec::new();
    if let Some((name, _, _)) = entry {
        for (_, gid, members) in group_entries(group) {
            if members.split(',').any(|m| m == name) && !groups.contains(&gid) {
                groups.push(gid);
            }
        }
    }
    Ok(Some(RunAs { uid, gid, groups }))
}

/// The name half of a `KEY=VALUE` entry, or the whole string when there is no
/// `=` (a bare `-e KEY` passthrough that found nothing to pass through).
#[must_use]
pub fn env_key(entry: &str) -> &str {
    match entry.find('=') {
        Some(i) => &entry[..i],
        None => entry,
    }
}

/// Compose a container's environment the way `docker run` does: the image's
/// `Env` first, then `-e` entries, which **override by name** rather than
/// appending a second entry for the same key.
///
/// Two rules that are easy to get wrong and that the tests pin:
///
///  * **Order is the image's, not the overrides'.** An override replaces the
///    value in place, so a program that walks `environ` sees the image's layout.
///    Appending instead leaves two entries for one key, and which one wins is
///    then up to the libc.
///  * **`PATH` is guaranteed.** The kernel treats a non-empty environment as the
///    *whole* environment, so a composed list that happens to lack `PATH` would
///    leave the container unable to resolve a bare program name — the exact
///    `exec: redis-server: not found` that a short `PATH` used to cause. Images
///    essentially always set it; when one does not, the default is added.
#[must_use]
pub fn compose_env(image_env: &[String], overrides: &[String]) -> Vec<String> {
    let mut out: Vec<String> = image_env.to_vec();
    for entry in overrides {
        let key = env_key(entry);
        match out.iter().position(|e| env_key(e) == key) {
            Some(i) => out[i] = entry.clone(),
            None => out.push(entry.clone()),
        }
    }
    if !out.iter().any(|e| env_key(e) == "PATH") {
        out.push(String::from(DEFAULT_PATH));
    }
    out
}

/// The environment a *service* falls back to when nothing supplies one.
///
/// Mirrors the kernel's `DEFAULT_ENV` (`crates/akuma-exec/src/process/types.rs`)
/// and must keep mirroring it: the kernel uses that list only when a spawn passes
/// **no** environment at all, so the moment a caller composes a list of its own
/// it owns every variable — a service that gained one `env =` line would
/// otherwise silently lose `HOME` and `TERM`.
///
/// Not used by `box run`: a container's environment is its image's `Env`, which
/// is Docker's rule and deliberately does not include `HOME`/`TERM`.
pub const DEFAULT_ENV: &[&str] = &[
    DEFAULT_PATH,
    "HOME=/",
    "TERM=xterm",
];

/// The `PATH` a container falls back to when its image sets none.
///
/// Same search order as the kernel's `DEFAULT_ENV` and as Docker's: local before
/// system, `sbin` before `bin` at each level.
pub const DEFAULT_PATH: &str =
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Whether there is anything to run at all.
impl ImageProcess {
    pub fn is_empty(&self) -> bool {
        self.entrypoint.is_empty() && self.cmd.is_empty()
    }
}

/// A box id derived from its name, so `box run --name web` twice means the same
/// box. Not a hash anyone should rely on — just a stable spread over u64.
pub fn box_id_for(name: &str) -> u64 {
    let mut id = 0u64;
    for b in name.as_bytes() {
        id = id.wrapping_mul(31).wrapping_add(u64::from(*b));
    }
    // Box 0 is the host. A container must never land there.
    if id == 0 {
        1
    } else {
        id
    }
}

/// What `box run`'s command line asked for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunArgs {
    pub rm: bool,
    pub detached: bool,
    pub interactive: bool,
    pub name: Option<String>,
    pub workdir: Option<String>,
    pub entrypoint: Option<String>,
    /// `-u`/`--user`: overrides the image's `config.User`, as `docker run -u`.
    pub user: Option<String>,
    /// `-e` entries in command-line order. A bare `KEY` (no `=`) is a
    /// passthrough request the caller resolves against its own environment
    /// before composing — this layer keeps it verbatim.
    pub env: Vec<String>,
    pub image: String,
    /// Everything after the image name, untouched — including anything that
    /// looks like a flag, which belongs to the container's program, not to us.
    pub argv: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunArgsError {
    /// A flag that takes a value was last on the line.
    MissingValue(&'static str),
    /// No image was named.
    NoImage,
}

/// Parse `box run`'s arguments (everything after `run`).
///
/// The first non-flag argument is the image, and **everything after it is the
/// container's**: `box run img sh -c ls` must not have `-c` read as a box flag.
pub fn parse_run_args(args: &[&str]) -> Result<RunArgs, RunArgsError> {
    /// Take the argument after a flag, or report the flag as incomplete.
    fn value_of(
        args: &[&str],
        i: &mut usize,
        flag: &'static str,
    ) -> Result<String, RunArgsError> {
        let v = args.get(*i).ok_or(RunArgsError::MissingValue(flag))?;
        *i += 1;
        Ok(String::from(*v))
    }

    let mut out = RunArgs::default();
    let mut i = 0;

    while i < args.len() {
        let arg = args[i];
        i += 1;
        match arg {
            "--rm" => out.rm = true,
            "-d" | "--detached" => out.detached = true,
            "-i" | "-I" | "-it" | "--interactive" => out.interactive = true,
            "--name" => out.name = Some(value_of(args, &mut i, "--name")?),
            "--entrypoint" => out.entrypoint = Some(value_of(args, &mut i, "--entrypoint")?),
            "-e" | "--env" => out.env.push(value_of(args, &mut i, "--env")?),
            "-w" | "--workdir" => out.workdir = Some(value_of(args, &mut i, "--workdir")?),
            "-u" | "--user" => out.user = Some(value_of(args, &mut i, "--user")?),
            _ => {
                out.image = String::from(arg);
                out.argv = args[i..].iter().map(|a| String::from(*a)).collect();
                return Ok(out);
            }
        }
    }

    Err(RunArgsError::NoImage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| String::from(*s)).collect()
    }

    const BUSYBOX_CONFIG: &str = r#"{
        "architecture": "arm64",
        "config": {
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin"],
            "Cmd": ["sh"],
            "WorkingDir": "",
            "Entrypoint": null
        },
        "created": "2025-05-27T18:16:36Z",
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": ["sha256:abc"]}
    }"#;

    #[test]
    fn reads_cmd_from_an_image_config() {
        let p = image_process_from_config(BUSYBOX_CONFIG);
        assert_eq!(p.cmd, ["sh"]);
        assert!(p.entrypoint.is_empty());
        // An empty WorkingDir means the root, not a directory named "".
        assert_eq!(p.working_dir, "/");
    }

    #[test]
    fn reads_entrypoint_and_workdir() {
        let doc = r#"{"config": {
            "Entrypoint": ["/usr/bin/curl"],
            "Cmd": ["--help"],
            "WorkingDir": "/srv"
        }}"#;
        let p = image_process_from_config(doc);
        assert_eq!(p.entrypoint, ["/usr/bin/curl"]);
        assert_eq!(p.cmd, ["--help"]);
        assert_eq!(p.working_dir, "/srv");
    }

    #[test]
    fn ignores_the_build_time_container_config() {
        // `container_config` holds what the *build* ran last. Picking it up
        // makes the image start `/bin/sh -c #(nop) CMD ["sh"]` instead of `sh`.
        let doc = r##"{
            "container_config": {"Cmd": ["/bin/sh", "-c", "#(nop) CMD [\"sh\"]"]},
            "config": {"Cmd": ["sh"]}
        }"##;
        assert_eq!(image_process_from_config(doc).cmd, ["sh"]);
    }

    #[test]
    fn a_config_with_nothing_to_run_is_empty_not_a_panic() {
        let p = image_process_from_config(r#"{"config": {}}"#);
        assert_eq!(p, ImageProcess { working_dir: String::from("/"), ..Default::default() });
        assert!(p.argv_with(&[]).is_empty());
    }

    #[test]
    fn a_malformed_config_yields_nothing_to_run() {
        // Better a clear "no Entrypoint or Cmd" than a half-read command line.
        let p = image_process_from_config("{ this is not json");
        assert!(p.argv_with(&[]).is_empty());
    }

    #[test]
    fn image_cmd_runs_when_no_args_are_given() {
        let p = ImageProcess {
            entrypoint: vec![],
            cmd: strs(&["sh"]),
            working_dir: String::from("/"),
            env: Vec::new(),
            user: String::new(),
        };
        assert_eq!(p.argv_with(&[]), ["sh"]);
    }

    #[test]
    fn user_args_replace_cmd_and_keep_the_entrypoint() {
        let p = ImageProcess {
            entrypoint: strs(&["/usr/bin/curl"]),
            cmd: strs(&["--help"]),
            working_dir: String::from("/"),
            env: Vec::new(),
            user: String::new(),
        };
        assert_eq!(p.argv_with(&[]), ["/usr/bin/curl", "--help"]);
        assert_eq!(
            p.argv_with(&strs(&["-sS", "https://example.com"])),
            ["/usr/bin/curl", "-sS", "https://example.com"]
        );
    }

    #[test]
    fn without_an_entrypoint_user_args_are_the_program() {
        let p = ImageProcess {
            entrypoint: vec![],
            cmd: strs(&["sh"]),
            working_dir: String::from("/"),
            env: Vec::new(),
            user: String::new(),
        };
        assert_eq!(p.argv_with(&strs(&["echo", "hi"])), ["echo", "hi"]);
    }

    #[test]
    fn entrypoint_override_drops_the_image_cmd() {
        let mut p = ImageProcess {
            entrypoint: strs(&["/usr/bin/curl"]),
            cmd: strs(&["--help"]),
            working_dir: String::from("/"),
            env: Vec::new(),
            user: String::new(),
        };
        p.override_entrypoint("/bin/sh");
        assert_eq!(p.argv_with(&[]), ["/bin/sh"]);
        assert_eq!(p.argv_with(&strs(&["-c", "ls"])), ["/bin/sh", "-c", "ls"]);
    }

    #[test]
    fn reads_the_image_env() {
        let p = image_process_from_config(BUSYBOX_CONFIG);
        assert_eq!(p.env, ["PATH=/usr/local/sbin:/usr/local/bin"]);
        // An image with no Env is empty, not a panic — `compose_env` is what
        // decides that an empty list still gets a PATH.
        assert!(image_process_from_config(r#"{"config": {}}"#).env.is_empty());
    }

    #[test]
    fn env_overrides_replace_in_place() {
        // Docker's rule: `-e` replaces the image's value for that key, and the
        // image's ORDER is preserved. Appending would leave two `LANG=` entries.
        let image = strs(&["PATH=/bin", "LANG=C", "TZ=UTC"]);
        let out = compose_env(&image, &strs(&["LANG=en_US.UTF-8"]));
        assert_eq!(out, ["PATH=/bin", "LANG=en_US.UTF-8", "TZ=UTC"]);
        assert_eq!(out.iter().filter(|e| e.starts_with("LANG=")).count(), 1);
    }

    #[test]
    fn env_overrides_append_when_the_image_has_no_such_key() {
        let out = compose_env(&strs(&["PATH=/bin"]), &strs(&["REDIS_PORT=6379"]));
        assert_eq!(out, ["PATH=/bin", "REDIS_PORT=6379"]);
    }

    #[test]
    fn later_overrides_win_over_earlier_ones() {
        // `-e A=1 -e A=2` is command-line order, so the last one is the answer.
        let out = compose_env(&strs(&["PATH=/bin"]), &strs(&["A=1", "A=2"]));
        assert_eq!(out, ["PATH=/bin", "A=2"]);
    }

    #[test]
    fn a_value_containing_an_equals_sign_is_not_split_twice() {
        // Only the FIRST `=` separates name from value; `KEY=a=b` is one entry
        // whose value is `a=b`, which is what a connection string looks like.
        let out = compose_env(&[], &strs(&["DSN=host=db port=5432", "PATH=/bin"]));
        assert_eq!(out, ["DSN=host=db port=5432", "PATH=/bin"]);
        assert_eq!(env_key("DSN=host=db"), "DSN");
    }

    #[test]
    fn path_is_always_present() {
        // The kernel treats a non-empty environment as the WHOLE environment, so
        // a composed list without PATH would break every bare program name.
        assert_eq!(compose_env(&[], &[]), [DEFAULT_PATH]);
        assert_eq!(compose_env(&[], &strs(&["FOO=1"])), ["FOO=1", DEFAULT_PATH]);
        // ...but a PATH that IS supplied is never second-guessed.
        let out = compose_env(&strs(&["PATH=/only"]), &[]);
        assert_eq!(out, ["PATH=/only"]);
        let out = compose_env(&strs(&["PATH=/image"]), &strs(&["PATH=/user"]));
        assert_eq!(out, ["PATH=/user"]);
    }

    #[test]
    fn a_bare_key_is_kept_verbatim_for_the_caller_to_resolve() {
        // `-e TZ` asks to pass TZ through from the caller's own environment.
        // Resolving it needs a real environment, so this layer only records it.
        let a = parse_run_args(&["-e", "TZ", "img"]).unwrap();
        assert_eq!(a.env, ["TZ"]);
        assert_eq!(env_key("TZ"), "TZ");
    }

    #[test]
    fn collects_repeated_env_flags_before_the_image() {
        let a = parse_run_args(&["-e", "A=1", "--env", "B=2", "--rm", "img", "sh"]).unwrap();
        assert_eq!(a.env, ["A=1", "B=2"]);
        assert!(a.rm);
        assert_eq!(a.image, "img");
        assert_eq!(a.argv, ["sh"]);
    }

    #[test]
    fn an_env_flag_after_the_image_belongs_to_the_container() {
        let a = parse_run_args(&["img", "-e", "A=1"]).unwrap();
        assert!(a.env.is_empty());
        assert_eq!(a.argv, ["-e", "A=1"]);
    }

    #[test]
    fn an_env_flag_with_no_value_is_an_error() {
        assert_eq!(parse_run_args(&["-e"]), Err(RunArgsError::MissingValue("--env")));
    }

    #[test]
    fn box_ids_are_stable_and_distinct() {
        assert_eq!(box_id_for("web"), box_id_for("web"));
        assert_ne!(box_id_for("web"), box_id_for("db"));
    }

    #[test]
    fn no_name_maps_to_box_zero() {
        // Box 0 is the host: a container landing there would not be contained.
        assert_ne!(box_id_for(""), 0);
    }

    #[test]
    fn image_is_the_first_non_flag_argument() {
        let a = parse_run_args(&["busybox"]).unwrap();
        assert_eq!(a.image, "busybox");
        assert!(a.argv.is_empty());
        assert_eq!(a, RunArgs { image: String::from("busybox"), ..Default::default() });
    }

    #[test]
    fn collects_flags_before_the_image() {
        let a = parse_run_args(&["--rm", "-d", "-i", "--name", "web", "busybox"]).unwrap();
        assert!(a.rm && a.detached && a.interactive);
        assert_eq!(a.name.as_deref(), Some("web"));
        assert_eq!(a.image, "busybox");
    }

    #[test]
    fn flags_after_the_image_belong_to_the_container() {
        // `box run busybox sh -c ls` — `-c` is the shell's, and a `--rm` after
        // the image is the container program's argument, not a box flag.
        let a = parse_run_args(&["busybox", "sh", "-c", "ls"]).unwrap();
        assert_eq!(a.image, "busybox");
        assert_eq!(a.argv, ["sh", "-c", "ls"]);
        assert!(!a.rm);

        let a = parse_run_args(&["busybox", "--rm"]).unwrap();
        assert!(!a.rm);
        assert_eq!(a.argv, ["--rm"]);
    }

    #[test]
    fn parses_workdir_and_entrypoint_overrides() {
        let a = parse_run_args(&["-w", "/srv", "--entrypoint", "/bin/sh", "img", "-c", "ls"])
            .unwrap();
        assert_eq!(a.workdir.as_deref(), Some("/srv"));
        assert_eq!(a.entrypoint.as_deref(), Some("/bin/sh"));
        assert_eq!(a.image, "img");
        assert_eq!(a.argv, ["-c", "ls"]);
    }

    #[test]
    fn it_is_an_alias_for_interactive() {
        assert!(parse_run_args(&["-it", "img"]).unwrap().interactive);
        assert!(parse_run_args(&["-I", "img"]).unwrap().interactive);
        assert!(parse_run_args(&["--interactive", "img"]).unwrap().interactive);
    }

    #[test]
    fn a_flag_with_no_value_is_an_error_not_a_swallowed_image() {
        assert_eq!(
            parse_run_args(&["--name"]),
            Err(RunArgsError::MissingValue("--name"))
        );
        assert_eq!(
            parse_run_args(&["--entrypoint"]),
            Err(RunArgsError::MissingValue("--entrypoint"))
        );
        assert_eq!(
            parse_run_args(&["-w"]),
            Err(RunArgsError::MissingValue("--workdir"))
        );
    }

    #[test]
    fn flags_with_no_image_are_an_error() {
        assert_eq!(parse_run_args(&[]), Err(RunArgsError::NoImage));
        assert_eq!(parse_run_args(&["--rm", "-d"]), Err(RunArgsError::NoImage));
    }

    #[test]
    fn a_value_that_looks_like_a_flag_is_still_a_value() {
        let a = parse_run_args(&["--name", "--rm", "img"]).unwrap();
        assert_eq!(a.name.as_deref(), Some("--rm"));
        assert!(!a.rm);
        assert_eq!(a.image, "img");
    }

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\npostgres:x:70:70:pg:/var/lib/postgresql:/bin/sh\nredis:x:999:1000::/data:/sbin/nologin\n";
    const GROUP: &str = "root:x:0:root\npostgres:x:70:\nredis:x:1000:\nwheel:x:10:root,postgres\nssl:x:101:postgres,redis\n";

    #[test]
    fn config_user_is_read() {
        let p = image_process_from_config(r#"{"config":{"User":"postgres","Cmd":["x"]}}"#);
        assert_eq!(p.user, "postgres");
        assert_eq!(image_process_from_config(BUSYBOX_CONFIG).user, "");
    }

    #[test]
    fn empty_user_is_root_unchanged() {
        assert_eq!(resolve_user("", PASSWD, GROUP), Ok(None));
    }

    #[test]
    fn a_name_takes_its_passwd_gid_and_its_group_memberships() {
        let r = resolve_user("postgres", PASSWD, GROUP).unwrap().unwrap();
        assert_eq!(r, RunAs { uid: 70, gid: 70, groups: vec![10, 101] });
    }

    #[test]
    fn an_explicit_group_overrides_the_passwd_one() {
        assert_eq!(resolve_user("redis:ssl", PASSWD, GROUP).unwrap().unwrap().gid, 101);
        assert_eq!(resolve_user("redis:5", PASSWD, GROUP).unwrap().unwrap().gid, 5);
    }

    #[test]
    fn a_numeric_uid_needs_no_passwd_line() {
        let r = resolve_user("1234", PASSWD, GROUP).unwrap().unwrap();
        assert_eq!(r, RunAs { uid: 1234, gid: 0, groups: vec![] });
        // ...but uses one when there is one.
        let r = resolve_user("999", PASSWD, GROUP).unwrap().unwrap();
        assert_eq!((r.uid, r.gid, r.groups), (999, 1000, vec![101]));
        assert_eq!(resolve_user("1234:1234", "", "").unwrap().unwrap().gid, 1234);
    }

    #[test]
    fn an_unknown_name_is_an_error_never_root() {
        assert!(resolve_user("nobody-here", PASSWD, GROUP).is_err());
        assert!(resolve_user("redis:nogroup", PASSWD, GROUP).is_err());
    }

    #[test]
    fn dash_u_is_parsed() {
        let a = parse_run_args(&["-u", "1000:1000", "alpine", "id"]).unwrap();
        assert_eq!(a.user.as_deref(), Some("1000:1000"));
        assert_eq!(a.argv, vec![String::from("id")]);
    }
}
