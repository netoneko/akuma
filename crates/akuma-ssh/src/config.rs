use alloc::string::String;

#[derive(Debug, Clone, Default)]
pub struct SshdConfig {
    pub disable_key_verification: bool,
    pub shell: Option<String>,
}

impl SshdConfig {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a multi-line config string into a config struct.
    #[must_use]
    pub fn parse(content: &str) -> Self {
        let mut config = Self::default();
        for line in content.lines() {
            config.parse_line(line);
        }
        config
    }

    /// Parse a single `key = value` config line.
    pub fn parse_line(&mut self, line: &str) {
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            return;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                "disable_key_verification" => {
                    self.disable_key_verification = parse_bool(value);
                }
                "shell" => {
                    self.shell = Some(String::from(value));
                }
                _ => {
                    log::warn!("[SSH Config] Unknown config key: {key}");
                }
            }
        }
    }
}

fn parse_bool(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    matches!(s.as_str(), "true" | "yes" | "1" | "on")
}
