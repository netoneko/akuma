//! `.dockerignore` matching — Docker's rules, pure and host-tested.
//!
//! Each non-blank, non-`#` line is a pattern relative to the context root;
//! a leading `!` re-includes. **The last matching pattern wins**, and a pattern
//! that matches a directory covers everything under it (so `!` has to name the
//! file itself to rescue one from an ignored directory — only if no earlier
//! rule excluded the parent *and* a later one re-includes the path).
//! Per path segment: `*` any run, `?` one char, `[a-z]`/`[^a-z]`, `\x` literal;
//! a whole segment `**` matches any number of directories (including none).

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    negated: bool,
    segments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ignore {
    patterns: Vec<Pattern>,
}

/// Collapse `.`/empty segments and a leading `/`; `..` pops.
fn clean(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(String::from(s)),
        }
    }
    out
}

impl Ignore {
    pub fn parse(text: &str) -> Self {
        let mut patterns = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            let segments = clean(body);
            if segments.is_empty() {
                continue;
            }
            patterns.push(Pattern { negated, segments });
        }
        Self { patterns }
    }

    /// Whether `path` (context-relative, `/`-separated) is excluded.
    pub fn is_ignored(&self, path: &str) -> bool {
        let segs = clean(path);
        let mut ignored = false;
        for p in &self.patterns {
            // A pattern matching the path or any ancestor directory of it.
            let hit = (1..=segs.len()).any(|n| match_segments(&p.segments, &segs[..n]));
            if hit {
                ignored = !p.negated;
            }
        }
        ignored
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

fn match_segments(pat: &[String], path: &[String]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((p, rest)) if p == "**" => {
            // Zero or more whole segments.
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((p, rest)) => match path.split_first() {
            Some((s, path_rest)) => match_glob(p, s) && match_segments(rest, path_rest),
            None => false,
        },
    }
}

/// Match one path segment against one pattern segment.
pub fn match_glob(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    glob(&p, &n)
}

fn glob(p: &[char], n: &[char]) -> bool {
    match p.first() {
        None => n.is_empty(),
        Some('*') => (0..=n.len()).any(|k| glob(&p[1..], &n[k..])),
        Some('?') => !n.is_empty() && glob(&p[1..], &n[1..]),
        Some('[') => {
            let Some(&c) = n.first() else { return false };
            let mut i = 1;
            let negate = matches!(p.get(i), Some('^' | '!'));
            if negate {
                i += 1;
            }
            let mut matched = false;
            let mut first = true;
            while i < p.len() && (p[i] != ']' || first) {
                first = false;
                let lo = if p[i] == '\\' && i + 1 < p.len() {
                    i += 1;
                    p[i]
                } else {
                    p[i]
                };
                if p.get(i + 1) == Some(&'-') && i + 2 < p.len() && p[i + 2] != ']' {
                    if lo <= c && c <= p[i + 2] {
                        matched = true;
                    }
                    i += 3;
                } else {
                    if lo == c {
                        matched = true;
                    }
                    i += 1;
                }
            }
            if i >= p.len() {
                return false; // unterminated class matches nothing
            }
            matched != negate && glob(&p[i + 1..], &n[1..])
        }
        Some('\\') if p.len() > 1 => n.first() == Some(&p[1]) && glob(&p[2..], &n[1..]),
        Some(&c) => n.first() == Some(&c) && glob(&p[1..], &n[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(t: &str) -> Ignore {
        Ignore::parse(t)
    }

    #[test]
    fn globs() {
        assert!(match_glob("*.txt", "a.txt"));
        assert!(!match_glob("*.txt", "a.md"));
        assert!(match_glob("a?c", "abc"));
        assert!(match_glob("[a-c]x", "bx"));
        assert!(!match_glob("[^a-c]x", "bx"));
        assert!(match_glob("a\\*b", "a*b"));
        assert!(!match_glob("a\\*b", "axb"));
        assert!(!match_glob("[abc", "a"));
        assert!(match_glob("*", ""));
    }

    #[test]
    fn plain_patterns_and_directories() {
        let i = ig("target\n*.log\n# comment\n\n");
        assert!(i.is_ignored("target"));
        assert!(i.is_ignored("target/debug/app"), "a directory covers its contents");
        assert!(i.is_ignored("x.log"));
        assert!(!i.is_ignored("src/x.log.rs"));
        // Patterns are anchored at the root, not matched at any depth.
        assert!(!i.is_ignored("src/target"));
        assert!(!i.is_ignored("src/x.log"), "`*.log` is root-level only");
    }

    #[test]
    fn double_star() {
        let i = ig("**/*.log\n**/node_modules");
        assert!(i.is_ignored("a.log"));
        assert!(i.is_ignored("a/b/c.log"));
        assert!(i.is_ignored("web/node_modules/x/y.js"));
        assert!(!i.is_ignored("a/b/c.txt"));
    }

    #[test]
    fn last_match_wins_and_negation() {
        let i = ig("*.md\n!README.md");
        assert!(i.is_ignored("CHANGES.md"));
        assert!(!i.is_ignored("README.md"));
        let i = ig("!README.md\n*.md");
        assert!(i.is_ignored("README.md"), "order matters");
    }

    #[test]
    fn leading_slash_and_dot_are_normalised() {
        let i = ig("/build\n./tmp");
        assert!(i.is_ignored("build/x"));
        assert!(i.is_ignored("tmp"));
    }

    #[test]
    fn empty_ignores_nothing() {
        assert!(!ig("").is_ignored("anything"));
        assert!(ig("").is_empty());
    }
}
