use crate::config::RuleConfig;
use anyhow::{Context, Result, anyhow, bail};
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

fn parse_int_or_hex(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Some(hex_part) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex_part, 16).map_err(|e| anyhow!("Invalid hex number '{}': {}", s, e))
    } else {
        s.parse::<i64>().map_err(|e| anyhow!("Invalid number '{}': {}", s, e))
    }
}

fn find_operator(s: &str) -> (&str, Option<(char, i64)>) {
    for (idx, ch) in s.char_indices() {
        if idx > 0 && (ch == '+' || ch == '-') {
            let left = s[..idx].trim();
            let right = s[idx + 1..].trim();
            if let Ok(delta) = parse_int_or_hex(right) {
                return (left, Some((ch, delta)));
            }
        }
    }
    (s.trim(), None)
}

fn resolve_target(
    target: &str,
    captures: &regex::Captures,
    input_bytes: &[u8],
    output_bytes: Option<&[u8]>,
) -> Result<(i64, Option<String>)> {
    let t = target.trim();
    if let Some(stripped) = t.strip_prefix("out.") {
        let byte_idx: usize = stripped
            .parse()
            .with_context(|| format!("Invalid out byte index '{}'", stripped))?;
        if byte_idx == 0 {
            bail!("'out' byte index must be 1-based (out.1, out.2, ...)");
        }
        let out = output_bytes.ok_or_else(|| {
            anyhow!("Cannot reference 'out.{}' in replace pattern before output is generated", byte_idx)
        })?;
        if byte_idx > out.len() {
            bail!("Reference to out.{} exceeds output length ({})", byte_idx, out.len());
        }
        let val = out[byte_idx - 1];
        return Ok((val as i64, Some(format!("{:02X}", val))));
    }

    if let Some(stripped) = t.strip_prefix("in.") {
        let byte_idx: usize = stripped
            .parse()
            .with_context(|| format!("Invalid in byte index '{}'", stripped))?;
        if byte_idx == 0 {
            bail!("'in' byte index must be 1-based (in.1, in.2, ...)");
        }
        if byte_idx > input_bytes.len() {
            bail!("Reference to in.{} exceeds input length ({})", byte_idx, input_bytes.len());
        }
        let val = input_bytes[byte_idx - 1];
        return Ok((val as i64, Some(format!("{:02X}", val))));
    }

    // Capture group: either $1, 1, or named group
    let group_str = t.strip_prefix('$').unwrap_or(t);
    if let Ok(idx) = group_str.parse::<usize>() {
        if let Some(m) = captures.get(idx) {
            let s = m.as_str().trim();
            let val = i64::from_str_radix(s, 16)
                .with_context(|| format!("Captured value '{}' in group ${} is not valid hex", s, idx))?;
            return Ok((val, Some(s.to_string())));
        } else {
            bail!("Capture group ${} did not match", idx);
        }
    }

    // Named capture group
    if let Some(m) = captures.name(group_str) {
        let s = m.as_str().trim();
        let val = i64::from_str_radix(s, 16)
            .with_context(|| format!("Captured value '{}' in group '{}' is not valid hex", s, group_str))?;
        return Ok((val, Some(s.to_string())));
    }

    bail!("Unknown expression target '{}'", target)
}

fn eval_expression(
    expr: &str,
    captures: &regex::Captures,
    input_bytes: &[u8],
    output_bytes: Option<&[u8]>,
    default_format_hex: bool,
) -> Result<String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    // Split optional format modifier: <expr>[:format]
    let (body, format_opt) = match trimmed.rsplit_once(':') {
        Some((b, fmt)) => (b.trim(), Some(fmt.trim().to_ascii_lowercase())),
        None => (trimmed, None),
    };

    let (target_part, op_and_operand) = find_operator(body);
    let (base_num, raw_str) = resolve_target(target_part, captures, input_bytes, output_bytes)?;

    let format_str = format_opt.as_deref();

    // If there is NO arithmetic and NO format specified, and raw_str is available:
    // preserve the raw string as-is (e.g. "${1}", "${ch}", "${prog}")
    if op_and_operand.is_none() && format_str.is_none()
        && let Some(raw) = raw_str
    {
        return Ok(raw);
    }

    let computed_val = match op_and_operand {
        Some(('+', delta)) => (base_num + delta).rem_euclid(256) as u8,
        Some(('-', delta)) => (base_num - delta).rem_euclid(256) as u8,
        _ => (base_num).rem_euclid(256) as u8,
    };

    match format_str {
        Some("dec") => Ok(format!("{}", computed_val)),
        Some("ch") | Some("channel") => Ok(format!("{}", (computed_val % 16) + 1)),
        Some("hex") => Ok(format!("{:02X}", computed_val)),
        Some(other) => bail!("Unknown format modifier ':{}' in expression '${{{}}}'", other, expr),
        None => {
            if default_format_hex || op_and_operand.is_some() {
                Ok(format!("{:02X}", computed_val))
            } else if let Some(raw) = raw_str {
                Ok(raw)
            } else {
                Ok(format!("{:02X}", computed_val))
            }
        }
    }
}

pub fn expand_template(
    template: &str,
    captures: &regex::Captures,
    input_bytes: &[u8],
    output_bytes: Option<&[u8]>,
    default_format_hex: bool,
) -> Result<String> {
    let mut result = String::with_capacity(template.len() + 16);
    let chars: Vec<char> = template.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '$' && i + 1 < len {
            if chars[i + 1] == '$' {
                // Escaped $$ -> $
                result.push('$');
                i += 2;
                continue;
            }
            if chars[i + 1] == '{' {
                if let Some(close_pos) = chars[i + 2..].iter().position(|&c| c == '}') {
                    let end = i + 2 + close_pos;
                    let expr_str: String = chars[i + 2..end].iter().collect();
                    let val = eval_expression(
                        &expr_str,
                        captures,
                        input_bytes,
                        output_bytes,
                        default_format_hex,
                    )?;
                    result.push_str(&val);
                    i = end + 1;
                    continue;
                }
            } else if chars[i + 1].is_ascii_digit() {
                let mut end = i + 1;
                while end < len && chars[end].is_ascii_digit() {
                    end += 1;
                }
                let num_str: String = chars[i + 1..end].iter().collect();
                let group_idx: usize = num_str.parse()?;
                if let Some(m) = captures.get(group_idx) {
                    result.push_str(m.as_str());
                } else {
                    bail!("Capture group ${} not found", group_idx);
                }
                i = end;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    Ok(result)
}

pub fn process_midi_bytes(
    rules: &mut [CompiledRule],
    input_bytes: &[u8],
    now: Instant,
) -> Result<RuleOutcome> {
    let input_hex = format_midi_hex(input_bytes);
    for (idx, rule) in rules.iter_mut().enumerate() {
        if let Some(captures) = rule.regex.captures(&input_hex) {
            if !rule.debounce.is_zero()
                && let Some(last) = rule.last_triggered
                && now.saturating_duration_since(last) < rule.debounce
            {
                let comment = rule.comment.as_deref().and_then(|c| {
                    expand_template(c, &captures, input_bytes, None, false).ok()
                });
                return Ok(RuleOutcome::FilteredByDebounce {
                    rule_index: idx,
                    comment,
                });
            }
            rule.last_triggered = Some(now);

            let replaced = expand_template(&rule.replace, &captures, input_bytes, None, true)?;
            let output_bytes = parse_midi_hex(&replaced, rule.dec_second_byte)?;

            let comment = match rule.comment.as_deref() {
                Some(c) => Some(expand_template(
                    c,
                    &captures,
                    input_bytes,
                    Some(&output_bytes),
                    false,
                )?),
                None => None,
            };

            return Ok(RuleOutcome::Matched {
                output_bytes,
                rule_index: idx,
                comment,
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

    #[test]
    fn test_template_expressions_in_replace_and_comment() {
        let mut rules = vec![
            CompiledRule::compile(&RuleConfig {
                match_pattern: r"C0 (\w\w)".to_string(),
                replace: "90 ${$1 - 1} 7F".to_string(),
                debounce_ms: 0,
                dec_second_byte: false,
                comment: Some("PC $1 -> Note ${$1 - 1:dec} (hex ${out.2}) [in: ${in.1}]".to_string()),
            })
            .unwrap(),
        ];

        let t0 = Instant::now();
        let res = process_midi_bytes(&mut rules, &[0xC0, 0x01], t0).unwrap();
        assert_eq!(
            res,
            RuleOutcome::Matched {
                output_bytes: vec![0x90, 0x00, 0x7F],
                rule_index: 0,
                comment: Some("PC 01 -> Note 0 (hex 00) [in: C0]".to_string()),
            }
        );

        // Test with higher value 0x0A (10 dec -> 9 dec)
        let res2 = process_midi_bytes(&mut rules, &[0xC0, 0x0A], t0).unwrap();
        assert_eq!(
            res2,
            RuleOutcome::Matched {
                output_bytes: vec![0x90, 0x09, 0x7F],
                rule_index: 0,
                comment: Some("PC 0A -> Note 9 (hex 09) [in: C0]".to_string()),
            }
        );
    }

    #[test]
    fn test_channel_formatting_and_named_captures() {
        let mut rules = vec![
            CompiledRule::compile(&RuleConfig {
                match_pattern: r"9(?P<ch>\w) (?P<note>\w\w) (?P<vel>\w\w)".to_string(),
                replace: "B${ch} 50 ${vel}".to_string(),
                debounce_ms: 0,
                dec_second_byte: false,
                comment: Some("Channel ${ch:ch} Note ${note:dec} -> CC 80 val ${out.3:dec}".to_string()),
            })
            .unwrap(),
        ];

        let t0 = Instant::now();
        // 92 3C 64 -> Ch 2 (0-indexed -> ch 3 1-indexed), note 0x3C = 60, vel 0x64 = 100
        let res = process_midi_bytes(&mut rules, &[0x92, 0x3C, 0x64], t0).unwrap();
        assert_eq!(
            res,
            RuleOutcome::Matched {
                output_bytes: vec![0xB2, 0x50, 0x64],
                rule_index: 0,
                comment: Some("Channel 3 Note 60 -> CC 80 val 100".to_string()),
            }
        );
    }
}
