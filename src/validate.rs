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
            Self::String => write!(f, "string"),
            Self::Url => write!(f, "url"),
            Self::Integer => write!(f, "integer"),
            Self::Number => write!(f, "number"),
            Self::Boolean => write!(f, "boolean"),
            Self::Email => write!(f, "email"),
            Self::Json => write!(f, "json"),
            Self::Base64 => write!(f, "base64"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub const fn has_cross_field_rules(&self) -> bool {
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

/// Stable category for a validation failure.
///
/// These codes intentionally describe constraints, never candidate or
/// configured secret values. They are safe to persist and expose through the
/// CLI and MCP responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    Empty,
    Format,
    Enum,
    Pattern,
    MinLength,
    MaxLength,
    Range,
    RequiredIf,
    RequiredWith,
    RequiredUnless,
}

impl ValidationCode {
    const fn message(self) -> &'static str {
        match self {
            Self::Empty => "value must not be empty",
            Self::Format => "value does not match the required format",
            Self::Enum => "value is not an allowed option",
            Self::Pattern => "value does not satisfy the configured pattern",
            Self::MinLength => "value is shorter than the configured minimum length",
            Self::MaxLength => "value exceeds the configured maximum length",
            Self::Range => "value is outside the configured range",
            Self::RequiredIf => "value is required because configured conditions are met",
            Self::RequiredWith => "value is required because a related secret is set",
            Self::RequiredUnless => "value is required because no alternative secret is set",
        }
    }
}

/// Non-sensitive details about the failed constraint.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValidationConstraint {
    Format { format: Format },
    Enum { allowed_count: usize },
    Pattern,
    MinLength { minimum: usize },
    MaxLength { maximum: usize },
    Range,
}

/// A value-free, serializable validation failure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationIssue {
    key: String,
    code: ValidationCode,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    constraint: Option<ValidationConstraint>,
}

impl ValidationIssue {
    fn new(key: &str, code: ValidationCode, constraint: Option<ValidationConstraint>) -> Self {
        Self {
            key: key.to_string(),
            code,
            message: code.message().to_string(),
            constraint,
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub const fn code(&self) -> ValidationCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn constraint(&self) -> Option<&ValidationConstraint> {
        self.constraint.as_ref()
    }
}

/// A cross-field validation violation found during deploy or status checks.
#[derive(Debug, Clone, Serialize)]
pub struct CrossFieldViolation {
    key: String,
    env: String,
    code: ValidationCode,
    references: Vec<String>,
    message: String,
}

impl CrossFieldViolation {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    pub const fn code(&self) -> ValidationCode {
        self.code
    }

    pub fn references(&self) -> &[String] {
        &self.references
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Coerce a list of values (bool, number, string) to strings.
pub fn resolve_enum_values(raw: &[serde_json::Value]) -> Result<Vec<String>> {
    let mut result = Vec::with_capacity(raw.len());
    for v in raw {
        match v {
            serde_json::Value::Bool(b) => result.push(b.to_string()),
            serde_json::Value::Number(n) => result.push(n.to_string()),
            serde_json::Value::String(s) => result.push(s.clone()),
            _ => bail!("unsupported enum value type (expected string, number, or boolean)"),
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
        if !min.is_finite() || !max.is_finite() {
            bail!("secret '{key}': range values must be finite numbers");
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
            bail!("secret '{key}': invalid regex pattern");
        }
    }

    // enum values must pass format check if format is set
    if let Some(ref raw_values) = spec.enum_values {
        let values = resolve_enum_values(raw_values)?;
        if let Some(format) = spec.format {
            for v in &values {
                if validate_format(v, format).is_err() {
                    bail!("secret '{key}': an enum value does not match format '{format}'");
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

/// Validate a single value against a spec without retaining the candidate value.
pub fn validate_value(key: &str, value: &str, spec: &Validation) -> Result<(), ValidationError> {
    // Empty value handling
    if value.is_empty() {
        if spec.optional {
            return Ok(());
        }
        return Err(ValidationError::new(vec![ValidationIssue::new(
            key,
            ValidationCode::Empty,
            None,
        )]));
    }

    let mut violations = Vec::new();

    // Format check
    if let Some(format) = spec.format {
        if validate_format(value, format).is_err() {
            violations.push(ValidationIssue::new(
                key,
                ValidationCode::Format,
                Some(ValidationConstraint::Format { format }),
            ));
        }
    }

    // Enum check
    if let Some(ref raw_values) = spec.enum_values {
        // resolve_enum_values should not fail at this point (validated at spec time)
        if let Ok(allowed) = resolve_enum_values(raw_values) {
            if !allowed.iter().any(|v| v == value) {
                violations.push(ValidationIssue::new(
                    key,
                    ValidationCode::Enum,
                    Some(ValidationConstraint::Enum {
                        allowed_count: allowed.len(),
                    }),
                ));
            }
        }
    }

    // Pattern check
    if let Some(ref pat) = spec.pattern {
        if let Ok(re) = regex_lite::Regex::new(pat) {
            if !re.is_match(value) {
                violations.push(ValidationIssue::new(
                    key,
                    ValidationCode::Pattern,
                    Some(ValidationConstraint::Pattern),
                ));
            }
        }
    }

    // Length checks (character count, not byte count)
    if let Some(min) = spec.min_length {
        let len = value.chars().count();
        if len < min {
            violations.push(ValidationIssue::new(
                key,
                ValidationCode::MinLength,
                Some(ValidationConstraint::MinLength { minimum: min }),
            ));
        }
    }
    if let Some(max) = spec.max_length {
        let len = value.chars().count();
        if len > max {
            violations.push(ValidationIssue::new(
                key,
                ValidationCode::MaxLength,
                Some(ValidationConstraint::MaxLength { maximum: max }),
            ));
        }
    }

    // Range check (only meaningful for numeric formats)
    if let Some((min, max)) = spec.range {
        if let Ok(n) = value.parse::<f64>() {
            if n < min || n > max {
                violations.push(ValidationIssue::new(
                    key,
                    ValidationCode::Range,
                    Some(ValidationConstraint::Range),
                ));
            }
        }
        // If not parseable as number, the format check already caught it
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(ValidationError::new(violations))
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
                violations.push(CrossFieldViolation {
                    key: key.to_string(),
                    env: env.to_string(),
                    code: ValidationCode::RequiredIf,
                    references: conditions.keys().cloned().collect(),
                    message: ValidationCode::RequiredIf.message().to_string(),
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
                            code: ValidationCode::RequiredWith,
                            references: vec![peer.clone()],
                            message: ValidationCode::RequiredWith.message().to_string(),
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
                    violations.push(CrossFieldViolation {
                        key: key.to_string(),
                        env: env.to_string(),
                        code: ValidationCode::RequiredUnless,
                        references: alternatives.clone(),
                        message: ValidationCode::RequiredUnless.message().to_string(),
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

fn validate_format(value: &str, format: Format) -> Result<(), ()> {
    match format {
        Format::String => {
            // Any non-empty string is valid (emptiness checked earlier)
            Ok(())
        }
        Format::Url => {
            let scheme = value.split("://").next().unwrap_or("");
            if scheme.is_empty() || scheme == value {
                return Err(());
            }
            let after_scheme = value.split("://").nth(1).unwrap_or("");
            if after_scheme.is_empty() || after_scheme == "/" {
                return Err(());
            }
            Ok(())
        }
        Format::Integer => {
            if value.parse::<i64>().is_err() {
                return Err(());
            }
            Ok(())
        }
        Format::Number => {
            if value.parse::<f64>().is_err() {
                return Err(());
            }
            Ok(())
        }
        Format::Boolean => {
            let lower = value.to_lowercase();
            if !["true", "false", "1", "0", "yes", "no"].contains(&lower.as_str()) {
                return Err(());
            }
            Ok(())
        }
        Format::Email => {
            let parts: Vec<&str> = value.splitn(2, '@').collect();
            if parts.len() != 2 || parts[0].is_empty() || !parts[1].contains('.') {
                return Err(());
            }
            Ok(())
        }
        Format::Json => {
            if serde_json::from_str::<serde_json::Value>(value).is_err() {
                return Err(());
            }
            Ok(())
        }
        Format::Base64 => {
            use base64::Engine;
            if base64::engine::general_purpose::STANDARD
                .decode(value)
                .is_err()
            {
                return Err(());
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
pub struct ValidationError {
    key: String,
    violations: Vec<ValidationIssue>,
}

impl ValidationError {
    fn new(violations: Vec<ValidationIssue>) -> Self {
        debug_assert!(!violations.is_empty());
        let key = violations
            .first()
            .map(|issue| issue.key().to_string())
            .unwrap_or_default();
        Self { key, violations }
    }

    pub fn message(&self) -> String {
        self.violations
            .iter()
            .map(ValidationIssue::message)
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn violations(&self) -> &[ValidationIssue] {
        &self.violations
    }

    pub fn into_violations(self) -> Vec<ValidationIssue> {
        self.violations
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
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
        assert_eq!(err.violations()[0].code(), ValidationCode::Enum);
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
        assert_eq!(err.violations()[0].code(), ValidationCode::Pattern);
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
        assert_eq!(err.violations()[0].code(), ValidationCode::Range);
        let err = validate_value("K", "99999", &spec).unwrap_err();
        assert_eq!(err.violations()[0].code(), ValidationCode::Range);
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
        assert_eq!(err.violations()[0].code(), ValidationCode::MinLength);
    }

    #[test]
    fn length_too_long() {
        let spec = Validation {
            max_length: Some(3),
            ..Default::default()
        };
        let err = validate_value("K", "abcde", &spec).unwrap_err();
        assert_eq!(err.violations()[0].code(), ValidationCode::MaxLength);
    }

    #[test]
    fn issues_never_disclose_candidate_or_constraint_values() {
        let candidate = "candidate-sentinel";
        let allowed = "allowed-sentinel";
        let pattern = "^pattern-sentinel$";
        let spec = Validation {
            enum_values: Some(vec![serde_json::Value::String(allowed.to_string())]),
            pattern: Some(pattern.to_string()),
            ..Default::default()
        };

        let err = validate_value("TOKEN", candidate, &spec).unwrap_err();
        let rendered = serde_json::to_string(err.violations()).unwrap();
        for secret_material in [candidate, allowed, pattern] {
            assert!(!rendered.contains(secret_material), "{rendered}");
        }
        assert_eq!(err.violations()[0].code(), ValidationCode::Enum);
        assert_eq!(err.violations()[1].code(), ValidationCode::Pattern);
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

    #[test]
    fn unsupported_enum_types_do_not_disclose_config_values() {
        let sentinel = "enum-sentinel";
        let spec = Validation {
            enum_values: Some(vec![serde_json::json!([sentinel])]),
            ..Default::default()
        };
        let err = validate_spec("TOKEN", &spec, &known(&["TOKEN"])).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unsupported enum value type"), "{message}");
        assert!(!message.contains(sentinel), "{message}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(
            &path,
            format!(
                "project: demo\nenvironments: [dev]\nsecrets:\n  App:\n    TOKEN:\n      validate:\n        enum:\n          - [{sentinel}]\n"
            ),
        )
        .unwrap();
        let err = crate::config::Config::load(&path).unwrap_err();
        let error_chain = format!("{err:#}");
        assert!(
            error_chain.contains("unsupported enum value type"),
            "{error_chain}"
        );
        assert!(!error_chain.contains(sentinel), "{error_chain}");
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
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
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
        assert_eq!(v[0].key(), "AUTH_SECRET");
        assert_eq!(v[0].code(), ValidationCode::RequiredIf);
        assert_eq!(v[0].references(), ["AUTH_ENABLED"]);
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
    fn cross_field_issues_never_disclose_predicate_values() {
        let predicate = "predicate-sentinel";
        let spec = Validation {
            required_if: Some(BTreeMap::from([("SWITCH".into(), predicate.into())])),
            ..Default::default()
        };
        let specs = BTreeMap::from([("REQUIRED", &spec)]);
        let store = secrets(&[("SWITCH:dev", predicate)]);

        let violations = validate_cross_field(&specs, &store, "dev");
        let rendered = serde_json::to_string(&violations).unwrap();
        assert!(!rendered.contains(predicate), "{rendered}");
        assert_eq!(violations[0].references(), ["SWITCH"]);
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
        assert_eq!(v[0].code(), ValidationCode::RequiredIf);
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
        assert_eq!(v[0].code(), ValidationCode::RequiredWith);
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
        assert_eq!(v[0].code(), ValidationCode::RequiredUnless);
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
        assert!(err.to_string().contains('A') && err.to_string().contains('B'));
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
    fn spec_rejects_non_finite_range_nan() {
        let spec = Validation {
            format: Some(Format::Number),
            range: Some((f64::NAN, 10.0)),
            ..Default::default()
        };
        let err = validate_spec("K", &spec, &known(&["K"])).unwrap_err();
        assert!(err.to_string().contains("finite"));
    }

    #[test]
    fn spec_rejects_non_finite_range_inf() {
        let spec = Validation {
            format: Some(Format::Integer),
            range: Some((0.0, f64::INFINITY)),
            ..Default::default()
        };
        let err = validate_spec("K", &spec, &known(&["K"])).unwrap_err();
        assert!(err.to_string().contains("finite"));
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
