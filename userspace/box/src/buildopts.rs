//! `box build`'s command line and the small value parsers its instructions need.
//! Pure, host-tested.

use alloc::string::String;
use alloc::vec::Vec;

use crate::imgconfig::Healthcheck;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildArgs {
    pub tags: Vec<String>,
    pub file: Option<String>,
    pub build_args: Vec<(String, Option<String>)>,
    pub target: Option<String>,
    pub no_cache: bool,
    pub context: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BuildArgsError {
    MissingValue(&'static str),
    Unknown(String),
    NoContext,
    ExtraArgument(String),
}

/// `box build [-t name]… [-f Dockerfile] [--build-arg K[=V]]… [--target S]
/// [--no-cache] <context>`. Flags may follow the context, as with Docker.
pub fn parse_build_args(args: &[&str]) -> Result<BuildArgs, BuildArgsError> {
    let mut out = BuildArgs::default();
    let mut context: Option<&str> = None;
    let mut i = 0;
    let value = |i: &mut usize, flag: &'static str| -> Result<String, BuildArgsError> {
        *i += 1;
        args.get(*i).map(|s| String::from(*s)).ok_or(BuildArgsError::MissingValue(flag))
    };
    while i < args.len() {
        let a = args[i];
        // `--flag=value` spelling.
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(String::from(v))),
            _ => (a, None),
        };
        let take = |i: &mut usize, name: &'static str| -> Result<String, BuildArgsError> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => value(i, name),
            }
        };
        match flag {
            "-t" | "--tag" => out.tags.push(take(&mut i, "-t")?),
            "-f" | "--file" => out.file = Some(take(&mut i, "-f")?),
            "--target" => out.target = Some(take(&mut i, "--target")?),
            "--build-arg" => {
                let kv = take(&mut i, "--build-arg")?;
                out.build_args.push(match kv.split_once('=') {
                    Some((k, v)) => (String::from(k), Some(String::from(v))),
                    None => (kv, None),
                });
            }
            "--no-cache" => out.no_cache = true,
            _ if flag.starts_with('-') && flag.len() > 1 => return Err(BuildArgsError::Unknown(String::from(flag))),
            _ => match context {
                None => context = Some(a),
                Some(_) => return Err(BuildArgsError::ExtraArgument(String::from(a))),
            },
        }
        i += 1;
    }
    out.context = String::from(context.ok_or(BuildArgsError::NoContext)?);
    Ok(out)
}

/// `30s`, `1m30s`, `500ms`, `2h` → nanoseconds.
pub fn parse_duration_ns(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut total: i64 = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let n: i64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (unit, len) = if rest.starts_with("ms") {
            (1_000_000, 2)
        } else if rest.starts_with("us") {
            (1_000, 2)
        } else if rest.starts_with("ns") {
            (1, 2)
        } else if rest.starts_with('s') {
            (1_000_000_000, 1)
        } else if rest.starts_with('m') {
            (60_000_000_000, 1)
        } else if rest.starts_with('h') {
            (3_600_000_000_000, 1)
        } else {
            return None;
        };
        rest = &rest[len..];
        total = total.checked_add(n.checked_mul(unit)?)?;
    }
    Some(total)
}

/// `HEALTHCHECK [--interval=D] [--timeout=D] [--start-period=D] [--retries=N]
/// CMD …` or `NONE`.
pub fn parse_healthcheck(raw: &str) -> Result<Healthcheck, String> {
    let mut hc = Healthcheck::default();
    let mut rest = raw.trim();
    while let Some(flag) = rest.strip_prefix("--") {
        let end = flag.find(char::is_whitespace).unwrap_or(flag.len());
        let (k, v) = flag[..end].split_once('=').ok_or_else(|| String::from("HEALTHCHECK flag needs a value"))?;
        match k {
            "interval" => hc.interval = parse_duration_ns(v).ok_or("bad --interval")?,
            "timeout" => hc.timeout = parse_duration_ns(v).ok_or("bad --timeout")?,
            "start-period" => hc.start_period = parse_duration_ns(v).ok_or("bad --start-period")?,
            "retries" => hc.retries = v.parse().map_err(|_| String::from("bad --retries"))?,
            other => return Err(alloc::format!("HEALTHCHECK --{other} is not supported")),
        }
        rest = flag[end..].trim_start();
    }
    if rest.eq_ignore_ascii_case("none") {
        hc.test = alloc::vec![String::from("NONE")];
        return Ok(hc);
    }
    let body = match rest.get(..3) {
        Some(c) if c.eq_ignore_ascii_case("cmd") => rest[3..].trim_start(),
        _ => return Err(String::from("HEALTHCHECK needs NONE or CMD")),
    };
    if body.is_empty() {
        return Err(String::from("HEALTHCHECK CMD needs a command"));
    }
    hc.test = match crate::dockerfile::json_string_array(body) {
        Some(v) => core::iter::once(String::from("CMD")).chain(v).collect(),
        None => alloc::vec![String::from("CMD-SHELL"), String::from(body)],
    };
    Ok(hc)
}

/// `80` → `80/tcp`; anything with a protocol is kept.
pub fn normalize_port(p: &str) -> String {
    if p.contains('/') {
        String::from(p)
    } else {
        alloc::format!("{p}/tcp")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn full_command_line() {
        let a = parse_build_args(&[
            "-t", "app:1", "--tag=app:latest", "-f", "Df", "--build-arg", "A=1", "--build-arg", "B",
            "--target", "final", "--no-cache", ".",
        ])
        .unwrap();
        assert_eq!(a.tags, ["app:1", "app:latest"]);
        assert_eq!(a.file.as_deref(), Some("Df"));
        assert_eq!(a.build_args, [("A".into(), Some("1".into())), ("B".into(), None)]);
        assert_eq!((a.target.as_deref(), a.no_cache, a.context.as_str()), (Some("final"), true, "."));
    }

    #[test]
    fn flags_after_the_context_and_errors() {
        assert_eq!(parse_build_args(&["ctx", "-t", "x"]).unwrap().tags, ["x"]);
        assert_eq!(parse_build_args(&["-t", "x"]), Err(BuildArgsError::NoContext));
        assert_eq!(parse_build_args(&["a", "b"]), Err(BuildArgsError::ExtraArgument("b".into())));
        assert_eq!(parse_build_args(&[".", "-t"]), Err(BuildArgsError::MissingValue("-t")));
        assert_eq!(parse_build_args(&[".", "--squash"]), Err(BuildArgsError::Unknown("--squash".into())));
        // A build arg value may itself contain '='.
        let a = parse_build_args(&["--build-arg", "URL=http://x/?a=b", "."]).unwrap();
        assert_eq!(a.build_args[0].1.as_deref(), Some("http://x/?a=b"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ns("30s"), Some(30_000_000_000));
        assert_eq!(parse_duration_ns("1m30s"), Some(90_000_000_000));
        assert_eq!(parse_duration_ns("500ms"), Some(500_000_000));
        assert_eq!(parse_duration_ns("2h"), Some(7_200_000_000_000));
        assert_eq!(parse_duration_ns("30"), None);
        assert_eq!(parse_duration_ns("s"), None);
        assert_eq!(parse_duration_ns(""), None);
    }

    #[test]
    fn healthchecks() {
        let h = parse_healthcheck("--interval=5s --retries=2 CMD curl -f http://localhost/").unwrap();
        assert_eq!(h.test, ["CMD-SHELL", "curl -f http://localhost/"]);
        assert_eq!((h.interval, h.retries), (5_000_000_000, 2));
        let h = parse_healthcheck("CMD [\"true\", \"x\"]").unwrap();
        assert_eq!(h.test, ["CMD", "true", "x"]);
        assert_eq!(parse_healthcheck("NONE").unwrap().test, ["NONE"]);
        assert!(parse_healthcheck("CMD").is_err());
        assert!(parse_healthcheck("--interval=zz CMD x").is_err());
        assert!(parse_healthcheck("echo hi").is_err());
    }

    #[test]
    fn ports() {
        assert_eq!(normalize_port("80"), "80/tcp");
        assert_eq!(normalize_port("53/udp"), "53/udp");
        let _ = vec![0];
    }
}
