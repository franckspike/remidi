use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub filters: Vec<FilterConfig>,
    #[serde(default)]
    pub tests: Vec<TestStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    #[serde(default = "default_filter_id")]
    pub id: String,
    #[serde(alias = "created_midi_out_port_name", default = "default_virtual_port")]
    pub virtual_port: String,
    #[serde(alias = "opened_midi_out_port_name")]
    pub target_port: String,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

fn default_filter_id() -> String {
    "default".to_string()
}

fn default_virtual_port() -> String {
    "ReMIDI".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    #[serde(rename = "match")]
    pub match_pattern: String,
    #[serde(alias = "replace_by")]
    pub replace: String,
    #[serde(alias = "min_delay", default)]
    pub min_delay_ms: u64,
    #[serde(default)]
    pub dec_second_byte: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestStep {
    /// For TOML convenience: `send = "C0 01"`
    #[serde(default)]
    pub send: Option<String>,
    /// For TOML convenience: `sleep_ms = 1000`
    #[serde(default)]
    pub sleep_ms: Option<u64>,
    /// Legacy JSON support: `action = "send" | "sleep"`
    #[serde(default)]
    pub action: Option<String>,
    /// Legacy JSON support: `message = "C0 01"`
    #[serde(default)]
    pub message: Option<String>,
    /// Legacy JSON support: `duration = 1000`
    #[serde(default)]
    pub duration: Option<u64>,
    #[serde(default)]
    pub dec_second_byte: bool,
}

impl TestStep {
    pub fn normalized(&self) -> Option<NormalizedTestStep> {
        if let Some(msg) = &self.send {
            return Some(NormalizedTestStep::Send {
                message: msg.clone(),
                dec_second_byte: self.dec_second_byte,
            });
        }
        if let Some(ms) = self.sleep_ms {
            return Some(NormalizedTestStep::Sleep { duration_ms: ms });
        }
        match self.action.as_deref() {
            Some("send") => self.message.as_ref().map(|m| NormalizedTestStep::Send {
                message: m.clone(),
                dec_second_byte: self.dec_second_byte,
            }),
            Some("sleep") => self
                .duration
                .map(|d| NormalizedTestStep::Sleep { duration_ms: d }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedTestStep {
    Send {
        message: String,
        dec_second_byte: bool,
    },
    Sleep {
        duration_ms: u64,
    },
}

/// Also allow a single top-level filter in TOML without `[[filters]]`
#[derive(Debug, Clone, Deserialize)]
struct SingleFilterToml {
    #[serde(default = "default_filter_id")]
    pub id: String,
    #[serde(alias = "created_midi_out_port_name", default = "default_virtual_port")]
    pub virtual_port: String,
    #[serde(alias = "opened_midi_out_port_name")]
    pub target_port: Option<String>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub filters: Vec<FilterConfig>,
    #[serde(default)]
    pub tests: Vec<TestStep>,
}

impl Config {
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file '{}'", path.display()))?;
        Self::parse_str(&content, path)
    }

    pub fn parse_str(content: &str, path: &Path) -> Result<Self> {
        let trimmed = content.trim_start();
        if trimmed.starts_with('{') {
            let cfg: Config = serde_json::from_str(content)
                .with_context(|| format!("Failed to parse JSON config '{}'", path.display()))?;
            cfg.validate()?;
            Ok(cfg)
        } else {
            let raw: SingleFilterToml = toml::from_str(content)
                .with_context(|| format!("Failed to parse TOML config '{}'", path.display()))?;
            let mut filters = raw.filters;
            if let Some(target_port) = raw.target_port {
                filters.insert(
                    0,
                    FilterConfig {
                        id: raw.id,
                        virtual_port: raw.virtual_port,
                        target_port,
                        rules: raw.rules,
                    },
                );
            }
            let cfg = Config {
                filters,
                tests: raw.tests,
            };
            cfg.validate()?;
            Ok(cfg)
        }
    }

    fn validate(&self) -> Result<()> {
        if self.filters.is_empty() {
            bail!("Configuration must define at least one filter (or top-level target_port + rules)");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_load_sample_toml_and_legacy_json() {
        let sample_path = PathBuf::from("Sample.remidi");
        let cfg = Config::load_from_file(&sample_path).expect("Failed to load Sample.remidi");
        assert_eq!(cfg.filters.len(), 1);
        assert_eq!(cfg.filters[0].virtual_port, "ReMIDI");
        assert_eq!(cfg.filters[0].target_port, "01. Ethernet MIDI");
        assert_eq!(cfg.filters[0].rules.len(), 2);
        assert_eq!(cfg.tests.len(), 3);

        let legacy_json = r#"{
            "filters": [{
                "id": "direct",
                "created_midi_out_port_name": "ReMIDI",
                "opened_midi_out_port_name": "01. Ethernet MIDI",
                "rules": [
                    { "match": "C0 (\\w\\w)", "replace_by": "C1 $1", "min_delay": 500, "dec_second_byte": true }
                ]
            }],
            "tests": [
                { "action": "send", "message": "C0 01", "dec_second_byte": false },
                { "action": "sleep", "duration": 1000 }
            ]
        }"#;
        let legacy_cfg = Config::parse_str(legacy_json, Path::new("legacy.remidi"))
            .expect("Failed to parse legacy JSON .remidi");
        assert_eq!(legacy_cfg.filters.len(), 1);
        assert_eq!(legacy_cfg.filters[0].virtual_port, "ReMIDI");
        assert_eq!(legacy_cfg.filters[0].target_port, "01. Ethernet MIDI");
        assert!(legacy_cfg.filters[0].rules[0].dec_second_byte);
        assert_eq!(legacy_cfg.filters[0].rules[0].min_delay_ms, 500);
    }
}
