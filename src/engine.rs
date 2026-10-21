use crate::config::RuleConfig;
use anyhow::{Context, Result, bail};
use regex::Regex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct CompiledRule {
    pub regex: Regex,
    pub replace: String,
    pub debounce: Duration,
    pub dec_second_byte: bool,
    pub comment: Option<String>,
    pub last_triggered: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RuleOutcome {
    Matched {
        output_bytes: Vec<u8>,
        rule_index: usize,
        comment: Option<String>,
    },
    FilteredByDebounce {
        rule_index: usize,
        comment: Option<String>,
    },
    NoMatch,
}

impl CompiledRule {
    pub fn compile(cfg: &RuleConfig) -> Result<Self> {
        let pattern = if cfg.match_pattern.starts_with('^') || cfg.match_pattern.ends_with('$') {
            cfg.match_pattern.clone()
        } else {
            format!("^(?:{})$", cfg.match_pattern.trim())
        };
        let regex = Regex::new(&pattern)
            .with_context(|| format!("Invalid regex pattern '{}'", cfg.match_pattern))?;
        Ok(Self {
            regex,
            replace: cfg.replace.clone(),
            debounce: Duration::from_millis(cfg.debounce_ms),
            dec_second_byte: cfg.dec_second_byte,
            comment: cfg.comment.clone(),
            last_triggered: None,
        })
    }
}

pub fn format_midi_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_midi_hex(hex_str: &str, dec_second_byte: bool) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for token in hex_str.split_whitespace() {
        let b = u8::from_str_radix(token, 16)
            .with_context(|| format!("Invalid hex byte '{}' in '{}'", token, hex_str))?;
        bytes.push(b);
    }
    if bytes.is_empty() {
        bail!("Replaced MIDI message is empty");
    }
    if dec_second_byte && bytes.len() >= 2 {
        bytes[1] = bytes[1].wrapping_sub(1);
    }
    Ok(bytes)
}

/// Convert C++/PCRE style `$1`, `$2` capture group references into Rust `regex` `${1}`, `${2}`
/// so that e.g. `"C$1 $2"` doesn't get interpreted by `regex` as capture group `$1` followed by space,
/// or `"$1a"` as group `1a`.
fn normalize_replacement_syntax(replace: &str) -> String {
    let mut out = String::with_capacity(replace.len() + 8);
    let chars: Vec<char> = replace.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
            out.push_str("${");
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                out.push(chars[i]);
                i += 1;
            }
            out.push('}');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

pub fn process_midi_bytes(
    rules: &mut [CompiledRule],
    input_bytes: &[u8],
    now: Instant,
) -> Result<RuleOutcome> {
    let input_hex = format_midi_hex(input_bytes);
    for (idx, rule) in rules.iter_mut().enumerate() {
        if rule.regex.is_match(&input_hex) {
            if !rule.debounce.is_zero()
                && let Some(last) = rule.last_triggered
                && now.saturating_duration_since(last) < rule.debounce
            {
                return Ok(RuleOutcome::FilteredByDebounce {
                    rule_index: idx,
                    comment: rule.comment.clone(),
                });
            }
            rule.last_triggered = Some(now);
            let normalized_rep = normalize_replacement_syntax(&rule.replace);
            let replaced = rule.regex.replace(&input_hex, normalized_rep.as_str());
            let output_bytes = parse_midi_hex(&replaced, rule.dec_second_byte)?;
            return Ok(RuleOutcome::Matched {
                output_bytes,
                rule_index: idx,
                comment: rule.comment.clone(),
            });
        }
    }
    Ok(RuleOutcome::NoMatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_program_change_to_note_on_and_delay() {
        let mut rules = vec![
            CompiledRule::compile(&RuleConfig {
                match_pattern: r"C0 (\w\w)".to_string(),
                replace: "90 $1 7F".to_string(),
                debounce_ms: 500,
                dec_second_byte: false,
                comment: Some("PC -> Note On".to_string()),
            })
            .unwrap(),
            CompiledRule::compile(&RuleConfig {
                match_pattern: r"90 01 \w\w".to_string(),
                replace: "B0 50 00".to_string(),
                debounce_ms: 0,
                dec_second_byte: false,
                comment: Some("Tap Tempo".to_string()),
            })
            .unwrap(),
        ];

        let t0 = Instant::now();
        let res1 = process_midi_bytes(&mut rules, &[0xC0, 0x04], t0).unwrap();
        assert_eq!(
            res1,
            RuleOutcome::Matched {
                output_bytes: vec![0x90, 0x04, 0x7F],
                rule_index: 0,
                comment: Some("PC -> Note On".to_string()),
            }
        );

        // Within 500ms -> filtered by debounce
        let res2 = process_midi_bytes(&mut rules, &[0xC0, 0x05], t0 + Duration::from_millis(200)).unwrap();
        assert_eq!(
            res2,
            RuleOutcome::FilteredByDebounce {
                rule_index: 0,
                comment: Some("PC -> Note On".to_string()),
            }
        );

        // After 500ms -> matched again
        let res3 = process_midi_bytes(&mut rules, &[0xC0, 0x05], t0 + Duration::from_millis(501)).unwrap();
        assert_eq!(
            res3,
            RuleOutcome::Matched {
                output_bytes: vec![0x90, 0x05, 0x7F],
                rule_index: 0,
                comment: Some("PC -> Note On".to_string()),
            }
        );
    }

    #[test]
    fn test_dec_second_byte() {
        let mut rules = vec![
            CompiledRule::compile(&RuleConfig {
                match_pattern: r"C0 (\w\w)".to_string(),
                replace: "C1 $1".to_string(),
                debounce_ms: 0,
                dec_second_byte: true,
                comment: None,
            })
            .unwrap(),
        ];

        let t0 = Instant::now();
        let res = process_midi_bytes(&mut rules, &[0xC0, 0x01], t0).unwrap();
        assert_eq!(
            res,
            RuleOutcome::Matched {
                output_bytes: vec![0xC1, 0x00],
                rule_index: 0,
                comment: None,
            }
        );
    }
}
