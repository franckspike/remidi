use crate::midi::LogLevel;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{Document, value};

pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.default.toml");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub pipelines: Vec<PipelineConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    #[serde(default = "default_pipeline_id")]
    pub id: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_virtual_port")]
    pub virtual_port: String,
    pub target_port: String,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub test: Vec<String>,
}

fn default_pipeline_id() -> String {
    "Pipeline".to_string()
}

fn default_enabled() -> bool {
    true
}

fn default_virtual_port() -> String {
    "Main".to_string()
}

impl PipelineConfig {
    pub fn full_virtual_port_name(&self) -> String {
        #[cfg(target_os = "windows")]
        {
            format!("ReMIDI - {}", self.virtual_port)
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.virtual_port.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    #[serde(rename = "match")]
    pub match_pattern: String,
    pub replace: String,
    #[serde(default)]
    pub debounce_ms: u64,
    #[serde(default)]
    pub dec_second_byte: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestAction {
    Send(String),
    Sleep(u64),
}

impl PipelineConfig {
    pub fn parse_test_steps(&self) -> Result<Vec<TestAction>> {
        let mut actions = Vec::with_capacity(self.test.len());
        for raw in &self.test {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("sleep") {
                let ms_str = rest.trim();
                let ms: u64 = ms_str.parse().with_context(|| {
                    format!("Invalid sleep duration '{}' in test step '{}'", ms_str, raw)
                })?;
                actions.push(TestAction::Sleep(ms));
            } else {
                actions.push(TestAction::Send(trimmed.to_string()));
            }
        }
        Ok(actions)
    }
}

pub fn default_user_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Could not determine OS user configuration directory")?;
    Ok(base.join("remidi").join("config.toml"))
}

pub fn ensure_user_config_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory '{}'", parent.display())
            })?;
        }
        fs::write(path, DEFAULT_CONFIG_TEMPLATE).with_context(|| {
            format!("Failed to create default config file '{}'", path.display())
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default)]
    pub pause_all: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            start_minimized: false,
            log_level: LogLevel::Info,
            pause_all: false,
        }
    }
}

#[derive(Deserialize)]
struct RawAppSettings {
    #[serde(default)]
    start_minimized: bool,
    #[serde(default)]
    log_level: Option<LogLevel>,
    #[serde(default)]
    verbose: Option<bool>,
    #[serde(default)]
    only_errors: Option<bool>,
    #[serde(default)]
    pause_all: bool,
}

impl AppSettings {
    pub fn load_from_file(path: &Path) -> Self {
        if let Ok(content) = fs::read_to_string(path)
            && let Ok(raw) = toml::from_str::<RawAppSettings>(&content)
        {
            let log_level = if let Some(lvl) = raw.log_level {
                lvl
            } else if raw.only_errors == Some(true) {
                LogLevel::Error
            } else if raw.verbose == Some(true) {
                LogLevel::Verbose
            } else {
                LogLevel::Info
            };
            return Self {
                start_minimized: raw.start_minimized,
                log_level,
                pause_all: raw.pause_all,
            };
        }
        Self::default()
    }

    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(path, content)?;
        Ok(())
    }
}

impl Config {
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file '{}'", path.display()))?;
        Self::parse_str(&content, path)
    }

    pub fn parse_str(content: &str, path: &Path) -> Result<Self> {
        let cfg: Config = toml::from_str(content)
            .with_context(|| format!("Failed to parse TOML config '{}'", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.pipelines.is_empty() {
            bail!("Configuration must define at least one [[pipelines]] section");
        }
        for p in &self.pipelines {
            let _ = p.parse_test_steps()?;
        }
        Ok(())
    }

    /// Updates `enabled` or `target_port` for a given pipeline index in the TOML file
    /// while preserving all comments and formatting.
    pub fn save_pipeline_update(
        path: &Path,
        pipeline_idx: usize,
        new_enabled: Option<bool>,
        new_target_port: Option<&str>,
    ) -> Result<()> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read '{}' for saving", path.display()))?;
        let mut doc = content
            .parse::<Document>()
            .with_context(|| format!("Failed to parse '{}' as TOML document", path.display()))?;

        let pipelines = doc
            .get_mut("pipelines")
            .and_then(|item| item.as_array_of_tables_mut())
            .context("Missing [[pipelines]] array of tables in config.toml")?;

        let table = pipelines
            .get_mut(pipeline_idx)
            .with_context(|| format!("Pipeline index {} out of range", pipeline_idx))?;

        if let Some(en) = new_enabled {
            table["enabled"] = value(en);
        }
        if let Some(tp) = new_target_port {
            table["target_port"] = value(tp);
        }

        fs::write(path, doc.to_string())
            .with_context(|| format!("Failed to write updated config to '{}'", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_default_template_and_test_steps() {
        let cfg = Config::parse_str(DEFAULT_CONFIG_TEMPLATE, Path::new("config.default.toml"))
            .expect("Failed to parse config.default.toml");
        assert_eq!(cfg.pipelines.len(), 2);
        assert_eq!(cfg.pipelines[0].id, "Soleman");
        assert!(cfg.pipelines[0].enabled);
        assert_eq!(cfg.pipelines[0].virtual_port, "Main");
        #[cfg(target_os = "windows")]
        assert_eq!(cfg.pipelines[0].full_virtual_port_name(), "ReMIDI - Main");
        #[cfg(not(target_os = "windows"))]
        assert_eq!(cfg.pipelines[0].full_virtual_port_name(), "Main");
        assert_eq!(
            cfg.pipelines[0].parse_test_steps().unwrap(),
            vec![
                TestAction::Send("C0 01".to_string()),
                TestAction::Sleep(1000),
                TestAction::Send("C0 04".to_string()),
            ]
        );
        assert_eq!(cfg.pipelines[0].rules[0].debounce_ms, 500);
        assert!(!cfg.pipelines[1].enabled);
        assert_eq!(cfg.pipelines[1].id, "G-Major");
        assert_eq!(cfg.pipelines[1].rules[0].replace, "C1 ${$1 - 1}");
    }

    #[test]
    fn test_app_settings_default_and_roundtrip() {
        let defaults = AppSettings::default();
        assert!(!defaults.start_minimized);
        assert_eq!(defaults.log_level, LogLevel::Info);
        assert!(!defaults.pause_all);

        let toml_str = toml::to_string_pretty(&defaults).unwrap();
        let parsed: AppSettings = toml::from_str(&toml_str).unwrap();
        assert_eq!(defaults, parsed);

        let custom = AppSettings {
            start_minimized: true,
            log_level: LogLevel::Verbose,
            pause_all: true,
        };
        let custom_toml = toml::to_string_pretty(&custom).unwrap();
        let custom_parsed: AppSettings = toml::from_str(&custom_toml).unwrap();
        assert_eq!(custom, custom_parsed);

        // Test backward-compatible legacy settings loading
        let legacy_verbose_toml = "start_minimized = false\nverbose = true\npause_all = false\n";
        let temp_dir = std::env::temp_dir();
        let p_verb = temp_dir.join("remidi_legacy_verb.toml");
        fs::write(&p_verb, legacy_verbose_toml).unwrap();
        let loaded = AppSettings::load_from_file(&p_verb);
        assert_eq!(loaded.log_level, LogLevel::Verbose);
        let _ = fs::remove_file(p_verb);

        let legacy_err_toml = "start_minimized = true\nonly_errors = true\npause_all = false\n";
        let p_err = temp_dir.join("remidi_legacy_err.toml");
        fs::write(&p_err, legacy_err_toml).unwrap();
        let loaded_err = AppSettings::load_from_file(&p_err);
        assert_eq!(loaded_err.log_level, LogLevel::Error);
        assert!(loaded_err.start_minimized);
        let _ = fs::remove_file(p_err);
    }
}
