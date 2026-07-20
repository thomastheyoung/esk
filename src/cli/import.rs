use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{self, Config};
use crate::store::{validate_key, SecretStore};

pub struct ImportOptions<'a> {
    pub path: &'a Path,
    pub env: &'a str,
    pub group: Option<&'a str>,
}

pub fn run(config: &Config, opts: &ImportOptions<'_>) -> Result<()> {
    config.validate_env(opts.env)?;
    let group = opts.group.unwrap_or("Imported");
    config::validate_secret_group(group)?;
    let contents = std::fs::read_to_string(opts.path)
        .with_context(|| format!("failed to read dotenv file {}", opts.path.display()))?;
    let values = parse_dotenv(&contents)?;
    if values.is_empty() {
        bail!("dotenv file {} contains no values", opts.path.display());
    }

    for (key, value) in &values {
        validate_key(key)?;
        if let Some((_, def)) = config.find_secret(key) {
            if let Some(ref spec) = def.validate {
                crate::validate::validate_value(key, value, spec)
                    .map_err(|e| anyhow::anyhow!("validation failed for {key}: {e}"))?;
            }
        }
    }

    let config_path = config.root.join("esk.yaml");
    let keys_to_register: Vec<&str> = values
        .keys()
        .filter(|key| config.find_secret(key).is_none())
        .map(String::as_str)
        .collect();
    let added = config::add_secrets_to_config(&config_path, &keys_to_register, group)?;

    let payload = SecretStore::open(&config.root)?.set_many(opts.env, &values)?;
    cliclack::log::success(format!(
        "Imported {} value(s) into {} (v{}){}",
        values.len(),
        opts.env,
        payload.version,
        if added == 0 {
            String::new()
        } else {
            format!(", added {added} key(s) to esk.yaml")
        }
    ))?;
    Ok(())
}

pub(crate) fn parse_dotenv(contents: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for (line_number, raw_line) in contents.lines().enumerate() {
        let line_number = line_number + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            bail!("invalid dotenv line {line_number}: expected KEY=VALUE");
        };
        let key = raw_key.trim();
        validate_key(key).with_context(|| format!("invalid dotenv key on line {line_number}"))?;
        if values.contains_key(key) {
            bail!("duplicate dotenv key '{key}' on line {line_number}");
        }
        values.insert(key.to_string(), parse_value(raw_value.trim(), line_number)?);
    }
    Ok(values)
}

fn parse_value(value: &str, line_number: usize) -> Result<String> {
    if value.len() >= 2 {
        let quote = value.as_bytes()[0] as char;
        if (quote == '\'' || quote == '"') && value.ends_with(quote) {
            let inner = &value[1..value.len() - 1];
            if quote == '\'' {
                return Ok(inner.to_string());
            }
            let mut parsed = String::with_capacity(inner.len());
            let mut escaped = false;
            for c in inner.chars() {
                if escaped {
                    parsed.push(match c {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '\\' => '\\',
                        '"' => '"',
                        other => other,
                    });
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else {
                    parsed.push(c);
                }
            }
            if escaped {
                bail!("unterminated escape on dotenv line {line_number}");
            }
            return Ok(parsed);
        }
        if quote == '\'' || quote == '"' {
            bail!("unterminated quote on dotenv line {line_number}");
        }
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_exports_and_quotes() {
        let values = parse_dotenv(
            "# comment\nexport API_KEY=plain\nNAME=\"hello\\nworld\"\nSINGLE='literal # value'\nEMPTY=\n",
        )
        .unwrap();
        assert_eq!(values["API_KEY"], "plain");
        assert_eq!(values["NAME"], "hello\nworld");
        assert_eq!(values["SINGLE"], "literal # value");
        assert_eq!(values["EMPTY"], "");
    }

    #[test]
    fn rejects_malformed_and_duplicate_lines() {
        assert!(parse_dotenv("BROKEN").is_err());
        assert!(parse_dotenv("A=1\nA=2").is_err());
        assert!(parse_dotenv("A=\"unterminated").is_err());
        assert!(parse_dotenv("1BAD=value").is_err());
    }
}
