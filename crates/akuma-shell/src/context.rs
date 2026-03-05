//! Per-session shell context.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Per-session shell context holding state like current working directory.
pub struct ShellContext {
    cwd: String,
    pub async_exec: bool,
    pub interactive_exec: bool,
    env: BTreeMap<String, String>,
}

impl ShellContext {
    /// Create a shell context with explicit defaults.
    ///
    /// `default_env` supplies the initial environment (e.g. `["PATH=/bin", "HOME=/"]`).
    /// `async_exec` controls whether external commands are spawned asynchronously.
    #[must_use]
    pub fn with_defaults(default_env: &[&str], async_exec: bool) -> Self {
        let mut env = BTreeMap::new();
        for entry in default_env {
            if let Some((k, v)) = entry.split_once('=') {
                env.insert(String::from(k), String::from(v));
            }
        }
        env.insert(String::from("PWD"), String::from("/"));

        Self {
            cwd: String::from("/"),
            async_exec,
            interactive_exec: true,
            env,
        }
    }

    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn set_cwd(&mut self, path: &str) {
        self.cwd = String::from(path);
        self.env.insert(String::from("PWD"), String::from(path));
    }

    #[must_use]
    pub const fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    #[must_use]
    pub fn get_env(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    pub fn set_env(&mut self, key: &str, value: &str) {
        self.env.insert(String::from(key), String::from(value));
    }

    pub fn remove_env(&mut self, key: &str) {
        self.env.remove(key);
    }

    #[must_use]
    pub fn env_as_vec(&self) -> Vec<String> {
        self.env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect()
    }

    /// Resolve a path relative to the current working directory.
    #[must_use]
    pub fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            normalize_path(path)
        } else {
            let full_path = if self.cwd == "/" {
                format!("/{path}")
            } else {
                format!("{}/{path}", self.cwd)
            };
            normalize_path(&full_path)
        }
    }
}

/// Normalize a path (resolve `.` and `..`).
#[must_use]
pub fn normalize_path(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            c => {
                components.push(c);
            }
        }
    }

    if components.is_empty() {
        String::from("/")
    } else {
        let mut result = String::new();
        for c in components {
            result.push('/');
            result.push_str(c);
        }
        result
    }
}
