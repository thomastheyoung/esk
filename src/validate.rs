use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    pub enum_values: Option<Vec<serde_json::Value>>,
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

    /// Required when all conditions match. Key -> value, `"*"` = any non-empty value.
    #[serde(default)]
    pub required_if: Option<BTreeMap<String, String>>,

    /// Required when any listed secret has a value. Declare on both sides for symmetry.
    #[serde(default)]
    pub required_with: Option<Vec<String>>,

    /// Not required when any listed secret has a value.
    #[serde(default)]
    pub required_unless: Option<Vec<String>>,
}

impl Validation {
    /// Returns true if this spec has any cross-field rules.
    pub fn has_cross_field_rules(&self) -> bool {
        self.required_if.is_some() || self.required_with.is_some() || self.required_unless.is_some()
    }

    /// Collects all secret keys referenced by cross-field rules.
    pub fn referenced_keys(&self) -> BTreeSet<&str> {
        let mut keys = BTreeSet::new();
        if let Some(ref map) = self.required_if {
            for k in map.keys() {
                keys.insert(k.as_str());
            }
        }
        if let Some(ref list) = self.required_with {
            for k in list {
                keys.insert(k.as_str());
            }
        }
        if let Some(ref list) = self.required_unless {
            for k in list {
                keys.insert(k.as_str());
            }
        }
        keys
    }
}

/// A cross-field validation violation found during deploy or status checks.
#[derive(Debug, Clone)]
pub struct CrossFieldViolation {
    pub key: String,
    pub env: String,
    pub message: String,
}

/// Coerce a list of values (bool, number, string) to strings.
pub fn resolve_enum_values(raw: &[serde_json::Value]) -> Result<Vec<String>> {
    let mut result = Vec::with_capacity(raw.len());
    for v in raw {
        match v {
            serde_json::Value::Bool(b) => result.push(b.to_string()),
            serde_json::Value::Number(n) => result.push(n.to_string()),
            serde_json::Value::String(s) => result.push(s.clone()),
            other => bail!("unsupported enum value: {other:?}"),
        }
    }
    Ok(result)
}

/// Validate the spec itself at config parse time.
pub fn validate_spec(key: &str, spec: &Validation, known_keys: &BTreeSet<&str>) -> Result<()> {
    // range only valid on numeric formats
    if let Some((min, max)) = spec.range {
        match spec.format {
            Some(Format::Integer | Format::Number) => {}
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

    // Cross-field rule validation
    if let Some(ref map) = spec.required_if {
        if map.is_empty() {
            bail!("secret '{key}': required_if must not be empty");
        }
        for ref_key in map.keys() {
            validate_cross_field_ref(key, "required_if", ref_key, known_keys)?;
        }
    }
    if let Some(ref list) = spec.required_with {
        if list.is_empty() {
            bail!("secret '{key}': required_with must not be empty");
        }
        for ref_key in list {
            validate_cross_field_ref(key, "required_with", ref_key, known_keys)?;
        }
    }
    if let Some(ref list) = spec.required_unless {
        if list.is_empty() {
            bail!("secret '{key}': required_unless must not be empty");
        }
        for ref_key in list {
            validate_cross_field_ref(key, "required_unless", ref_key, known_keys)?;
        }
    }

    Ok(())
}

fn validate_cross_field_ref(
    key: &str,
    rule: &str,
    ref_key: &str,
    known_keys: &BTreeSet<&str>,
) -> Result<()> {
    if ref_key == key {
        bail!("secret '{key}': {rule} references itself");
    }
    if !known_keys.contains(ref_key) {
        let candidates: Vec<&str> = known_keys.iter().copied().collect();
        let hint = crate::suggest::closest(ref_key, &candidates)
            .map(|s| format!(" (did you mean '{s}'?)"))
            .unwrap_or_default();
        bail!("secret '{key}': {rule} references unknown key '{ref_key}'{hint}");
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
            errors.push(format!("length {len} is below minimum {min}"));
        }
    }
    if let Some(max) = spec.max_length {
        let len = value.chars().count();
        if len > max {
            errors.push(format!("length {len} exceeds maximum {max}"));
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

/// Validate cross-field rules for all secrets in a given environment.
///
/// `specs` maps secret key → validation spec (only secrets with cross-field rules).
/// `secrets` is the full store map (composite keys like `"KEY:env"`).
pub fn validate_cross_field(
    specs: &BTreeMap<&str, &Validation>,
    secrets: &BTreeMap<String, String>,
    env: &str,
) -> Vec<CrossFieldViolation> {
    let mut violations = Vec::new();

    for (&key, spec) in specs {
        if !spec.has_cross_field_rules() {
            continue;
        }

        let composite = format!("{key}:{env}");
        let value = secrets
            .get(&composite)
            .map_or("", std::string::String::as_str);
        let has_value = !value.is_empty();

        // required_if: all conditions match (AND) → secret must have a non-empty value
        if let Some(ref conditions) = spec.required_if {
            let all_match = conditions.iter().all(|(cond_key, cond_val)| {
                let cond_composite = format!("{cond_key}:{env}");
                let actual = secrets
                    .get(&cond_composite)
                    .map_or("", std::string::String::as_str);
                if cond_val == "*" {
                    !actual.is_empty()
                } else {
                    actual == cond_val
                }
            });

            if all_match && !has_value {
                let reasons: Vec<String> = conditions
                    .iter()
                    .map(|(k, v)| {
                        if v == "*" {
                            format!("{k} is set")
                        } else {
                            format!("{k} = \"{v}\"")
                        }
                    })
                    .collect();
                violations.push(CrossFieldViolation {
                    key: key.to_string(),
                    env: env.to_string(),
                    message: format!("required because {}", reasons.join(" and ")),
                });
            }
        }

        // required_with: any listed peer has a non-empty value → this must too
        if let Some(ref peers) = spec.required_with {
            if !has_value {
                for peer in peers {
                    let peer_composite = format!("{peer}:{env}");
                    let peer_val = secrets
                        .get(&peer_composite)
                        .map_or("", std::string::String::as_str);
                    if !peer_val.is_empty() {
                        violations.push(CrossFieldViolation {
                            key: key.to_string(),
                            env: env.to_string(),
                            message: format!("required because {peer} is set"),
                        });
                        break;
                    }
                }
            }
        }

        // required_unless: none of the alternatives has a non-empty value → this must
        if let Some(ref alternatives) = spec.required_unless {
            if !has_value {
                let any_alt_set = alternatives.iter().any(|alt| {
                    let alt_composite = format!("{alt}:{env}");
                    let alt_val = secrets
                        .get(&alt_composite)
                        .map_or("", std::string::String::as_str);
                    !alt_val.is_empty()
                });

                if !any_alt_set {
                    let names = alternatives.join(", ");
                    violations.push(CrossFieldViolation {
                        key: key.to_string(),
                        env: env.to_string(),
                        message: format!("required because none of {names} is set"),
                    });
                }
            }
        }
    }

    violations
}

/// DFS-based cycle detection on the cross-field dependency graph.
///
/// Only `required_if` and `required_unless` participate in cycle detection.
/// `required_with` is excluded because mutual declaration is the intended pattern.
pub fn detect_cross_field_cycles(specs: &BTreeMap<&str, &Validation>) -> Result<()> {
    // Standard three-color DFS
    const WHITE: u8 = 0;
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;

    // Build adjacency list from required_if and required_unless only
    let mut graph: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (&key, spec) in specs {
        let mut refs = BTreeSet::new();
        if let Some(ref map) = spec.required_if {
            for k in map.keys() {
                refs.insert(k.as_str());
            }
        }
        if let Some(ref list) = spec.required_unless {
            for k in list {
                refs.insert(k.as_str());
            }
        }
        if !refs.is_empty() {
            graph.insert(key, refs);
        }
    }

    let mut color: BTreeMap<&str, u8> = BTreeMap::new();
    let mut parent: BTreeMap<&str, &str> = BTreeMap::new();

    for &start in graph.keys() {
        if *color.get(start).unwrap_or(&WHITE) != WHITE {
            continue;
        }

        let mut stack = vec![(start, false)]; // (node, returning)
        while let Some((node, returning)) = stack.pop() {
            if returning {
                color.insert(node, BLACK);
                continue;
            }

            color.insert(node, GRAY);
            stack.push((node, true)); // push return marker

            if let Some(neighbors) = graph.get(node) {
                for &neighbor in neighbors {
                    match *color.get(neighbor).unwrap_or(&WHITE) {
                        GRAY => {
                            // Found cycle — reconstruct path
                            let mut cycle = vec![neighbor, node];
                            let mut cur = node;
                            while cur != neighbor {
                                if let Some(&p) = parent.get(cur) {
                                    cycle.push(p);
                                    cur = p;
                                } else {
                                    break;
                                }
                            }
                            cycle.reverse();
                            let path = cycle.join(" -> ");
                            bail!("circular cross-field reference: {path}");
                        }
                        WHITE => {
                            parent.insert(neighbor, node);
                            stack.push((neighbor, false));
                        }
                        _ => {} // BLACK — already fully explored
                    }
                }
            }
        }
    }

    Ok(())
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
                return Err(format!("expected integer, got {value:?}"));
            }
            Ok(())
        }
        Format::Number => {
            if value.parse::<f64>().is_err() {
                return Err(format!("expected number, got {value:?}"));
            }
            Ok(())
        }
        Format::Boolean => {
            let lower = value.to_lowercase();
            if !["true", "false", "1", "0", "yes", "no"].contains(&lower.as_str()) {
                return Err(format!(
                    "expected boolean (true/false/1/0/yes/no), got {value:?}"
                ));
            }
            Ok(())
        }
        Format::Email => {
            let parts: Vec<&str> = value.splitn(2, '@').collect();
            if parts.len() != 2 || parts[0].is_empty() || !parts[1].contains('.') {
                return Err(format!("expected email address, got {value:?}"));
            }
            Ok(())
        }
        Format::Json => {
            if serde_json::from_str::<serde_json::Value>(value).is_err() {
                return Err(format!("expected valid JSON, got {value:?}"));
            }
            Ok(())
        }
        Format::Base64 => {
            use base64::Engine;
            if base64::engine::general_purpose::STANDARD
                .decode(value)
                .is_err()
            {
                return Err(format!("expected valid base64, got {value:?}"));
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
                serde_json::Value::String("dev".into()),
                serde_json::Value::String("prod".into()),
            ]),
            ..Default::default()
        };
        assert!(validate_value("K", "dev", &spec).is_ok());
    }

    #[test]
    fn enum_rejects_non_matching() {
        let spec = Validation {
            enum_values: Some(vec![
                serde_json::Value::String("dev".into()),
                serde_json::Value::String("prod".into()),
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
                serde_json::Value::Bool(true),
                serde_json::Value::Bool(false),
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
                serde_json::Value::Number(serde_json::Number::from(80)),
                serde_json::Value::Number(serde_json::Number::from(443)),
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

    fn known<'a>(keys: &'a [&'a str]) -> BTreeSet<&'a str> {
        keys.iter().copied().collect()
    }

    #[test]
    fn spec_rejects_range_on_non_numeric() {
        let spec = Validation {
            format: Some(Format::String),
            range: Some((1.0, 10.0)),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec, &known(&["K"])).is_err());
    }

    #[test]
    fn spec_rejects_inverted_range() {
        let spec = Validation {
            format: Some(Format::Integer),
            range: Some((100.0, 1.0)),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec, &known(&["K"])).is_err());
    }

    #[test]
    fn spec_rejects_inverted_length() {
        let spec = Validation {
            min_length: Some(10),
            max_length: Some(3),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec, &known(&["K"])).is_err());
    }

    #[test]
    fn spec_rejects_bad_regex() {
        let spec = Validation {
            pattern: Some("[invalid".to_string()),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec, &known(&["K"])).is_err());
    }

    #[test]
    fn spec_rejects_enum_values_failing_format() {
        let spec = Validation {
            format: Some(Format::Integer),
            enum_values: Some(vec![serde_json::Value::String("not_a_number".into())]),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec, &known(&["K"])).is_err());
    }

    #[test]
    fn spec_accepts_valid() {
        let spec = Validation {
            format: Some(Format::Integer),
            range: Some((1.0, 100.0)),
            enum_values: Some(vec![serde_json::Value::Number(serde_json::Number::from(
                42,
            ))]),
            ..Default::default()
        };
        assert!(validate_spec("K", &spec, &known(&["K"])).is_ok());
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
            serde_json::Value::String("dev".into()),
            serde_json::Value::Bool(true),
            serde_json::Value::Number(serde_json::Number::from(42)),
        ];
        let values = resolve_enum_values(&raw).unwrap();
        assert_eq!(values, vec!["dev", "true", "42"]);
    }

    // --- has_cross_field_rules ---

    #[test]
    fn has_cross_field_rules_empty() {
        assert!(!Validation::default().has_cross_field_rules());
    }

    #[test]
    fn has_cross_field_rules_with_required_if() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([("X".into(), "true".into())])),
            ..Default::default()
        };
        assert!(spec.has_cross_field_rules());
    }

    // --- referenced_keys ---

    #[test]
    fn referenced_keys_all_types() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([("A".into(), "*".into())])),
            required_with: Some(vec!["B".into()]),
            required_unless: Some(vec!["C".into()]),
            ..Default::default()
        };
        let keys = spec.referenced_keys();
        assert_eq!(keys, BTreeSet::from(["A", "B", "C"]));
    }

    // --- validate_cross_field ---

    fn secrets(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn required_if_triggered() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([("AUTH_ENABLED".into(), "true".into())])),
            ..Default::default()
        };
        let specs = BTreeMap::from([("AUTH_SECRET", &spec)]);
        let store = secrets(&[("AUTH_ENABLED:dev", "true")]);
        let v = validate_cross_field(&specs, &store, "dev");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].key, "AUTH_SECRET");
        assert!(v[0].message.contains("AUTH_ENABLED = \"true\""));
    }

    #[test]
    fn required_if_not_triggered() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([("AUTH_ENABLED".into(), "true".into())])),
            ..Default::default()
        };
        let specs = BTreeMap::from([("AUTH_SECRET", &spec)]);
        let store = secrets(&[("AUTH_ENABLED:dev", "false")]);
        assert!(validate_cross_field(&specs, &store, "dev").is_empty());
    }

    #[test]
    fn required_if_wildcard() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([("DB_HOST".into(), "*".into())])),
            ..Default::default()
        };
        let specs = BTreeMap::from([("DB_PORT", &spec)]);
        let store = secrets(&[("DB_HOST:dev", "localhost")]);
        let v = validate_cross_field(&specs, &store, "dev");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("DB_HOST is set"));
    }

    #[test]
    fn required_if_wildcard_empty() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([("DB_HOST".into(), "*".into())])),
            ..Default::default()
        };
        let specs = BTreeMap::from([("DB_PORT", &spec)]);
        let store = secrets(&[("DB_HOST:dev", "")]);
        assert!(validate_cross_field(&specs, &store, "dev").is_empty());
    }

    #[test]
    fn required_if_multiple_conditions() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([
                ("AUTH_ENABLED".into(), "true".into()),
                ("AUTH_TYPE".into(), "oauth".into()),
            ])),
            ..Default::default()
        };
        let specs = BTreeMap::from([("OAUTH_SECRET", &spec)]);

        // Both conditions match → violation
        let store = secrets(&[("AUTH_ENABLED:dev", "true"), ("AUTH_TYPE:dev", "oauth")]);
        assert_eq!(validate_cross_field(&specs, &store, "dev").len(), 1);

        // Only one condition → no violation
        let store = secrets(&[("AUTH_ENABLED:dev", "true"), ("AUTH_TYPE:dev", "basic")]);
        assert!(validate_cross_field(&specs, &store, "dev").is_empty());
    }

    #[test]
    fn required_if_satisfied() {
        let spec = Validation {
            required_if: Some(BTreeMap::from([("AUTH_ENABLED".into(), "true".into())])),
            ..Default::default()
        };
        let specs = BTreeMap::from([("AUTH_SECRET", &spec)]);
        let store = secrets(&[("AUTH_ENABLED:dev", "true"), ("AUTH_SECRET:dev", "s3cr3t")]);
        assert!(validate_cross_field(&specs, &store, "dev").is_empty());
    }

    #[test]
    fn required_with_triggered() {
        let spec = Validation {
            required_with: Some(vec!["OAUTH_CLIENT_SECRET".into()]),
            ..Default::default()
        };
        let specs = BTreeMap::from([("OAUTH_CLIENT_ID", &spec)]);
        let store = secrets(&[("OAUTH_CLIENT_SECRET:dev", "secret123")]);
        let v = validate_cross_field(&specs, &store, "dev");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("OAUTH_CLIENT_SECRET is set"));
    }

    #[test]
    fn required_with_neither_set() {
        let spec = Validation {
            required_with: Some(vec!["OAUTH_CLIENT_SECRET".into()]),
            ..Default::default()
        };
        let specs = BTreeMap::from([("OAUTH_CLIENT_ID", &spec)]);
        let store = secrets(&[]);
        assert!(validate_cross_field(&specs, &store, "dev").is_empty());
    }

    #[test]
    fn required_with_both_set() {
        let spec = Validation {
            required_with: Some(vec!["OAUTH_CLIENT_SECRET".into()]),
            ..Default::default()
        };
        let specs = BTreeMap::from([("OAUTH_CLIENT_ID", &spec)]);
        let store = secrets(&[
            ("OAUTH_CLIENT_SECRET:dev", "secret"),
            ("OAUTH_CLIENT_ID:dev", "id123"),
        ]);
        assert!(validate_cross_field(&specs, &store, "dev").is_empty());
    }

    #[test]
    fn required_unless_triggered() {
        let spec = Validation {
            required_unless: Some(vec!["DB_URL".into()]),
            ..Default::default()
        };
        let specs = BTreeMap::from([("DB_HOST", &spec)]);
        let store = secrets(&[]);
        let v = validate_cross_field(&specs, &store, "dev");
        assert_eq!(v.len(), 1);
        assert!(v[0].message.contains("none of DB_URL is set"));
    }

    #[test]
    fn required_unless_alternative_set() {
        let spec = Validation {
            required_unless: Some(vec!["DB_URL".into()]),
            ..Default::default()
        };
        let specs = BTreeMap::from([("DB_HOST", &spec)]);
        let store = secrets(&[("DB_URL:dev", "postgres://localhost/db")]);
        assert!(validate_cross_field(&specs, &store, "dev").is_empty());
    }

    // --- validate_spec cross-field checks ---

    #[test]
    fn spec_rejects_unknown_key() {
        let spec = Validation {
            required_with: Some(vec!["NONEXISTENT".into()]),
            ..Default::default()
        };
        let err = validate_spec("MY_KEY", &spec, &known(&["MY_KEY", "OTHER"])).unwrap_err();
        assert!(err.to_string().contains("unknown key 'NONEXISTENT'"));
    }

    #[test]
    fn spec_rejects_self_reference() {
        let spec = Validation {
            required_with: Some(vec!["MY_KEY".into()]),
            ..Default::default()
        };
        let err = validate_spec("MY_KEY", &spec, &known(&["MY_KEY"])).unwrap_err();
        assert!(err.to_string().contains("references itself"));
    }

    // --- detect_cross_field_cycles ---

    #[test]
    fn cycle_detection_finds_cycle() {
        // required_if creates directed dependencies that can form cycles
        let spec_a = Validation {
            required_if: Some(BTreeMap::from([("B".into(), "*".into())])),
            ..Default::default()
        };
        let spec_b = Validation {
            required_if: Some(BTreeMap::from([("A".into(), "*".into())])),
            ..Default::default()
        };
        let specs: BTreeMap<&str, &Validation> = BTreeMap::from([("A", &spec_a), ("B", &spec_b)]);
        let err = detect_cross_field_cycles(&specs).unwrap_err();
        assert!(err.to_string().contains("circular cross-field reference"));
        assert!(err.to_string().contains("A") && err.to_string().contains("B"));
    }

    #[test]
    fn cycle_detection_no_cycle() {
        let spec_a = Validation {
            required_if: Some(BTreeMap::from([("B".into(), "*".into())])),
            ..Default::default()
        };
        let spec_c = Validation {
            required_unless: Some(vec!["D".into()]),
            ..Default::default()
        };
        let specs: BTreeMap<&str, &Validation> = BTreeMap::from([("A", &spec_a), ("C", &spec_c)]);
        assert!(detect_cross_field_cycles(&specs).is_ok());
    }

    #[test]
    fn cycle_detection_ignores_required_with() {
        // Mutual required_with is the intended pattern, should not be flagged
        let spec_a = Validation {
            required_with: Some(vec!["B".into()]),
            ..Default::default()
        };
        let spec_b = Validation {
            required_with: Some(vec!["A".into()]),
            ..Default::default()
        };
        let specs: BTreeMap<&str, &Validation> = BTreeMap::from([("A", &spec_a), ("B", &spec_b)]);
        assert!(detect_cross_field_cycles(&specs).is_ok());
    }
}
