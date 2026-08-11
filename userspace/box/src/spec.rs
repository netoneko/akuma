//! What to run, and what the command line asked for.
//!
//! Two halves of the same decision: the image config says what the image wants
//! to run, `box run`'s flags say what the user wants instead, and
//! [`ImageProcess::argv_with`] resolves the two the way `docker run` does.

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
}

impl Default for ImageProcess {
    /// Nothing to run, starting at the root — an image whose config could not
    /// be read must not end up with `""` as its working directory.
    fn default() -> Self {
        Self {
            entrypoint: Vec::new(),
            cmd: Vec::new(),
            working_dir: String::from("/"),
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
    }
}

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
            "-w" | "--workdir" => out.workdir = Some(value_of(args, &mut i, "--workdir")?),
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
        };
        assert_eq!(p.argv_with(&[]), ["sh"]);
    }

    #[test]
    fn user_args_replace_cmd_and_keep_the_entrypoint() {
        let p = ImageProcess {
            entrypoint: strs(&["/usr/bin/curl"]),
            cmd: strs(&["--help"]),
            working_dir: String::from("/"),
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
        };
        assert_eq!(p.argv_with(&strs(&["echo", "hi"])), ["echo", "hi"]);
    }

    #[test]
    fn entrypoint_override_drops_the_image_cmd() {
        let mut p = ImageProcess {
            entrypoint: strs(&["/usr/bin/curl"]),
            cmd: strs(&["--help"]),
            working_dir: String::from("/"),
        };
        p.override_entrypoint("/bin/sh");
        assert_eq!(p.argv_with(&[]), ["/bin/sh"]);
        assert_eq!(p.argv_with(&strs(&["-c", "ls"])), ["/bin/sh", "-c", "ls"]);
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
}
