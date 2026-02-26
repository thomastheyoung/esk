use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    String,
    Url,
    Integer,
    Number,
    Boolean,
    Email,
    Json,
    Base64,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Format::String => write!(f, "string"),
            Format::Url => write!(f, "url"),
            Format::Integer => write!(f, "integer"),
            Format::Number => write!(f, "number"),
            Format::Boolean => write!(f, "boolean"),
            Format::Email => write!(f, "email"),
            Format::Json => write!(f, "json"),
            Format::Base64 => write!(f, "base64"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Validation {
    #[serde(default)]
    pub format: Option<Format>,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<serde_yaml::Value>>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub min_length: Option<usize>,
    #[serde(default)]
    pub max_length: Option<usize>,
    #[serde(default)]
    pub range: Option<(f64, f64)>,
    #[serde(default)]
    pub optional: bool,
}

/// Coerce a list of YAML values (bool, int, string) to strings.
pub fn resolve_enum_values(raw: &[serde_yaml::Value]) -> Result<Vec<String>> {
    let mut result = Vec::with_capacity(raw.len());
    for v in raw {
        match v {
            serde_yaml::Value::Bool(b) => result.push(b.to_string()),
            serde_yaml::Value::Number(n) => result.push(n.to_string()),
            serde_yaml::Value::String(s) => result.push(s.clone()),
            other => bail!("unsupported enum value: {:?}", other),
        }
    }
    Ok(result)
}

/// Validate the spec itself at config parse time.
pub fn validate_spec(key: &str, spec: &Validation) -> Result<()> {
    // range only valid on numeric formats
    if let Some((min, max)) = spec.range {
        match spec.format {
            Some(Format::Integer) | Some(Format::Number) => {}
            _ => bail!("secret '{key}': range constraint requires format 'integer' or 'number'"),
        }
        if min > max {
            bail!("secret '{key}': range min ({min}) > max ({max})");
        }
    }

    // min_length <= max_length
    if let (Some(min), Some(max)) = (spec.min_length, spec.max_length) {
        if min > max {
            bail!("secret '{key}': min_length ({min}) > max_length ({max})");
        }
    }

    // pattern must compile
    if let Some(ref pat) = spec.pattern {
        if regex_lite::Regex::new(pat).is_err() {
            bail!("secret '{key}': invalid regex pattern '{pat}'");
        }
    }

    // enum values must pass format check if format is set
    if let Some(ref raw_values) = spec.enum_values {
        let values = resolve_enum_values(raw_values)?;
        if let Some(format) = spec.format {
            for v in &values {
                if let Err(e) = validate_format(v, format) {
                    bail!("secret '{key}': enum value '{v}' does not match format '{format}': {e}");
                }
            }
        }
    }

    Ok(())
}

/// Validate a single value against a spec. Returns a human-readable error message.
pub fn validate_value(key: &str, value: &str, spec: &Validation) -> Result<(), ValidationError> {
    let is_optional = spec.optional;

    // Empty value handling
    if value.is_empty() {
        if is_optional {
            return Ok(());
        }
        return Err(ValidationError {
            key: key.to_string(),
            message: "value is empty (set optional: true to allow)".to_string(),
        });
    }

    let mut errors = Vec::new();

    // Format check
    if let Some(format) = spec.format {
        if let Err(msg) = validate_format(value, format) {
            errors.push(msg);
        }
    }

    // Enum check
    if let Some(ref raw_values) = spec.enum_values {
        // resolve_enum_values should not fail at this point (validated at spec time)
        if let Ok(allowed) = resolve_enum_values(raw_values) {
            if !allowed.iter().any(|v| v == value) {
                errors.push(format!(
                    "expected one of [{}], got {:?}",
                    allowed.join(", "),
                    value
                ));
            }
        }
    }

    // Pattern check
    if let Some(ref pat) = spec.pattern {
        if let Ok(re) = regex_lite::Regex::new(pat) {
            if !re.is_match(value) {
                errors.push(format!("does not match pattern '{pat}'"));
            }
        }
    }

    // Length checks (character count, not byte count)
    if let Some(min) = spec.min_length {
        let len = value.chars().count();
        if len < min {
            errors.push(format!("length {} is below minimum {}", len, min));
        }
    }
    if let Some(max) = spec.max_length {
        let len = value.chars().count();
        if len > max {
            errors.push(format!("length {} exceeds maximum {}", len, max));
        }
    }

    // Range check (only meaningful for numeric formats)
    if let Some((min, max)) = spec.range {
        if let Ok(n) = value.parse::<f64>() {
            if n < min || n > max {
                errors.push(format!("value {n} is outside range [{min}, {max}]"));
            }
        }
        // If not parseable as number, the format check already caught it
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationError {
            key: key.to_string(),
            message: errors.join("; "),
        })
    }
}

/// Returns true if the value is empty or contains only whitespace.
pub fn is_effectively_empty(value: &str) -> bool {
    value.trim().is_empty()
}

fn validate_format(value: &str, format: Format) -> Result<(), String> {
    match format {
        Format::String => {
            // Any non-empty string is valid (emptiness checked earlier)
            Ok(())
        }
        Format::Url => {
            let scheme = value.split("://").next().unwrap_or("");
            if scheme.is_empty() || scheme == value {
                return Err("expected url (must contain '://')".to_string());
            }
            let after_scheme = value.split("://").nth(1).unwrap_or("");
            if after_scheme.is_empty() || after_scheme == "/" {
                return Err("expected url with host after scheme".to_string());
            }
            Ok(())
        }
        Format::Integer => {
            if value.parse::<i64>().is_err() {
                return Err(format!("expected integer, got {:?}", value));
            }
            Ok(())
        }
        Format::Number => {
            if value.parse::<f64>().is_err() {
                return Err(format!("expected number, got {:?}", value));
            }
            Ok(())
        }
        Format::Boolean => {
            let lower = value.to_lowercase();
            if !["true", "false", "1", "0", "yes", "no"].contains(&lower.as_str()) {
                return Err(format!(
                    "expected boolean (true/false/1/0/yes/no), got {:?}",
                    value
                ));
            }
            Ok(())
        }
        Format::Email => {
            let parts: Vec<&str> = value.splitn(2, '@').collect();
            if parts.len() != 2 || parts[0].is_empty() || !parts[1].contains('.') {
                return Err(format!("expected email address, got {:?}", value));
            }
            Ok(())
        }
        Format::Json => {
            if serde_json::from_str::<serde_json::Value>(value).is_err() {
                return Err(format!("expected valid JSON, got {:?}", value));
            }
            Ok(())
        }
        Format::Base64 => {
            use base64::Engine;
            if base64::engine::general_purpose::STANDARD
                .decode(value)
                .is_err()
            {
                return Err(format!("expected valid base64, got {:?}", value));
            }
            Ok(())
        }
    }
}

#[derive(Debug)]
pub struct ValidationError {
    pub key: String,
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_format(format: Format) -> Validation {
        Validation {
            format: Some(format),
            ..Default::default()
        }
    }

    // --- Format: string ---

    #[test]
    fn string_accepts_any_nonempty() {
        let spec = spec_with_format(Format::String);
        assert!(validate_value("K", "hello", &spec).is_ok());
    }

    #[test]
    fn empty_rejects_by_default() {
        let spec = spec_with_format(Format::String);
        assert!(validate_value("K", "", &spec).is_err());
    }

    #[test]
    fn empty_allowed_when_optional() {
        let spec = Validation {
            format: Some(Format::String),
            optional: true,
            ..Default::default()
        };
        assert!(validate_value("K", "", &spec).is_ok());
    }

    // --- Format: url ---

    #[test]
    fn url_valid() {
        let spec = spec_with_format(Format::Url);
        assert!(validate_value("K", "https://example.com", &spec).is_ok());
        assert!(validate_value("K", "postgres://localhost:5432/db", &spec).is_ok());
    }

    #[test]
    fn url_missing_scheme() {
        let spec = spec_with_format(Format::Url);
        assert!(validate_value("K", "example.com", &spec).is_err());
    }

    #[test]
    fn url_empty_host() {
        let spec = spec_with_format(Format::Url);
        assert!(validate_value("K", "http://", &spec).is_err());
    }

    #[test]
    fn url_empty_scheme() {
        let spec = spec_with_format(Format::Url);
        assert!(validate_value("K", "://example.com", &spec).is_err());
    }

    // --- Format: integer ---

    #[test]
    fn integer_valid() {
        let spec = spec_with_format(Format::Integer);
        assert!(validate_value("K", "42", &spec).is_ok());
        assert!(validate_value("K", "-1", &spec).is_ok());
        assert!(validate_value("K", "0", &spec).is_ok());
    }

    #[test]
    fn integer_rejects_float() {
        let spec = spec_with_format(Format::Integer);
        assert!(validate_value("K", "3.14", &spec).is_err());
    }

    #[test]
    fn integer_rejects_text() {
        let spec = spec_with_format(Format::Integer);
        assert!(validate_value("K", "abc", &spec).is_err());
    }

    // --- Format: number ---

    #[test]
    fn number_accepts_float_and_int() {
        let spec = spec_with_format(Format::Number);
        assert!(validate_value("K", "3.14", &spec).is_ok());
        assert!(validate_value("K", "42", &spec).is_ok());
        assert!(validate_value("K", "-0.5", &spec).is_ok());
    }

    #[test]
    fn number_rejects_text() {
        let spec = spec_with_format(Format::Number);
        assert!(validate_value("K", "not-a-number", &spec).is_err());
    }

    // --- Format: boolean ---

    #[test]
    fn boolean_valid_cases() {
        let spec = spec_with_format(Format::Boolean);
        for v in &[
            "true", "false", "True", "FALSE", "1", "0", "yes", "no", "YES", "No",
        ] {
            assert!(validate_value("K", v, &spec).is_ok(), "should accept {v}");
        }
    }

    #[test]
    fn boolean_rejects_invalid() {
        let spec = spec_with_format(Format::Boolean);
        assert!(validate_value("K", "maybe", &spec).is_err());
    }

    // --- Format: email ---

    #[test]
    fn email_valid() {
        let spec = spec_with_format(Format::Email);
        assert!(validate_value("K", "user@example.com", &spec).is_ok());
    }

    #[test]
    fn email_missing_at() {
        let spec = spec_with_format(Format::Email);
        assert!(validate_value("K", "userexample.com", &spec).is_err());
    }

    #[test]
    fn email_no_dot_after_at() {
        let spec = spec_with_format(Format::Email);
        assert!(validate_value("K", "user@localhost", &spec).is_err());
    }

    // --- Format: json ---

    #[test]
    fn json_valid() {
        let spec = spec_with_format(Format::Json);
        assert!(validate_value("K", r#"{"key":"val"}"#, &spec).is_ok());
        assert!(validate_value("K", "[1,2,3]", &spec).is_ok());
        assert!(validate_value("K", "\"hello\"", &spec).is_ok());
    }

    #[test]
    fn json_invalid() {
        let spec = spec_with_format(Format::Json);
        assert!(validate_value("K", "{not json}", &spec).is_err());
    }

    // --- Format: base64 ---

    #[test]
    fn base64_valid() {
        let spec = spec_with_format(Format::Base64);
        assert!(validate_value("K", "aGVsbG8=", &spec).is_ok());
        assert!(validate_value("K", "dGVzdA==", &spec).is_ok());
    }

    #[test]
    fn base64_invalid() {
        let spec = spec_with_format(Format::Base64);
        assert!(validate_value("K", "not!valid!base64$$$", &spec).is_err());
    }

    // --- Enum ---

    #[test]
    fn enum_accepts_matching_value() {
        let spec = Validation {
            enum_values: Some(vec![
                serde_yaml::Value::String("dev".into()),
                serde_yaml::Value::String("prod".into()),
            ]),
            ..Default::default()
        };
        assert!(validate_value("K", "dev", &spec).is_ok());
    }

    #[test]
    fn enum_rejects_non_matching() {
        let spec = Validation {
            enum_values: Some(vec![
                serde_yaml::Value::String("dev".into()),
                serde_yaml::Value::String("prod".into()),
            ]),
            ..Default::default()
        };
        let err = validate_value("K", "staging", &spec).unwrap_err();
        assert!(err.message.contains("expected one of"));
    }

    #[test]
    fn enum_coerces_booleans() {
        let spec = Validation {
            enum_values: Some(vec![
                serde_yaml::Value::Bool(true),
                serde_yaml::Value::Bool(false),
            ]),
            ..Default::default()
        };
        assert!(validate_value("K", "true", &spec).is_ok());
        assert!(validate_value("K", "false", &spec).is_ok());
        assert!(validate_value("K", "yes", &spec).is_err());
    }

    #[test]
    fn enum_coerces_numbers() {
        let spec = Validation {
            enum_values: Some(vec![
                serde_yaml::Value::Number(serde_yaml::Number::from(80)),
                serde_yaml::Value::Number(serde_yaml::Number::from(443)),
            ]),
            ..Default::default()
        };
        assert!(validate_value("K", "80", &spec).is_ok());
        assert!(validate_value("K", "443", &spec).is_ok());
        assert!(validate_value("K", "8080", &spec).is_err());
    }

    // --- Pattern ---

    #[test]
    fn pattern_matches() {
        let spec = Validation {
            pattern: Some(r"^sk_[a-z]+$".to_string()),
            ..Default::default()
        };
        assert!(validate_value("K", "sk_live", &spec).is_ok());
    }

    #[test]
    fn pattern_rejects() {
        let spec = Validation {
            pattern: Some(r"^sk_[a-z]+$".to_string()),
            ..Default::default()
        };
        let err = validate_value("K", "pk_test", &spec).unwrap_err();
        assert!(err.message.contains("pattern"));
    }

    // --- Range ---

    #[test]
    fn range_within() {
        let spec = Validation {
            format: Some(Format::Integer),
            range: Some((1.0, 65535.0)),
            ..Default::default()
        };
        assert!(validate_value("K", "80", &spec).is_ok());
        assert!(validate_value("K", "1", &spec).is_ok());
        assert!(validate_value("K", "65535", &spec).is_ok());
    }

    #[test]
    fn range_outside() {
        let spec = Validation {
            format: Some(Format::Integer),
            range: Some((1.0, 65535.0)),
            ..Default::default()
        };
        let err = validate_value("K", "0", &spec).unwrap_err();
        assert!(err.message.contains("outside range"));
        let err = validate_value("K", "99999", &spec).unwrap_err();
        assert!(err.message.contains("outside range"));
    }

    // --- Length ---

    #[test]
    fn length_within_bounds() {
        let spec = Validation {
            min_length: Some(3),
            max_length: Some(10),
            ..Default::default()
        };
        assert!(validate_value("K", "abc", &spec).is_ok());
        assert!(validate_value("K", "abcdefghij", &spec).is_ok());
    }

    #[test]
    fn length_too_short() {
        let spec = Validation {
            min_length: Some(5),
            ..Default::default()
        };
        let err = validate_value("K", "abc", &spec).unwrap_err();
        assert!(err.message.contains("below minimum"));
    }

    #[test]
    fn length_too_long() {
        let spec = Validation {
            max_length: Some(3),
            ..Default::default()
        };
        let err = validate_value("K", "abcde", &spec).unwrap_err();
        assert!(err.message.contains("exceeds maximum"));
    }

    // --- validate_spec ---

    #[test]
    fn spec_rejects_range_on_non_numeric() {
        let spec = Validation {
            format: Some(Format::String),
            range: Some((1.0, 10.0)),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec).is_err());
    }

    #[test]
    fn spec_rejects_inverted_range() {
        let spec = Validation {
            format: Some(Format::Integer),
            range: Some((100.0, 1.0)),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec).is_err());
    }

    #[test]
    fn spec_rejects_inverted_length() {
        let spec = Validation {
            min_length: Some(10),
            max_length: Some(3),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec).is_err());
    }

    #[test]
    fn spec_rejects_bad_regex() {
        let spec = Validation {
            pattern: Some("[invalid".to_string()),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec).is_err());
    }

    #[test]
    fn spec_rejects_enum_values_failing_format() {
        let spec = Validation {
            format: Some(Format::Integer),
            enum_values: Some(vec![serde_yaml::Value::String("not_a_number".into())]),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec).is_err());
    }

    #[test]
    fn spec_accepts_valid() {
        let spec = Validation {
            format: Some(Format::Integer),
            range: Some((1.0, 100.0)),
            enum_values: Some(vec![serde_yaml::Value::Number(serde_yaml::Number::from(
                42,
            ))]),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec).is_ok());
    }

    // --- resolve_enum_values ---

    // --- is_effectively_empty ---

    #[test]
    fn effectively_empty_true_cases() {
        assert!(is_effectively_empty(""));
        assert!(is_effectively_empty("   "));
        assert!(is_effectively_empty("\t"));
        assert!(is_effectively_empty("\n"));
        assert!(is_effectively_empty("  \t\n  "));
    }

    #[test]
    fn effectively_empty_false_cases() {
        assert!(!is_effectively_empty("a"));
        assert!(!is_effectively_empty(" a "));
        assert!(!is_effectively_empty("\"\""));
        assert!(!is_effectively_empty("0"));
        assert!(!is_effectively_empty("false"));
    }

    // --- resolve_enum_values ---

    #[test]
    fn resolve_enum_mixed_types() {
        let raw = vec![
            serde_yaml::Value::String("dev".into()),
            serde_yaml::Value::Bool(true),
            serde_yaml::Value::Number(serde_yaml::Number::from(42)),
        ];
        let values = resolve_enum_values(&raw).unwrap();
        assert_eq!(values, vec!["dev", "true", "42"]);
    }
}
