//! Dockerfile parsing and word expansion — pure, host-tested.
//!
//! [`parse`] turns a Dockerfile into stages of [`Instr`]s. Arguments stay
//! **raw**: `$VAR` expansion needs the build's current `ENV`/`ARG` state, which
//! only exists while building, so the builder calls [`word`], [`words`],
//! [`env_pairs`] and friends per step with the state of that moment. That is
//! also what makes the cache key honest — it hashes the expanded instruction.
//!
//! Refused loudly, never ignored: `ONBUILD`, `RUN --mount|--network|--security`
//! and heredocs. Each is a clear error until someone implements it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A command as written: `RUN foo bar` or `RUN ["foo","bar"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Shell(String),
    Exec(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CopyArgs {
    pub from: Option<String>,
    pub chown: Option<String>,
    pub chmod: Option<String>,
    /// Raw, space-separated source words followed by the destination.
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instr {
    /// `FROM [--platform=p] image [AS name]`
    From { image: String, alias: Option<String>, platform: Option<String> },
    Run(Cmd),
    Copy(CopyArgs),
    Add(CopyArgs),
    /// Raw text after `ENV` — see [`env_pairs`].
    Env(String),
    /// Raw text after `LABEL` — same grammar as `ENV`.
    Label(String),
    /// `ARG name[=default] …` — raw.
    Arg(String),
    Workdir(String),
    User(String),
    Cmd(Cmd),
    Entrypoint(Cmd),
    Expose(String),
    Shell(Vec<String>),
    StopSignal(String),
    Volume(String),
    Healthcheck(String),
}

impl Instr {
    pub fn keyword(&self) -> &'static str {
        match self {
            Self::From { .. } => "FROM",
            Self::Run(_) => "RUN",
            Self::Copy(_) => "COPY",
            Self::Add(_) => "ADD",
            Self::Env(_) => "ENV",
            Self::Label(_) => "LABEL",
            Self::Arg(_) => "ARG",
            Self::Workdir(_) => "WORKDIR",
            Self::User(_) => "USER",
            Self::Cmd(_) => "CMD",
            Self::Entrypoint(_) => "ENTRYPOINT",
            Self::Expose(_) => "EXPOSE",
            Self::Shell(_) => "SHELL",
            Self::StopSignal(_) => "STOPSIGNAL",
            Self::Volume(_) => "VOLUME",
            Self::Healthcheck(_) => "HEALTHCHECK",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub line: usize,
    /// The instruction exactly as written (continuations joined), for history
    /// entries and error messages.
    pub text: String,
    pub instr: Instr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub index: usize,
    pub image: String,
    pub alias: Option<String>,
    pub platform: Option<String>,
    pub line: usize,
    /// Instructions after the `FROM`.
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dockerfile {
    /// `ARG`s before the first `FROM` — visible to `FROM` lines only.
    pub global_args: Vec<Step>,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError { line, message: message.into() })
}

/// Join continuation lines and drop comments: `(first line number, text)`.
fn logical_lines(src: &str) -> Vec<(usize, String)> {
    // `# escape=\`` directive, only valid in the leading comment block.
    let mut escape = '\\';
    for line in src.lines() {
        let t = line.trim();
        if !t.starts_with('#') {
            break;
        }
        let body = t[1..].trim();
        if let Some((k, v)) = body.split_once('=') {
            if k.trim().eq_ignore_ascii_case("escape") {
                if let Some(c) = v.trim().chars().next() {
                    if c == '`' || c == '\\' {
                        escape = c;
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue; // comments are removed even inside a continuation
        }
        if trimmed.is_empty() {
            if cur.is_empty() {
                continue;
            }
            // A blank line inside a continuation is skipped, as Docker does.
            continue;
        }
        if cur.is_empty() {
            start = i + 1;
        }
        let end = line.trim_end();
        if let Some(stripped) = end.strip_suffix(escape) {
            cur.push_str(stripped);
            continue;
        }
        cur.push_str(line);
        out.push((start, core::mem::take(&mut cur)));
    }
    if !cur.is_empty() {
        out.push((start, cur));
    }
    out
}

/// Leading `--name[=value]` flags, then the rest.
fn take_flags(mut rest: &str) -> (Vec<(String, String)>, &str) {
    let mut flags = Vec::new();
    loop {
        let t = rest.trim_start();
        if !t.starts_with("--") {
            return (flags, t);
        }
        let end = t.find(char::is_whitespace).unwrap_or(t.len());
        let tok = &t[2..end];
        let (k, v) = tok.split_once('=').unwrap_or((tok, ""));
        flags.push((String::from(k), String::from(v)));
        rest = &t[end..];
    }
}

fn has_heredoc(args: &str) -> bool {
    args.split_whitespace().any(|w| {
        w.strip_prefix("<<").is_some_and(|r| {
            let r = r.trim_start_matches('-');
            r.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '"' || c == '\'' || c == '_')
        })
    })
}

/// A JSON array of strings, or `None` if `s` is not one. Escapes: `\" \\ \/
/// \b \f \n \r \t \uXXXX`.
pub fn json_string_array(s: &str) -> Option<Vec<String>> {
    let c: Vec<char> = s.trim().chars().collect();
    if c.first() != Some(&'[') || c.last() != Some(&']') {
        return None;
    }
    let mut i = 1;
    let end = c.len() - 1;
    let mut out = Vec::new();
    let skip_ws = |i: &mut usize| {
        while *i < end && c[*i].is_whitespace() {
            *i += 1;
        }
    };
    skip_ws(&mut i);
    if i == end {
        return Some(out);
    }
    loop {
        skip_ws(&mut i);
        if i >= end || c[i] != '"' {
            return None;
        }
        i += 1;
        let mut cur = String::new();
        loop {
            if i >= end {
                return None;
            }
            match c[i] {
                '"' => {
                    i += 1;
                    break;
                }
                '\\' => {
                    i += 1;
                    let e = *c.get(i)?;
                    match e {
                        '"' | '\\' | '/' => cur.push(e),
                        'b' => cur.push('\u{8}'),
                        'f' => cur.push('\u{c}'),
                        'n' => cur.push('\n'),
                        'r' => cur.push('\r'),
                        't' => cur.push('\t'),
                        'u' => {
                            let hex: String = c.get(i + 1..i + 5)?.iter().collect();
                            cur.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                            i += 4;
                        }
                        _ => return None,
                    }
                    i += 1;
                }
                ch => {
                    cur.push(ch);
                    i += 1;
                }
            }
        }
        out.push(cur);
        skip_ws(&mut i);
        if i == end {
            return Some(out);
        }
        if c[i] != ',' {
            return None;
        }
        i += 1;
    }
}

fn parse_cmd(rest: &str) -> Cmd {
    match json_string_array(rest) {
        Some(v) => Cmd::Exec(v),
        None => Cmd::Shell(String::from(rest.trim())),
    }
}

fn instruction(line: usize, text: &str) -> Result<Instr, ParseError> {
    let t = text.trim_start();
    let (kw, rest) = match t.find(char::is_whitespace) {
        Some(i) => (&t[..i], t[i..].trim_start()),
        None => (t, ""),
    };
    let upper = kw.to_ascii_uppercase();
    let need = |what: &str| -> Result<(), ParseError> {
        if rest.is_empty() {
            err(line, format!("{upper} needs {what}"))
        } else {
            Ok(())
        }
    };
    match upper.as_str() {
        "FROM" => {
            let (flags, rest) = take_flags(rest);
            let mut platform = None;
            for (k, v) in flags {
                match k.as_str() {
                    "platform" => platform = Some(v),
                    other => return err(line, format!("FROM --{other} is not supported")),
                }
            }
            let parts: Vec<&str> = rest.split_whitespace().collect();
            match parts.as_slice() {
                [image] => Ok(Instr::From { image: String::from(*image), alias: None, platform }),
                [image, as_, name] if as_.eq_ignore_ascii_case("as") => Ok(Instr::From {
                    image: String::from(*image),
                    alias: Some(String::from(*name)),
                    platform,
                }),
                _ => err(line, "FROM needs `image [AS name]`"),
            }
        }
        "RUN" => {
            let (flags, body) = take_flags(rest);
            if let Some((k, _)) = flags.first() {
                return err(line, format!("RUN --{k} is not supported"));
            }
            if has_heredoc(body) {
                return err(line, "heredocs are not supported");
            }
            if body.is_empty() {
                return err(line, "RUN needs a command");
            }
            Ok(Instr::Run(parse_cmd(body)))
        }
        "COPY" | "ADD" => {
            let (flags, body) = take_flags(rest);
            let mut a = CopyArgs::default();
            for (k, v) in flags {
                match k.as_str() {
                    "from" if upper == "COPY" => a.from = Some(v),
                    "chown" => a.chown = Some(v),
                    "chmod" => a.chmod = Some(v),
                    "link" => {}
                    other => return err(line, format!("{upper} --{other} is not supported")),
                }
            }
            if has_heredoc(body) {
                return err(line, "heredocs are not supported");
            }
            if body.split_whitespace().count() < 2 {
                return err(line, format!("{upper} needs a source and a destination"));
            }
            a.raw = String::from(body);
            Ok(if upper == "COPY" { Instr::Copy(a) } else { Instr::Add(a) })
        }
        "ENV" => need("key=value").map(|()| Instr::Env(String::from(rest))),
        "LABEL" => need("key=value").map(|()| Instr::Label(String::from(rest))),
        "ARG" => need("a name").map(|()| Instr::Arg(String::from(rest))),
        "WORKDIR" => need("a path").map(|()| Instr::Workdir(String::from(rest))),
        "USER" => need("a user").map(|()| Instr::User(String::from(rest))),
        "CMD" => need("a command").map(|()| Instr::Cmd(parse_cmd(rest))),
        "ENTRYPOINT" => need("a command").map(|()| Instr::Entrypoint(parse_cmd(rest))),
        "EXPOSE" => need("a port").map(|()| Instr::Expose(String::from(rest))),
        "STOPSIGNAL" => need("a signal").map(|()| Instr::StopSignal(String::from(rest))),
        "VOLUME" => need("a path").map(|()| Instr::Volume(String::from(rest))),
        "HEALTHCHECK" => need("NONE or CMD").map(|()| Instr::Healthcheck(String::from(rest))),
        "SHELL" => match json_string_array(rest) {
            Some(v) if !v.is_empty() => Ok(Instr::Shell(v)),
            _ => err(line, "SHELL needs a JSON array"),
        },
        "ONBUILD" => err(line, "ONBUILD is not supported"),
        "MAINTAINER" => Ok(Instr::Label(format!("maintainer={}", quote_for_label(rest)))),
        _ => err(line, format!("unknown instruction {kw}")),
    }
}

fn quote_for_label(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        if c == '"' || c == '\\' || c == '$' {
            o.push('\\');
        }
        o.push(c);
    }
    o.push('"');
    o
}

/// Parse a whole Dockerfile.
///
/// # Errors
/// The first malformed or unsupported line, with its number.
pub fn parse(src: &str) -> Result<Dockerfile, ParseError> {
    let mut df = Dockerfile::default();
    for (line, text) in logical_lines(src) {
        let instr = instruction(line, &text)?;
        let step_text = String::from(text.trim());
        match instr {
            Instr::From { image, alias, platform } => {
                if let Some(a) = &alias {
                    if df.stages.iter().any(|s| s.alias.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(a))) {
                        return err(line, format!("duplicate stage name {a}"));
                    }
                }
                let index = df.stages.len();
                df.stages.push(Stage { index, image, alias, platform, line, steps: Vec::new() });
            }
            other => {
                let step = Step { line, text: step_text, instr: other };
                match df.stages.last_mut() {
                    Some(stage) => stage.steps.push(step),
                    None => match step.instr {
                        Instr::Arg(_) => df.global_args.push(step),
                        _ => return err(line, "the first instruction must be FROM (or ARG)"),
                    },
                }
            }
        }
    }
    if df.stages.is_empty() {
        return err(0, "no FROM instruction");
    }
    Ok(df)
}

impl Dockerfile {
    /// Index of the stage a name or number refers to (`COPY --from=0`,
    /// `--target build`). Aliases match case-insensitively.
    pub fn stage_index(&self, name: &str) -> Option<usize> {
        if let Some(i) = self.stages.iter().position(|s| s.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(name))) {
            return Some(i);
        }
        name.parse::<usize>().ok().filter(|&i| i < self.stages.len())
    }

    /// Stages that must be built to produce `target`: its `FROM` and every
    /// `COPY --from` reach, transitively, in dependency (= file) order.
    pub fn closure(&self, target: usize) -> Vec<usize> {
        let mut needed = alloc::vec![false; self.stages.len()];
        let mut stack = alloc::vec![target];
        while let Some(i) = stack.pop() {
            if core::mem::replace(&mut needed[i], true) {
                continue;
            }
            let st = &self.stages[i];
            if let Some(j) = self.stage_index(&st.image).filter(|&j| j < i) {
                stack.push(j);
            }
            for step in &st.steps {
                if let Instr::Copy(CopyArgs { from: Some(f), .. }) = &step.instr {
                    if let Some(j) = self.stage_index(f).filter(|&j| j < i) {
                        stack.push(j);
                    }
                }
            }
        }
        (0..self.stages.len()).filter(|&i| needed[i]).collect()
    }
}

// ───────────────────────── word expansion ─────────────────────────

/// Variable lookup: `None` = unset.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Expand a `$…` at `c[*i] == '$'`, appending to `out`. Leaves `*i` after it.
fn expand_dollar(c: &[char], i: &mut usize, lookup: Lookup, out: &mut String) {
    *i += 1; // '$'
    match c.get(*i) {
        Some('{') => {
            // find matching '}'
            let mut depth = 1;
            let mut j = *i + 1;
            while j < c.len() {
                match c[j] {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    '\\' => j += 1,
                    _ => {}
                }
                j += 1;
            }
            if j >= c.len() {
                out.push_str("${");
                *i += 1;
                return;
            }
            let inner = &c[*i + 1..j];
            *i = j + 1;
            let name_len = inner.iter().take_while(|&&ch| is_name_char(ch)).count();
            let name: String = inner[..name_len].iter().collect();
            let value = lookup(&name);
            let op: String = inner[name_len..].iter().take(2).collect();
            let skip = |n: usize| inner[(name_len + n).min(inner.len())..].iter().collect::<String>();
            let sub = |text: String| expand_all(&text, lookup);
            match op.as_str() {
                "" => out.push_str(value.as_deref().unwrap_or("")),
                o if o.starts_with(":-") => match value.filter(|v| !v.is_empty()) {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&sub(skip(2))),
                },
                o if o.starts_with(":+") => {
                    if value.is_some_and(|v| !v.is_empty()) {
                        out.push_str(&sub(skip(2)));
                    }
                }
                o if o.starts_with('-') => match value {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&sub(skip(1))),
                },
                o if o.starts_with('+') => {
                    if value.is_some() {
                        out.push_str(&sub(skip(1)));
                    }
                }
                _ => out.push_str(value.as_deref().unwrap_or("")),
            }
        }
        Some(&ch) if is_name_char(ch) && !ch.is_ascii_digit() => {
            let mut name = String::new();
            while let Some(&ch) = c.get(*i) {
                if !is_name_char(ch) {
                    break;
                }
                name.push(ch);
                *i += 1;
            }
            out.push_str(&lookup(&name).unwrap_or_default());
        }
        _ => out.push('$'),
    }
}

/// `$VAR`/`${…}` expansion with no quote handling — used for the default text
/// inside `${V:-default}`.
fn expand_all(s: &str, lookup: Lookup) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '$' {
            expand_dollar(&c, &mut i, lookup, &mut out);
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

fn tokenize(s: &str, lookup: Lookup, split: bool) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        match ch {
            _ if split && ch.is_whitespace() => {
                if has {
                    words.push(core::mem::take(&mut cur));
                    has = false;
                }
                i += 1;
            }
            '\'' => {
                i += 1;
                has = true;
                while i < c.len() && c[i] != '\'' {
                    cur.push(c[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                i += 1;
                has = true;
                while i < c.len() && c[i] != '"' {
                    match c[i] {
                        '\\' if matches!(c.get(i + 1), Some('$' | '"' | '\\')) => {
                            cur.push(c[i + 1]);
                            i += 2;
                        }
                        '$' => expand_dollar(&c, &mut i, lookup, &mut cur),
                        other => {
                            cur.push(other);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            '\\' if i + 1 < c.len() => {
                cur.push(c[i + 1]);
                has = true;
                i += 2;
            }
            '$' => {
                let before = cur.len();
                expand_dollar(&c, &mut i, lookup, &mut cur);
                if cur.len() != before {
                    has = true;
                }
            }
            other => {
                cur.push(other);
                has = true;
                i += 1;
            }
        }
    }
    if has || !split {
        words.push(cur);
    }
    words
}

/// Expand and unquote one value that may contain spaces (`WORKDIR`, `USER`,
/// the value of the legacy `ENV key value` form).
pub fn word(s: &str, lookup: Lookup) -> String {
    tokenize(s.trim(), lookup, false).pop().unwrap_or_default()
}

/// Expand, unquote and split on unquoted whitespace.
pub fn words(s: &str, lookup: Lookup) -> Vec<String> {
    tokenize(s, lookup, true)
}

/// The pairs `ENV`/`LABEL` define. Handles `K=V K2="v 2"` and the legacy
/// `K some value` form.
pub fn env_pairs(raw: &str, lookup: Lookup) -> Result<Vec<(String, String)>, String> {
    let raw = raw.trim();
    // First *raw* token decides the form: the legacy form has no `=` in it.
    let first_end = raw.find(char::is_whitespace).unwrap_or(raw.len());
    if !raw[..first_end].contains('=') {
        let rest = raw[first_end..].trim();
        if rest.is_empty() {
            return Err(format!("{} needs a value", &raw[..first_end]));
        }
        return Ok(alloc::vec![(String::from(&raw[..first_end]), word(rest, lookup))]);
    }
    let mut out = Vec::new();
    for w in words(raw, lookup) {
        match w.split_once('=') {
            Some((k, v)) if !k.is_empty() => out.push((String::from(k), String::from(v))),
            _ => return Err(format!("expected key=value, got `{w}`")),
        }
    }
    Ok(out)
}

/// `ARG a b=1 c="x y"` → `(name, default)`.
pub fn arg_decls(raw: &str, lookup: Lookup) -> Vec<(String, Option<String>)> {
    words(raw, lookup)
        .into_iter()
        .map(|w| match w.split_once('=') {
            Some((k, v)) => (String::from(k), Some(String::from(v))),
            None => (w, None),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| String::from(*v))
    }

    #[test]
    fn expansion_forms() {
        let e = env(&[("A", "1"), ("EMPTY", "")]);
        assert_eq!(word("$A-${A}x", &e), "1-1x");
        assert_eq!(word("${NOPE:-dflt}", &e), "dflt");
        assert_eq!(word("${EMPTY:-dflt}", &e), "dflt");
        assert_eq!(word("${EMPTY-dflt}", &e), "");
        assert_eq!(word("${A:+yes}", &e), "yes");
        assert_eq!(word("${EMPTY:+yes}", &e), "");
        assert_eq!(word("${EMPTY+yes}", &e), "yes");
        assert_eq!(word("${NOPE:-${A}}", &e), "1");
        assert_eq!(word("price: $", &e), "price: $");
    }

    #[test]
    fn quoting_rules() {
        let e = env(&[("A", "x y")]);
        assert_eq!(word("'$A'", &e), "$A");
        assert_eq!(word("\"$A\"", &e), "x y");
        assert_eq!(word("\\$A", &e), "$A");
        assert_eq!(words("a \"b c\" 'd e' $A", &e), vec!["a", "b c", "d e", "x y"]);
        // An unquoted empty expansion produces no word at all.
        assert_eq!(words("a $NOPE b", &e), vec!["a", "b"]);
        assert_eq!(words("a \"\" b", &e), vec!["a", "", "b"]);
    }

    #[test]
    fn env_forms() {
        let e = env(&[("V", "7")]);
        assert_eq!(
            env_pairs("A=1 B=\"two words\" C=$V", &e).unwrap(),
            [("A".into(), "1".into()), ("B".into(), "two words".into()), ("C".into(), "7".into())]
        );
        assert_eq!(env_pairs("KEY some value $V", &e).unwrap(), [("KEY".into(), "some value 7".into())]);
        assert!(env_pairs("KEY", &e).is_err());
    }

    #[test]
    fn arg_declarations() {
        let e = env(&[]);
        assert_eq!(
            arg_decls("a b=1 c=\"x y\"", &e),
            [("a".into(), None), ("b".into(), Some("1".into())), ("c".into(), Some("x y".into()))]
        );
    }

    #[test]
    fn json_arrays() {
        assert_eq!(json_string_array("[\"a\", \"b c\"]").unwrap(), ["a", "b c"]);
        assert_eq!(json_string_array("[]").unwrap(), Vec::<String>::new());
        assert_eq!(json_string_array("[\"a\\n\\\"q\\\" \\u0041\"]").unwrap(), ["a\n\"q\" A"]);
        assert!(json_string_array("echo hi").is_none());
        assert!(json_string_array("[a]").is_none());
        assert!(json_string_array("[\"a\",]").is_none());
    }

    const SAMPLE: &str = "\
# syntax=docker/dockerfile:1
ARG BASE=alpine:3.19
FROM ${BASE} AS build
# a comment
RUN apk add --no-cache \\
    # inner comment
    gcc musl-dev
COPY --chown=1000:1000 src /src
WORKDIR /src
ENV A=1 B=2
CMD [\"./app\", \"--flag\"]

FROM scratch
COPY --from=build /src/app /app
ENTRYPOINT /app
";

    #[test]
    fn parses_stages_and_steps() {
        let df = parse(SAMPLE).unwrap();
        assert_eq!(df.global_args.len(), 1);
        assert_eq!(df.stages.len(), 2);
        let s0 = &df.stages[0];
        assert_eq!((s0.image.as_str(), s0.alias.as_deref()), ("${BASE}", Some("build")));
        assert_eq!(s0.steps.len(), 5);
        assert_eq!(s0.steps[0].line, 5);
        match &s0.steps[0].instr {
            Instr::Run(Cmd::Shell(c)) => assert!(c.contains("gcc musl-dev") && !c.contains("inner")),
            other => panic!("{other:?}"),
        }
        match &s0.steps[1].instr {
            Instr::Copy(a) => assert_eq!((a.chown.as_deref(), a.raw.as_str()), (Some("1000:1000"), "src /src")),
            other => panic!("{other:?}"),
        }
        assert_eq!(s0.steps[4].instr, Instr::Cmd(Cmd::Exec(vec!["./app".into(), "--flag".into()])));
        assert_eq!(df.stages[1].steps[1].instr, Instr::Entrypoint(Cmd::Shell("/app".into())));
    }


    /// Shapes lifted from the official images' Dockerfiles: legacy `ENV k v`,
    /// `RUN set -eux; \` chains with comments and quoted `$`, `LABEL` with quotes,
    /// `COPY --from`, `HEALTHCHECK`, `STOPSIGNAL`, JSON `ENTRYPOINT`.
    const CORPUS_NGINX: &str = r#"
FROM debian:bookworm-slim
LABEL maintainer="NGINX Docker Maintainers <docker-maint@nginx.com>"

ENV NGINX_VERSION   1.25.3
ENV NJS_VERSION     0.8.2
ENV PKG_RELEASE     1~bookworm

RUN set -x \
# create nginx user/group first, to be consistent throughout docker variants
    && groupadd --system --gid 101 nginx \
    && useradd --system --gid nginx --no-create-home --home /nonexistent --comment "nginx user" --shell /bin/false --uid 101 nginx \
    && apt-get update \
    && for p in nginx-${NGINX_VERSION}-${PKG_RELEASE}; do echo "$p"; done \
    && rm -rf /var/lib/apt/lists/*

COPY docker-entrypoint.sh /
COPY 10-listen-on-ipv6-by-default.sh /docker-entrypoint.d
RUN ln -sf /dev/stdout /var/log/nginx/access.log

ENTRYPOINT ["/docker-entrypoint.sh"]

EXPOSE 80

STOPSIGNAL SIGQUIT

CMD ["nginx", "-g", "daemon off;"]
"#;

    const CORPUS_GO: &str = r#"
# syntax=docker/dockerfile:1
ARG GO_VERSION=1.22
FROM golang:${GO_VERSION}-alpine AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/app ./cmd/app

FROM gcr.io/distroless/static-debian12:nonroot AS runtime
COPY --from=build --chown=nonroot:nonroot /out/app /app
USER nonroot:nonroot
HEALTHCHECK --interval=30s --timeout=3s CMD ["/app", "-health"]
EXPOSE 8080/tcp 9090
ENTRYPOINT ["/app"]
"#;

    const CORPUS_NODE: &str = "FROM node:20-alpine\r\nENV NODE_ENV=production \\\r\n    PORT=3000\r\nWORKDIR /usr/src/app\r\nCOPY package*.json ./\r\nRUN npm ci --omit=dev && npm cache clean --force\r\nCOPY . .\r\nVOLUME [\"/data\"]\r\nUSER node\r\nCMD node server.js\r\n";

    #[test]
    fn real_world_dockerfiles_parse() {
        let df = parse(CORPUS_NGINX).unwrap();
        assert_eq!(df.stages.len(), 1);
        let steps = &df.stages[0].steps;
        assert_eq!(steps.len(), 12);
        let look = |_: &str| None;
        // The legacy `ENV k   v` form keeps the whole value.
        let Instr::Env(raw) = &steps[1].instr else { panic!() };
        assert_eq!(env_pairs(raw, &look).unwrap(), [("NGINX_VERSION".into(), "1.25.3".into())]);
        // The RUN joined every continuation and dropped the comment line.
        let Instr::Run(Cmd::Shell(c)) = &steps[4].instr else { panic!() };
        assert!(c.contains("useradd") && c.contains("rm -rf /var/lib/apt") && !c.contains("create nginx user"));
        assert!(c.contains("${NGINX_VERSION}"), "RUN is not expanded by the parser");
        let Instr::Label(l) = &steps[0].instr else { panic!() };
        assert_eq!(
            env_pairs(l, &look).unwrap(),
            [("maintainer".into(), "NGINX Docker Maintainers <docker-maint@nginx.com>".into())]
        );

        let df = parse(CORPUS_GO).unwrap();
        assert_eq!(df.global_args.len(), 1);
        assert_eq!(df.stages.len(), 2);
        assert_eq!(df.stages[1].alias.as_deref(), Some("runtime"));
        let Instr::Copy(a) = &df.stages[1].steps[0].instr else { panic!() };
        assert_eq!((a.from.as_deref(), a.chown.as_deref()), (Some("build"), Some("nonroot:nonroot")));
        assert_eq!(df.closure(1), [0, 1]);
        let Instr::Healthcheck(h) = &df.stages[1].steps[2].instr else { panic!() };
        assert!(crate::buildopts::parse_healthcheck(h).is_ok());
        let Instr::Expose(e) = &df.stages[1].steps[3].instr else { panic!() };
        assert_eq!(words(e, &look), ["8080/tcp", "9090"]);

        let df = parse(CORPUS_NODE).unwrap();
        let steps = &df.stages[0].steps;
        let Instr::Env(raw) = &steps[0].instr else { panic!() };
        assert_eq!(env_pairs(raw, &look).unwrap().len(), 2, "CRLF continuations join");
        assert_eq!(steps.last().unwrap().instr, Instr::Cmd(Cmd::Shell("node server.js".into())));
    }

    #[test]
    fn stage_lookup_and_closure() {
        let df = parse(SAMPLE).unwrap();
        assert_eq!(df.stage_index("BUILD"), Some(0));
        assert_eq!(df.stage_index("1"), Some(1));
        assert_eq!(df.stage_index("nope"), None);
        assert_eq!(df.closure(1), [0, 1]);
        assert_eq!(df.closure(0), [0]);
    }

    #[test]
    fn backtick_escape_directive() {
        let df = parse("# escape=`\nFROM a\nRUN echo `\n  hi\n").unwrap();
        assert_eq!(df.stages[0].steps[0].instr, Instr::Run(Cmd::Shell("echo   hi".into())));
    }

    #[test]
    fn refuses_what_it_does_not_do() {
        for bad in [
            "FROM a\nONBUILD RUN x",
            "FROM a\nRUN --mount=type=cache,target=/c x",
            "FROM a\nRUN --network=none x",
            "FROM a\nRUN <<EOF\nx\nEOF",
            "FROM a\nCOPY <<EOF /f\nx\nEOF",
            "FROM a\nBOGUS x",
            "RUN x",
            "",
            "FROM a AS x\nFROM b AS X",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let e = parse("FROM a\nRUN --mount=x y").unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn keywords_are_case_insensitive_and_maintainer_becomes_a_label() {
        let df = parse("from a\nrun echo\nMAINTAINER Me \"x\"").unwrap();
        assert_eq!(df.stages[0].steps[0].instr.keyword(), "RUN");
        let Instr::Label(raw) = &df.stages[0].steps[1].instr else { panic!() };
        assert_eq!(env_pairs(raw, &env(&[])).unwrap(), [("maintainer".into(), "Me \"x\"".into())]);
    }
}
