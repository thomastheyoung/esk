use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;

use anyhow::{bail, Result};
use console::style;

use crate::config::{Config, GenerateFormat, GenerateOutput};
use crate::validate::Format;

struct SecretMeta {
    key: String,
    description: Option<String>,
    format: Option<Format>,
    optional: bool,
    enum_values: Option<Vec<String>>,
    pattern: Option<String>,
    range: Option<(f64, f64)>,
    min_length: Option<usize>,
    max_length: Option<usize>,
}

impl SecretMeta {
    fn from_def(key: String, def: &crate::config::SecretDef) -> Self {
        match &def.validate {
            Some(v) => {
                let enums = v
                    .enum_values
                    .as_ref()
                    .and_then(|raw| crate::validate::resolve_enum_values(raw).ok());
                Self {
                    key,
                    description: def.description.clone(),
                    format: v.format,
                    optional: v.optional,
                    enum_values: enums,
                    pattern: v.pattern.clone(),
                    range: v.range,
                    min_length: v.min_length,
                    max_length: v.max_length,
                }
            }
            None => Self {
                key,
                description: def.description.clone(),
                format: None,
                optional: false,
                enum_values: None,
                pattern: None,
                range: None,
                min_length: None,
                max_length: None,
            },
        }
    }

    fn has_constraints(&self) -> bool {
        self.enum_values.is_some()
            || self.pattern.is_some()
            || self.range.is_some()
            || self.min_length.is_some()
            || self.max_length.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum HelperKind {
    Bool,
    Env,
    Float,
    Int,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RuntimeHelper {
    kind: HelperKind,
    optional: bool,
}

impl RuntimeHelper {
    fn fn_name(self) -> &'static str {
        match (self.kind, self.optional) {
            (HelperKind::Env, false) => "requiredEnv",
            (HelperKind::Int, false) => "requiredInt",
            (HelperKind::Float, false) => "requiredFloat",
            (HelperKind::Bool, false) => "requiredBool",
            (HelperKind::Json, false) => "requiredJson",
            (HelperKind::Env, true) => "optionalEnv",
            (HelperKind::Int, true) => "optionalInt",
            (HelperKind::Float, true) => "optionalFloat",
            (HelperKind::Bool, true) => "optionalBool",
            (HelperKind::Json, true) => "optionalJson",
        }
    }

    fn body(self) -> &'static str {
        match (self.kind, self.optional) {
            (HelperKind::Env, false) => REQUIRED_ENV_BODY,
            (HelperKind::Int, false) => REQUIRED_INT_BODY,
            (HelperKind::Float, false) => REQUIRED_FLOAT_BODY,
            (HelperKind::Bool, false) => REQUIRED_BOOL_BODY,
            (HelperKind::Json, false) => REQUIRED_JSON_BODY,
            (HelperKind::Env, true) => OPTIONAL_ENV_BODY,
            (HelperKind::Int, true) => OPTIONAL_INT_BODY,
            (HelperKind::Float, true) => OPTIONAL_FLOAT_BODY,
            (HelperKind::Bool, true) => OPTIONAL_BOOL_BODY,
            (HelperKind::Json, true) => OPTIONAL_JSON_BODY,
        }
    }
}

// --- Helper body constants (loaded from external JS files) ---

const REQUIRED_ENV_BODY: &str = include_str!("generate_helpers/required_env.js");
const REQUIRED_INT_BODY: &str = include_str!("generate_helpers/required_int.js");
const REQUIRED_FLOAT_BODY: &str = include_str!("generate_helpers/required_float.js");
const REQUIRED_BOOL_BODY: &str = include_str!("generate_helpers/required_bool.js");
const REQUIRED_JSON_BODY: &str = include_str!("generate_helpers/required_json.js");
const OPTIONAL_ENV_BODY: &str = include_str!("generate_helpers/optional_env.js");
const OPTIONAL_INT_BODY: &str = include_str!("generate_helpers/optional_int.js");
const OPTIONAL_FLOAT_BODY: &str = include_str!("generate_helpers/optional_float.js");
const OPTIONAL_BOOL_BODY: &str = include_str!("generate_helpers/optional_bool.js");
const OPTIONAL_JSON_BODY: &str = include_str!("generate_helpers/optional_json.js");

// --- Utility functions ---

fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn format_f64(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        #[allow(clippy::cast_possible_truncation)]
        let i = v as i64;
        format!("{i}")
    } else {
        format!("{v}")
    }
}

// --- Helper dispatch ---

fn determine_helper(m: &SecretMeta) -> RuntimeHelper {
    let kind = match m.format {
        Some(Format::Integer) => HelperKind::Int,
        Some(Format::Number) => HelperKind::Float,
        Some(Format::Boolean) => HelperKind::Bool,
        Some(Format::Json) => HelperKind::Json,
        _ => HelperKind::Env,
    };
    RuntimeHelper {
        kind,
        optional: m.optional,
    }
}

// --- Opts building ---

fn build_string_opts(m: &SecretMeta) -> Option<String> {
    let mut fields = Vec::new();
    if let Some(ref values) = m.enum_values {
        let items: Vec<String> = values
            .iter()
            .map(|v| format!("\"{}\"", escape_js_string(v)))
            .collect();
        fields.push(format!("allowed: [{}]", items.join(", ")));
    }
    if let Some(ref pattern) = m.pattern {
        fields.push(format!(
            "pattern: new RegExp(\"{}\")",
            escape_js_string(pattern)
        ));
    }
    if let Some(min) = m.min_length {
        fields.push(format!("minLength: {min}"));
    }
    if let Some(max) = m.max_length {
        fields.push(format!("maxLength: {max}"));
    }
    if fields.is_empty() {
        None
    } else {
        Some(format!("{{ {} }}", fields.join(", ")))
    }
}

fn build_int_opts(m: &SecretMeta) -> Option<String> {
    let mut fields = Vec::new();
    if let Some(ref values) = m.enum_values {
        let items: Vec<String> = values.iter().map(ToString::to_string).collect();
        fields.push(format!("allowed: [{}]", items.join(", ")));
    }
    if let Some((min, max)) = m.range {
        fields.push(format!("min: {}", format_f64(min)));
        fields.push(format!("max: {}", format_f64(max)));
    }
    if fields.is_empty() {
        None
    } else {
        Some(format!("{{ {} }}", fields.join(", ")))
    }
}

fn build_float_opts(m: &SecretMeta) -> Option<String> {
    let mut fields = Vec::new();
    if let Some((min, max)) = m.range {
        fields.push(format!("min: {}", format_f64(min)));
        fields.push(format!("max: {}", format_f64(max)));
    }
    if fields.is_empty() {
        None
    } else {
        Some(format!("{{ {} }}", fields.join(", ")))
    }
}

fn build_opts(helper: RuntimeHelper, m: &SecretMeta) -> Option<String> {
    match helper.kind {
        HelperKind::Env => build_string_opts(m),
        HelperKind::Int => build_int_opts(m),
        HelperKind::Float => build_float_opts(m),
        HelperKind::Bool | HelperKind::Json => None,
    }
}

// --- Report / run / resolve_outputs (unchanged logic) ---

struct GenerateResult {
    relative_path: String,
    secret_count: usize,
    gitignore_warning: bool,
}

struct GenerateReport {
    results: Vec<GenerateResult>,
}

impl GenerateReport {
    fn render(&self) -> Result<()> {
        if self.results.is_empty() {
            cliclack::log::warning("No secrets defined in config")?;
            return Ok(());
        }

        for result in &self.results {
            cliclack::log::success(format!(
                "Wrote {} secrets to {}",
                result.secret_count, result.relative_path
            ))?;

            if result.gitignore_warning {
                cliclack::log::info(format!(
                    "Consider adding {} to .gitignore",
                    result.relative_path
                ))?;
            }
        }

        Ok(())
    }
}

pub fn run(
    config: &Config,
    format: Option<&GenerateFormat>,
    output: Option<&str>,
    preview: bool,
) -> Result<()> {
    let metas = collect_secret_metas(config);

    if metas.is_empty() {
        if preview {
            return Ok(());
        }
        let report = GenerateReport {
            results: Vec::new(),
        };
        return report.render();
    }

    let outputs = resolve_outputs(format, output, &config.generate)?;

    if !preview {
        cliclack::intro(
            style(format!(
                "{} · {} output{}",
                style(&config.project).bold(),
                outputs.len(),
                if outputs.len() == 1 { "" } else { "s" },
            ))
            .to_string(),
        )?;
    }

    if preview {
        for (i, entry) in outputs.iter().enumerate() {
            if i > 0 {
                println!();
            }
            if outputs.len() > 1 {
                let name = entry
                    .output
                    .as_deref()
                    .unwrap_or(entry.format.default_output());
                println!("── {name} ──");
            }
            let content = render_content(&metas, entry.format);
            print!("{content}");
        }
        return Ok(());
    }

    let mut results = Vec::new();
    for entry in &outputs {
        results.push(generate_one(config, &metas, entry)?);
    }

    let report = GenerateReport { results };
    report.render()?;

    let file_count = report.results.len();
    let secret_count = report.results.first().map_or(0, |r| r.secret_count);
    cliclack::outro(
        style(format!(
            "Generated {} file{} from {} secret{}",
            file_count,
            if file_count == 1 { "" } else { "s" },
            secret_count,
            if secret_count == 1 { "" } else { "s" },
        ))
        .dim()
        .to_string(),
    )?;
    Ok(())
}

fn resolve_outputs(
    format: Option<&GenerateFormat>,
    output: Option<&str>,
    config_generate: &[GenerateOutput],
) -> Result<Vec<GenerateOutput>> {
    match (format, output) {
        (Some(f), out) => Ok(vec![GenerateOutput {
            format: *f,
            output: out.map(String::from),
        }]),
        (None, Some(_)) => {
            bail!("--output requires a format argument (e.g. `esk generate dts --output path`)");
        }
        (None, None) if !config_generate.is_empty() => Ok(config_generate.to_vec()),
        (None, None) => {
            bail!(
                "No format specified and no generate outputs configured in esk.yaml\n\n\
                 Usage: esk generate <FORMAT> [--output <path>]\n\n\
                 Available formats:\n  \
                   dts          TypeScript declaration file (env.d.ts)\n  \
                   ts           Runtime TypeScript module (env.ts)\n  \
                   ts-lazy      Lazy runtime TypeScript module (env.ts)\n  \
                   zod          Zod schema with runtime parsing (env.ts)\n  \
                   env-example  Example .env file (.env.example)\n\n\
                 Or configure outputs in esk.yaml:\n  \
                   generate:\n    \
                   - format: dts\n    \
                   - format: env-example\n      \
                   output: config/.env.example"
            );
        }
    }
}

const GENERATED_HEADER: &str = "// Generated by esk";

fn is_esk_generated(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|c| c.starts_with(GENERATED_HEADER))
        .unwrap_or(false)
}

fn render_content(metas: &[SecretMeta], format: GenerateFormat) -> String {
    match format {
        GenerateFormat::Dts => generate_dts(metas),
        GenerateFormat::Ts => generate_runtime(metas),
        GenerateFormat::TsLazy => generate_runtime_lazy(metas),
        GenerateFormat::Zod => generate_zod(metas),
        GenerateFormat::EnvExample => generate_env_example(metas),
    }
}

fn generate_one(
    config: &Config,
    metas: &[SecretMeta],
    entry: &GenerateOutput,
) -> Result<GenerateResult> {
    let default_name = entry.format.default_output();
    let out_path = match &entry.output {
        Some(p) => config.root.join(p),
        None => config.root.join(default_name),
    };

    if out_path.exists() && !is_esk_generated(&out_path) {
        let relative = out_path.strip_prefix(&config.root).unwrap_or(&out_path);
        bail!(
            "{} already exists and was not generated by esk.\n\
             Use --output to write to a different path:\n\n  \
             esk generate {} --output esk-{}",
            relative.display(),
            entry.format.cli_name(),
            default_name,
        );
    }

    let content = render_content(metas, entry.format);

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out_path, content)?;

    let relative = out_path.strip_prefix(&config.root).unwrap_or(&out_path);
    let gitignore_warning =
        entry.format.should_warn_gitignore() && !is_gitignored(&config.root, relative);

    Ok(GenerateResult {
        relative_path: relative.display().to_string(),
        secret_count: metas.len(),
        gitignore_warning,
    })
}

fn collect_secret_metas(config: &Config) -> Vec<SecretMeta> {
    let mut seen = BTreeSet::new();
    let mut metas = Vec::new();
    for group in config.secrets.values() {
        for (key, def) in group {
            if seen.insert(key.as_str()) {
                metas.push(SecretMeta::from_def(key.clone(), def));
            }
        }
    }
    metas.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    metas
}

fn generate_dts(metas: &[SecretMeta]) -> String {
    let mut out = String::from("// Generated by esk — do not edit\n");
    out.push_str("declare namespace NodeJS {\n");
    out.push_str("  interface ProcessEnv {\n");
    for m in metas {
        if let Some(ref values) = m.enum_values {
            let union = values
                .iter()
                .map(|v| format!("\"{}\"", escape_js_string(v)))
                .collect::<Vec<_>>()
                .join(" | ");
            if m.optional {
                let _ = writeln!(out, "    {}?: {} | undefined;", m.key, union);
            } else {
                let _ = writeln!(out, "    {}: {};", m.key, union);
            }
        } else if m.optional {
            let _ = writeln!(out, "    {}?: string | undefined;", m.key);
        } else {
            let _ = writeln!(out, "    {}: string;", m.key);
        }
    }
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

#[derive(Clone, Copy)]
enum RuntimeMode {
    Eager,
    Lazy,
}

fn generate_runtime(metas: &[SecretMeta]) -> String {
    generate_runtime_inner(metas, RuntimeMode::Eager)
}

fn generate_runtime_lazy(metas: &[SecretMeta]) -> String {
    generate_runtime_inner(metas, RuntimeMode::Lazy)
}

fn generate_runtime_inner(metas: &[SecretMeta], mode: RuntimeMode) -> String {
    let mut out = String::from("// Generated by esk — do not edit\n");

    // Determine which helpers are needed
    let mut needed: BTreeSet<RuntimeHelper> = BTreeSet::new();
    for m in metas {
        if m.optional && determine_helper(m).kind == HelperKind::Env && !m.has_constraints() {
            continue; // bare process.env.X — no helper needed
        }
        needed.insert(determine_helper(m));
    }

    // Emit only the helpers that are used
    emit_runtime_helpers(&mut out, &needed);

    out.push_str("export const env = {\n");
    for m in metas {
        emit_runtime_property(&mut out, m, mode);
    }
    match mode {
        RuntimeMode::Eager => out.push_str("} as const;\n"),
        RuntimeMode::Lazy => out.push_str("};\n"),
    }
    out
}

fn emit_runtime_helpers(out: &mut String, needed: &BTreeSet<RuntimeHelper>) {
    for helper in needed {
        out.push_str(helper.body());
        out.push('\n');
    }
}

fn format_helper_call(helper: RuntimeHelper, m: &SecretMeta) -> String {
    let name = helper.fn_name();
    match build_opts(helper, m) {
        Some(opts) => format!("{name}(\"{}\", {opts})", m.key),
        None => format!("{name}(\"{}\")", m.key),
    }
}

fn emit_runtime_property(out: &mut String, m: &SecretMeta, mode: RuntimeMode) {
    let call = if m.optional && determine_helper(m).kind == HelperKind::Env && !m.has_constraints()
    {
        format!("process.env.{}", m.key)
    } else {
        let helper = determine_helper(m);
        format_helper_call(helper, m)
    };

    match mode {
        RuntimeMode::Eager => {
            let _ = writeln!(out, "  {}: {call},", m.key);
        }
        RuntimeMode::Lazy => {
            let _ = writeln!(out, "  get {}() {{ return {call}; }},", m.key);
        }
    }
}

fn generate_zod(metas: &[SecretMeta]) -> String {
    let mut out = String::from("// Generated by esk — do not edit\nimport { z } from \"zod\";\n\nconst envSchema = z.object({\n");

    for m in metas {
        let _ = write!(out, "  {}: ", m.key);
        let mut chain = zod_base_type(m);

        // Append constraints (only for string-based types, not enums which replace the base)
        if m.enum_values.is_none() {
            if let Some(ref pattern) = m.pattern {
                let _ = write!(
                    chain,
                    ".regex(new RegExp(\"{}\"))",
                    escape_js_string(pattern)
                );
            }
            if let Some(min) = m.min_length {
                let _ = write!(chain, ".min({min})");
            }
            if let Some(max) = m.max_length {
                let _ = write!(chain, ".max({max})");
            }
            if let Some((min, max)) = m.range {
                let _ = write!(chain, ".min({}).max({})", format_f64(min), format_f64(max));
            }
        }

        if m.optional {
            chain.push_str(".optional()");
        }

        let _ = writeln!(out, "{chain},");
    }

    out.push_str("});\n\nexport const env = envSchema.parse(process.env);\n");
    out
}

fn zod_base_type(m: &SecretMeta) -> String {
    // Enum replaces base type entirely
    if let Some(ref values) = m.enum_values {
        let items: Vec<String> = values
            .iter()
            .map(|v| format!("\"{}\"", escape_js_string(v)))
            .collect();
        return format!("z.enum([{}])", items.join(", "));
    }

    match m.format {
        Some(Format::Url) => "z.string().url()".to_string(),
        Some(Format::Email) => "z.string().email()".to_string(),
        Some(Format::Integer) => "z.coerce.number().int()".to_string(),
        Some(Format::Number) => "z.coerce.number()".to_string(),
        Some(Format::Boolean) => {
            "z.string().transform(v => [\"true\", \"1\", \"yes\"].includes(v.toLowerCase()))"
                .to_string()
        }
        Some(Format::Json) => "z.string().transform(v => JSON.parse(v) as unknown)".to_string(),
        Some(Format::Base64) => "z.string().base64()".to_string(),
        Some(Format::String) | None => "z.string()".to_string(),
    }
}

fn generate_env_example(metas: &[SecretMeta]) -> String {
    let mut out = String::from("# Generated by esk — do not edit\n");

    for (i, m) in metas.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if let Some(ref desc) = m.description {
            for line in desc.lines() {
                let _ = writeln!(out, "# {line}");
            }
        }
        if let Some(ref values) = m.enum_values {
            let _ = writeln!(out, "# Allowed: {}", values.join(", "));
        }
        if m.optional {
            out.push_str("# Optional\n");
            let _ = writeln!(out, "# {}=", m.key);
        } else {
            let _ = writeln!(out, "{}=", m.key);
        }
    }

    out
}

fn is_gitignored(root: &Path, relative: &Path) -> bool {
    let gitignore_path = root.join(".gitignore");
    let Ok(content) = std::fs::read_to_string(&gitignore_path) else {
        return false;
    };
    let filename = relative.file_name().unwrap_or_default().to_string_lossy();
    let full_path = relative.to_string_lossy();
    content.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && (pattern_matches(trimmed, &filename) || pattern_matches(trimmed, &full_path))
    })
}

fn pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        path.ends_with(suffix)
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        path.starts_with(prefix)
    } else {
        pattern == path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Required, TargetsConfig};
    use std::collections::BTreeMap;

    fn meta(key: &str) -> SecretMeta {
        SecretMeta {
            key: key.to_string(),
            description: None,
            format: None,
            optional: false,
            enum_values: None,
            pattern: None,
            range: None,
            min_length: None,
            max_length: None,
        }
    }

    fn collect_keys(config: &Config) -> BTreeSet<String> {
        collect_secret_metas(config)
            .into_iter()
            .map(|m| m.key)
            .collect()
    }

    fn config_with_secrets(
        secrets: BTreeMap<String, BTreeMap<String, crate::config::SecretDef>>,
    ) -> Config {
        Config {
            project: "test".to_string(),
            environments: vec!["dev".to_string(), "prod".to_string()],
            apps: BTreeMap::new(),
            targets: TargetsConfig::default(),
            remotes: BTreeMap::new(),
            secrets,
            generate: Vec::new(),
            root: std::path::PathBuf::from("/tmp"),
            typed_remotes: Vec::new(),
            typed_targets: Vec::new(),
        }
    }

    fn secret_def() -> crate::config::SecretDef {
        crate::config::SecretDef {
            description: None,
            targets: BTreeMap::new(),
            validate: None,
            required: Required::default(),
            allow_empty: false,
        }
    }

    fn secret_def_with_desc(desc: &str) -> crate::config::SecretDef {
        crate::config::SecretDef {
            description: Some(desc.to_string()),
            targets: BTreeMap::new(),
            validate: None,
            required: Required::default(),
            allow_empty: false,
        }
    }

    fn secret_def_with_validate(v: crate::validate::Validation) -> crate::config::SecretDef {
        crate::config::SecretDef {
            description: None,
            targets: BTreeMap::new(),
            validate: Some(v),
            required: Required::default(),
            allow_empty: false,
        }
    }

    #[test]
    fn collect_keys_unions_across_groups() {
        let mut secrets = BTreeMap::new();
        let mut group_a = BTreeMap::new();
        group_a.insert("ALPHA".to_string(), secret_def());
        group_a.insert("SHARED".to_string(), secret_def());
        secrets.insert("A".to_string(), group_a);

        let mut group_b = BTreeMap::new();
        group_b.insert("BETA".to_string(), secret_def());
        group_b.insert("SHARED".to_string(), secret_def());
        secrets.insert("B".to_string(), group_b);

        let config = config_with_secrets(secrets);
        let keys = collect_keys(&config);

        assert_eq!(keys.len(), 3);
        assert!(keys.contains("ALPHA"));
        assert!(keys.contains("BETA"));
        assert!(keys.contains("SHARED"));
    }

    #[test]
    fn collect_keys_empty_config() {
        let config = config_with_secrets(BTreeMap::new());
        assert!(collect_keys(&config).is_empty());
    }

    #[test]
    fn dts_output_format() {
        let metas = vec![meta("A_KEY"), meta("B_KEY")];

        let output = generate_dts(&metas);
        assert!(output.starts_with("// Generated by esk"));
        assert!(output.contains("declare namespace NodeJS"));
        assert!(output.contains("interface ProcessEnv"));
        assert!(output.contains("A_KEY: string;"));
        assert!(output.contains("B_KEY: string;"));
    }

    #[test]
    fn runtime_output_format() {
        let metas = vec![meta("DB_URL")];

        let output = generate_runtime(&metas);
        assert!(output.starts_with("// Generated by esk"));
        assert!(output.contains("function requiredEnv"));
        assert!(output.contains("export const env ="));
        assert!(output.contains("DB_URL: requiredEnv(\"DB_URL\")"));
        assert!(output.contains("as const;"));
    }

    #[test]
    fn keys_are_sorted() {
        let metas = vec![meta("ZEBRA"), meta("ALPHA"), meta("MIDDLE")];

        // collect_secret_metas sorts, but generate_dts takes pre-sorted slice.
        // Test via dts output order:
        let mut sorted = metas;
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        let output = generate_dts(&sorted);
        let alpha_pos = output.find("ALPHA").unwrap();
        let middle_pos = output.find("MIDDLE").unwrap();
        let zebra_pos = output.find("ZEBRA").unwrap();
        assert!(alpha_pos < middle_pos);
        assert!(middle_pos < zebra_pos);
    }

    #[test]
    fn gitignore_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "env.d.ts\n").unwrap();
        assert!(is_gitignored(dir.path(), Path::new("env.d.ts")));
        assert!(!is_gitignored(dir.path(), Path::new("other.ts")));
    }

    #[test]
    fn gitignore_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_gitignored(dir.path(), Path::new("env.d.ts")));
    }

    #[test]
    fn gitignore_glob_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.d.ts\n").unwrap();
        assert!(is_gitignored(dir.path(), Path::new("env.d.ts")));
        assert!(!is_gitignored(dir.path(), Path::new("env.ts")));
    }

    #[test]
    fn gitignore_matches_filename_in_subpath() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "env.d.ts\n").unwrap();
        assert!(is_gitignored(dir.path(), Path::new("types/env.d.ts")));
    }

    #[test]
    fn gitignore_comments_and_blanks_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "# comment\n\nenv.d.ts\n").unwrap();
        assert!(is_gitignored(dir.path(), Path::new("env.d.ts")));
        assert!(!is_gitignored(dir.path(), Path::new("# comment")));
    }

    #[test]
    fn generate_dts_enum_union_type() {
        let metas = vec![SecretMeta {
            enum_values: Some(vec![
                "development".to_string(),
                "staging".to_string(),
                "production".to_string(),
            ]),
            ..meta("NODE_ENV")
        }];
        let output = generate_dts(&metas);
        assert!(output.contains("NODE_ENV: \"development\" | \"staging\" | \"production\";"));
    }

    #[test]
    fn generate_dts_optional_field() {
        let metas = vec![SecretMeta {
            format: Some(Format::String),
            optional: true,
            ..meta("FEATURE_FLAG")
        }];
        let output = generate_dts(&metas);
        assert!(output.contains("FEATURE_FLAG?: string | undefined;"));
    }

    #[test]
    fn generate_runtime_emits_typed_helpers() {
        let metas = vec![
            SecretMeta {
                format: Some(Format::Integer),
                ..meta("PORT")
            },
            SecretMeta {
                format: Some(Format::Url),
                ..meta("URL")
            },
        ];
        let output = generate_runtime(&metas);
        assert!(output.contains("function requiredInt("));
        assert!(output.contains("PORT: requiredInt(\"PORT\")"));
        assert!(output.contains("function requiredEnv("));
        assert!(output.contains("URL: requiredEnv(\"URL\")"));
    }

    #[test]
    fn generate_runtime_omits_unused_helpers() {
        let metas = vec![SecretMeta {
            format: Some(Format::Integer),
            ..meta("PORT")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains("function requiredInt("));
        assert!(!output.contains("function requiredEnv("));
        assert!(!output.contains("function requiredBool("));
        assert!(!output.contains("function requiredFloat("));
        assert!(!output.contains("function requiredJson("));
    }

    #[test]
    fn generate_runtime_optional_string_uses_process_env() {
        let metas = vec![SecretMeta {
            format: Some(Format::String),
            optional: true,
            ..meta("FEATURE")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains("FEATURE: process.env.FEATURE"));
        assert!(!output.contains("function"));
    }

    #[test]
    fn generate_runtime_optional_bool_uses_helper() {
        let metas = vec![SecretMeta {
            format: Some(Format::Boolean),
            optional: true,
            ..meta("FEATURE")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"FEATURE: optionalBool("FEATURE")"#));
        assert!(output.contains("function optionalBool("));
    }

    #[test]
    fn collect_secret_metas_extracts_validation() {
        let mut secrets = BTreeMap::new();
        let mut group = BTreeMap::new();
        group.insert(
            "PORT".to_string(),
            secret_def_with_validate(crate::validate::Validation {
                format: Some(Format::Integer),
                ..Default::default()
            }),
        );
        group.insert("PLAIN".to_string(), secret_def());
        secrets.insert("General".to_string(), group);

        let config = config_with_secrets(secrets);
        let metas = collect_secret_metas(&config);

        let plain = metas.iter().find(|m| m.key == "PLAIN").unwrap();
        assert!(plain.format.is_none());

        let port = metas.iter().find(|m| m.key == "PORT").unwrap();
        assert_eq!(port.format, Some(Format::Integer));
    }

    // --- env-example tests ---

    #[test]
    fn env_example_basic() {
        let metas = vec![meta("A_KEY"), meta("B_KEY")];
        let output = generate_env_example(&metas);
        assert!(output.starts_with("# Generated by esk"));
        assert!(output.contains("A_KEY=\n"));
        assert!(output.contains("B_KEY=\n"));
    }

    #[test]
    fn env_example_with_descriptions() {
        let mut secrets = BTreeMap::new();
        let mut group = BTreeMap::new();
        group.insert(
            "STRIPE_KEY".to_string(),
            secret_def_with_desc("Your Stripe API key"),
        );
        secrets.insert("General".to_string(), group);

        let config = config_with_secrets(secrets);
        let metas = collect_secret_metas(&config);
        let output = generate_env_example(&metas);
        assert!(output.contains("# Your Stripe API key\n"));
        assert!(output.contains("STRIPE_KEY=\n"));
    }

    #[test]
    fn env_example_with_enums() {
        let metas = vec![SecretMeta {
            enum_values: Some(vec![
                "development".to_string(),
                "staging".to_string(),
                "production".to_string(),
            ]),
            ..meta("NODE_ENV")
        }];
        let output = generate_env_example(&metas);
        assert!(output.contains("# Allowed: development, staging, production\n"));
        assert!(output.contains("NODE_ENV=\n"));
    }

    #[test]
    fn env_example_optional_commented() {
        let metas = vec![SecretMeta {
            optional: true,
            ..meta("FEATURE_FLAG")
        }];
        let output = generate_env_example(&metas);
        assert!(output.contains("# Optional\n"));
        assert!(output.contains("# FEATURE_FLAG=\n"));
        // Only one occurrence of FEATURE_FLAG=, and it's commented
        assert_eq!(output.matches("FEATURE_FLAG=").count(), 1);
        assert!(output.contains("# FEATURE_FLAG="));
    }

    // --- resolve_outputs tests ---

    #[test]
    fn resolve_outputs_explicit_format() {
        let result = resolve_outputs(Some(&GenerateFormat::Ts), None, &[]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].format, GenerateFormat::Ts);
        assert!(result[0].output.is_none());
    }

    #[test]
    fn resolve_outputs_explicit_format_with_output() {
        let result =
            resolve_outputs(Some(&GenerateFormat::Dts), Some("types/env.d.ts"), &[]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].format, GenerateFormat::Dts);
        assert_eq!(result[0].output.as_deref(), Some("types/env.d.ts"));
    }

    #[test]
    fn resolve_outputs_config_driven() {
        let config_entries = vec![
            GenerateOutput {
                format: GenerateFormat::Dts,
                output: None,
            },
            GenerateOutput {
                format: GenerateFormat::EnvExample,
                output: None,
            },
        ];
        let result = resolve_outputs(None, None, &config_entries).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].format, GenerateFormat::Dts);
        assert_eq!(result[1].format, GenerateFormat::EnvExample);
    }

    #[test]
    fn resolve_outputs_no_format_no_config_errors() {
        let err = resolve_outputs(None, None, &[]).unwrap_err();
        assert!(err.to_string().contains("No format specified"));
        assert!(err.to_string().contains("Available formats"));
    }

    #[test]
    fn resolve_outputs_output_without_format_errors() {
        let err = resolve_outputs(None, Some("out.ts"), &[]).unwrap_err();
        assert!(err.to_string().contains("--output requires a format"));
    }

    // --- collect_secret_metas extracts description ---

    #[test]
    fn collect_secret_metas_extracts_description() {
        let mut secrets = BTreeMap::new();
        let mut group = BTreeMap::new();
        group.insert("MY_KEY".to_string(), secret_def_with_desc("A description"));
        group.insert("NO_DESC".to_string(), secret_def());
        secrets.insert("General".to_string(), group);

        let config = config_with_secrets(secrets);
        let metas = collect_secret_metas(&config);

        let my_key = metas.iter().find(|m| m.key == "MY_KEY").unwrap();
        assert_eq!(my_key.description.as_deref(), Some("A description"));

        let no_desc = metas.iter().find(|m| m.key == "NO_DESC").unwrap();
        assert!(no_desc.description.is_none());
    }

    // --- should_warn_gitignore ---

    #[test]
    fn should_warn_gitignore_true_for_dts() {
        assert!(GenerateFormat::Dts.should_warn_gitignore());
    }

    #[test]
    fn should_warn_gitignore_true_for_ts() {
        assert!(GenerateFormat::Ts.should_warn_gitignore());
    }

    #[test]
    fn should_warn_gitignore_true_for_ts_lazy() {
        assert!(GenerateFormat::TsLazy.should_warn_gitignore());
    }

    #[test]
    fn should_warn_gitignore_false_for_env_example() {
        assert!(!GenerateFormat::EnvExample.should_warn_gitignore());
    }

    // --- existing file conflict detection ---

    #[test]
    fn is_esk_generated_true() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.d.ts");
        std::fs::write(&path, "// Generated by esk — do not edit\nstuff").unwrap();
        assert!(is_esk_generated(&path));
    }

    #[test]
    fn is_esk_generated_false_for_user_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.d.ts");
        std::fs::write(
            &path,
            "declare namespace NodeJS {\n  interface ProcessEnv {}\n}",
        )
        .unwrap();
        assert!(!is_esk_generated(&path));
    }

    #[test]
    fn is_esk_generated_false_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_esk_generated(&dir.path().join("nope.ts")));
    }

    // --- env-example multi-line description ---

    #[test]
    fn env_example_multiline_description() {
        let metas = vec![SecretMeta {
            description: Some("Connection string\nfor the database".to_string()),
            ..meta("DB_URL")
        }];
        let output = generate_env_example(&metas);
        assert!(output.contains("# Connection string\n# for the database\n"));
    }

    // --- ts-lazy tests ---

    #[test]
    fn lazy_runtime_uses_getters() {
        let metas = vec![meta("DB_URL")];

        let output = generate_runtime_lazy(&metas);
        assert!(output.starts_with("// Generated by esk"));
        assert!(output.contains("function requiredEnv"));
        assert!(output.contains("export const env ="));
        assert!(output.contains("get DB_URL() { return requiredEnv(\"DB_URL\"); }"));
        // Lazy mode must NOT use `as const` — TS doesn't allow it on getter objects
        assert!(!output.contains("as const"));
        assert!(output.contains("};\n"));
    }

    #[test]
    fn lazy_runtime_typed_helpers() {
        let metas = vec![
            SecretMeta {
                format: Some(Format::Integer),
                ..meta("PORT")
            },
            SecretMeta {
                format: Some(Format::Number),
                ..meta("RATE")
            },
            SecretMeta {
                format: Some(Format::Boolean),
                ..meta("ENABLED")
            },
            SecretMeta {
                format: Some(Format::Json),
                ..meta("META")
            },
        ];
        let output = generate_runtime_lazy(&metas);
        assert!(output.contains("get PORT() { return requiredInt(\"PORT\"); }"));
        assert!(output.contains("get RATE() { return requiredFloat(\"RATE\"); }"));
        assert!(output.contains("get ENABLED() { return requiredBool(\"ENABLED\"); }"));
        assert!(output.contains("get META() { return requiredJson(\"META\"); }"));
    }

    #[test]
    fn lazy_runtime_optional_string_uses_process_env() {
        let metas = vec![SecretMeta {
            format: Some(Format::String),
            optional: true,
            ..meta("FEATURE")
        }];
        let output = generate_runtime_lazy(&metas);
        assert!(output.contains("get FEATURE() { return process.env.FEATURE; }"));
        assert!(!output.contains("function"));
    }

    #[test]
    fn lazy_runtime_optional_bool_uses_helper() {
        let metas = vec![SecretMeta {
            format: Some(Format::Boolean),
            optional: true,
            ..meta("FEATURE")
        }];
        let output = generate_runtime_lazy(&metas);
        assert!(output.contains(r#"get FEATURE() { return optionalBool("FEATURE"); }"#));
        assert!(output.contains("function optionalBool("));
    }

    #[test]
    fn lazy_runtime_omits_unused_helpers() {
        let metas = vec![SecretMeta {
            format: Some(Format::Integer),
            ..meta("PORT")
        }];
        let output = generate_runtime_lazy(&metas);
        assert!(output.contains("function requiredInt("));
        assert!(!output.contains("function requiredEnv("));
        assert!(!output.contains("function requiredBool("));
        assert!(!output.contains("function requiredFloat("));
        assert!(!output.contains("function requiredJson("));
    }

    // --- validation constraint tests ---

    #[test]
    fn runtime_enum_in_opts() {
        let metas = vec![SecretMeta {
            enum_values: Some(vec!["a".to_string(), "b".to_string()]),
            ..meta("KEY")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"KEY: requiredEnv("KEY", { allowed: ["a", "b"] })"#));
    }

    #[test]
    fn runtime_int_enum_in_opts() {
        let metas = vec![SecretMeta {
            format: Some(Format::Integer),
            enum_values: Some(vec!["80".to_string(), "443".to_string()]),
            ..meta("PORT")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"PORT: requiredInt("PORT", { allowed: [80, 443] })"#));
    }

    #[test]
    fn runtime_pattern_in_opts() {
        let metas = vec![SecretMeta {
            pattern: Some("^sk_".to_string()),
            ..meta("KEY")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"KEY: requiredEnv("KEY", { pattern: new RegExp("^sk_") })"#));
    }

    #[test]
    fn runtime_length_in_opts() {
        let metas = vec![SecretMeta {
            min_length: Some(5),
            max_length: Some(20),
            ..meta("KEY")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"KEY: requiredEnv("KEY", { minLength: 5, maxLength: 20 })"#));
    }

    #[test]
    fn runtime_range_on_int() {
        let metas = vec![SecretMeta {
            format: Some(Format::Integer),
            range: Some((1.0, 65535.0)),
            ..meta("PORT")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"PORT: requiredInt("PORT", { min: 1, max: 65535 })"#));
    }

    #[test]
    fn runtime_range_on_float() {
        let metas = vec![SecretMeta {
            format: Some(Format::Number),
            range: Some((0.1, 100.0)),
            ..meta("RATE")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"RATE: requiredFloat("RATE", { min: 0.1, max: 100 })"#));
    }

    #[test]
    fn runtime_combined_constraints() {
        let metas = vec![SecretMeta {
            enum_values: Some(vec!["a".to_string(), "b".to_string()]),
            pattern: Some("^[ab]$".to_string()),
            min_length: Some(1),
            max_length: Some(1),
            ..meta("KEY")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(
            r#"KEY: requiredEnv("KEY", { allowed: ["a", "b"], pattern: new RegExp("^[ab]$"), minLength: 1, maxLength: 1 })"#
        ));
    }

    #[test]
    fn runtime_optional_with_constraints() {
        let metas = vec![SecretMeta {
            optional: true,
            enum_values: Some(vec!["x".to_string(), "y".to_string()]),
            ..meta("KEY")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"KEY: optionalEnv("KEY", { allowed: ["x", "y"] })"#));
        assert!(output.contains("function optionalEnv("));
    }

    #[test]
    fn runtime_optional_no_constraints_bare() {
        let metas = vec![SecretMeta {
            optional: true,
            ..meta("KEY")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains("KEY: process.env.KEY"));
        assert!(!output.contains("function"));
    }

    #[test]
    fn runtime_regex_escaping() {
        let metas = vec![SecretMeta {
            pattern: Some(r#"^sk_[a-z"\\]+$"#.to_string()),
            ..meta("KEY")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"pattern: new RegExp("^sk_[a-z\"\\\\]+$")"#));
    }

    #[test]
    fn runtime_f64_whole_number() {
        assert_eq!(format_f64(1.0), "1");
        assert_eq!(format_f64(65535.0), "65535");
        assert_eq!(format_f64(-42.0), "-42");
    }

    #[test]
    fn runtime_f64_fractional() {
        assert_eq!(format_f64(0.5), "0.5");
        assert_eq!(format_f64(99.9), "99.9");
    }

    #[test]
    fn lazy_enum_in_opts() {
        let metas = vec![SecretMeta {
            enum_values: Some(vec!["a".to_string(), "b".to_string()]),
            ..meta("KEY")
        }];
        let output = generate_runtime_lazy(&metas);
        assert!(
            output.contains(r#"get KEY() { return requiredEnv("KEY", { allowed: ["a", "b"] }); }"#)
        );
    }

    #[test]
    fn determine_helper_all_combinations() {
        // required variants
        let cases = vec![
            (None, false, HelperKind::Env, false),
            (Some(Format::String), false, HelperKind::Env, false),
            (Some(Format::Url), false, HelperKind::Env, false),
            (Some(Format::Email), false, HelperKind::Env, false),
            (Some(Format::Base64), false, HelperKind::Env, false),
            (Some(Format::Integer), false, HelperKind::Int, false),
            (Some(Format::Number), false, HelperKind::Float, false),
            (Some(Format::Boolean), false, HelperKind::Bool, false),
            (Some(Format::Json), false, HelperKind::Json, false),
            // optional variants
            (None, true, HelperKind::Env, true),
            (Some(Format::Integer), true, HelperKind::Int, true),
            (Some(Format::Number), true, HelperKind::Float, true),
            (Some(Format::Boolean), true, HelperKind::Bool, true),
            (Some(Format::Json), true, HelperKind::Json, true),
        ];
        for (format, optional, expected_kind, expected_optional) in cases {
            let m = SecretMeta {
                format,
                optional,
                ..meta("X")
            };
            let helper = determine_helper(&m);
            assert_eq!(
                helper.kind, expected_kind,
                "format={format:?} optional={optional}"
            );
            assert_eq!(
                helper.optional, expected_optional,
                "format={format:?} optional={optional}"
            );
        }
    }

    #[test]
    fn escape_js_string_basics() {
        assert_eq!(escape_js_string(r#"hello"world"#), r#"hello\"world"#);
        assert_eq!(escape_js_string("line\nnew"), "line\\nnew");
        assert_eq!(escape_js_string(r"back\slash"), "back\\\\slash");
    }

    // --- zod tests ---

    #[test]
    fn zod_basic() {
        let metas = vec![meta("DB_URL")];
        let output = generate_zod(&metas);
        assert!(output.starts_with("// Generated by esk"));
        assert!(output.contains("import { z } from \"zod\";"));
        assert!(output.contains("const envSchema = z.object({"));
        assert!(output.contains("DB_URL: z.string(),"));
        assert!(output.contains("export const env = envSchema.parse(process.env);"));
    }

    #[test]
    fn zod_all_formats() {
        let metas = vec![
            SecretMeta {
                format: Some(Format::String),
                ..meta("A")
            },
            SecretMeta {
                format: Some(Format::Url),
                ..meta("B")
            },
            SecretMeta {
                format: Some(Format::Email),
                ..meta("C")
            },
            SecretMeta {
                format: Some(Format::Integer),
                ..meta("D")
            },
            SecretMeta {
                format: Some(Format::Number),
                ..meta("E")
            },
            SecretMeta {
                format: Some(Format::Boolean),
                ..meta("F")
            },
            SecretMeta {
                format: Some(Format::Json),
                ..meta("G")
            },
            SecretMeta {
                format: Some(Format::Base64),
                ..meta("H")
            },
            meta("I"), // no format
        ];
        let output = generate_zod(&metas);
        assert!(output.contains("A: z.string(),"));
        assert!(output.contains("B: z.string().url(),"));
        assert!(output.contains("C: z.string().email(),"));
        assert!(output.contains("D: z.coerce.number().int(),"));
        assert!(output.contains("E: z.coerce.number(),"));
        assert!(output.contains(
            r#"F: z.string().transform(v => ["true", "1", "yes"].includes(v.toLowerCase())),"#
        ));
        assert!(output.contains("G: z.string().transform(v => JSON.parse(v) as unknown),"));
        assert!(output.contains("H: z.string().base64(),"));
        assert!(output.contains("I: z.string(),"));
    }

    #[test]
    fn zod_constraints() {
        let metas = vec![
            SecretMeta {
                enum_values: Some(vec![
                    "debug".to_string(),
                    "info".to_string(),
                    "warn".to_string(),
                ]),
                ..meta("LOG_LEVEL")
            },
            SecretMeta {
                pattern: Some("^sk_[a-zA-Z0-9]+$".to_string()),
                min_length: Some(10),
                max_length: Some(100),
                ..meta("API_KEY")
            },
            SecretMeta {
                format: Some(Format::Integer),
                range: Some((1.0, 65535.0)),
                ..meta("PORT")
            },
        ];
        let output = generate_zod(&metas);
        assert!(output.contains(r#"LOG_LEVEL: z.enum(["debug", "info", "warn"]),"#));
        assert!(output.contains(
            r#"API_KEY: z.string().regex(new RegExp("^sk_[a-zA-Z0-9]+$")).min(10).max(100),"#
        ));
        assert!(output.contains("PORT: z.coerce.number().int().min(1).max(65535),"));
    }

    #[test]
    fn zod_optional() {
        let metas = vec![SecretMeta {
            format: Some(Format::Url),
            optional: true,
            ..meta("CALLBACK")
        }];
        let output = generate_zod(&metas);
        assert!(output.contains("CALLBACK: z.string().url().optional(),"));
    }

    #[test]
    fn zod_optional_without_constraints() {
        let metas = vec![SecretMeta {
            optional: true,
            ..meta("FEATURE")
        }];
        let output = generate_zod(&metas);
        assert!(output.contains("FEATURE: z.string().optional(),"));
    }

    #[test]
    fn zod_enum_replaces_base_type() {
        let metas = vec![SecretMeta {
            format: Some(Format::String),
            enum_values: Some(vec!["a".to_string(), "b".to_string()]),
            ..meta("MODE")
        }];
        let output = generate_zod(&metas);
        // enum should produce z.enum([...]) not z.string()
        assert!(output.contains(r#"MODE: z.enum(["a", "b"]),"#));
        assert!(!output.contains("z.string()"));
    }

    // --- Step 2: should_warn_gitignore for zod ---

    #[test]
    fn should_warn_gitignore_true_for_zod() {
        assert!(GenerateFormat::Zod.should_warn_gitignore());
    }

    // --- Step 4: DTS enum escaping ---

    #[test]
    fn generate_dts_enum_escapes_special_chars() {
        let metas = vec![SecretMeta {
            enum_values: Some(vec![
                r#"say "hello""#.to_string(),
                r"back\slash".to_string(),
            ]),
            ..meta("ESCAPED")
        }];
        let output = generate_dts(&metas);
        assert!(output.contains(r#"ESCAPED: "say \"hello\"" | "back\\slash";"#));
    }

    // --- Step 5: Zod regex injection ---

    #[test]
    fn zod_regex_with_slash() {
        let metas = vec![SecretMeta {
            pattern: Some("^https?://.*$".to_string()),
            ..meta("URL")
        }];
        let output = generate_zod(&metas);
        assert!(output.contains(r#"URL: z.string().regex(new RegExp("^https?://.*$")),"#));
    }

    // --- Step 8: Optional typed secrets get helpers ---

    #[test]
    fn runtime_optional_int_uses_helper() {
        let metas = vec![SecretMeta {
            format: Some(Format::Integer),
            optional: true,
            ..meta("PORT")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"PORT: optionalInt("PORT")"#));
        assert!(output.contains("function optionalInt("));
    }

    #[test]
    fn runtime_optional_json_uses_helper() {
        let metas = vec![SecretMeta {
            format: Some(Format::Json),
            optional: true,
            ..meta("META")
        }];
        let output = generate_runtime(&metas);
        assert!(output.contains(r#"META: optionalJson("META")"#));
        assert!(output.contains("function optionalJson<T = unknown>("));
    }

    #[test]
    fn needs_runtime_helper_formats() {
        // Formats that need typed helpers (not HelperKind::Env)
        for fmt in [
            Format::Integer,
            Format::Number,
            Format::Boolean,
            Format::Json,
        ] {
            let m = SecretMeta {
                format: Some(fmt),
                optional: true,
                ..meta("X")
            };
            assert_ne!(
                determine_helper(&m).kind,
                HelperKind::Env,
                "format {fmt:?} should need a typed helper"
            );
        }
        // Formats that map to Env (no special helper needed)
        for fmt in [Format::String, Format::Url, Format::Email, Format::Base64] {
            let m = SecretMeta {
                format: Some(fmt),
                optional: true,
                ..meta("X")
            };
            assert_eq!(
                determine_helper(&m).kind,
                HelperKind::Env,
                "format {fmt:?} should use Env helper"
            );
        }
    }
}
