use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub project: String,
    pub environments: Vec<String>,
    #[serde(default)]
    pub apps: BTreeMap<String, AppConfig>,
    #[serde(default)]
    pub adapters: AdaptersConfig,
    #[serde(default)]
    pub plugins: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub secrets: BTreeMap<String, BTreeMap<String, SecretDef>>,
    /// Root directory containing lockbox.yaml
    #[serde(skip)]
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdaptersConfig {
    #[serde(default)]
    pub env: Option<EnvAdapterConfig>,
    #[serde(default)]
    pub cloudflare: Option<CloudflareAdapterConfig>,
    #[serde(default)]
    pub convex: Option<ConvexAdapterConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvAdapterConfig {
    pub pattern: String,
    #[serde(default)]
    pub env_suffix: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareAdapterConfig {
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvexAdapterConfig {
    pub path: String,
    #[serde(default)]
    pub deployment_source: Option<String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

// --- Plugin config types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnePasswordPluginConfig {
    pub vault: String,
    pub item_pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudFilePluginConfig {
    pub path: String,
    #[serde(default = "default_cloud_file_format")]
    pub format: CloudFileFormat,
}

fn default_cloud_file_format() -> CloudFileFormat {
    CloudFileFormat::Encrypted
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CloudFileFormat {
    Encrypted,
    Cleartext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretDef {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub targets: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedTarget {
    pub adapter: String,
    pub app: Option<String>,
    pub environment: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedSecret {
    pub key: String,
    pub vendor: String,
    #[allow(dead_code)]
    pub description: Option<String>,
    pub targets: Vec<ResolvedTarget>,
}

impl Config {
    /// Walk up from `start` looking for `lockbox.yaml`.
    pub fn find(start: &Path) -> Result<PathBuf> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join("lockbox.yaml");
            if candidate.is_file() {
                return Ok(candidate);
            }
            if !dir.pop() {
                bail!(
                    "lockbox.yaml not found (searched from {} upward)",
                    start.display()
                );
            }
        }
    }

    /// Parse and validate a lockbox.yaml file.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config: Config = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.root = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.environments.is_empty() {
            bail!("at least one environment must be defined");
        }
        self.validate_plugins()?;
        // Validate secret targets reference known adapters, apps, and environments
        // Check for duplicate key names across vendors
        let mut key_vendors: BTreeMap<&str, &str> = BTreeMap::new();
        for (vendor, secrets) in &self.secrets {
            for (key, def) in secrets {
                if let Some(prev_vendor) = key_vendors.get(key.as_str()) {
                    bail!("secret '{key}' is defined in multiple vendors: {prev_vendor}, {vendor}");
                }
                key_vendors.insert(key, vendor);

                for (adapter, targets) in &def.targets {
                    self.validate_adapter(adapter)
                        .with_context(|| format!("secret {key} (vendor: {vendor})"))?;
                    for target_str in targets {
                        self.validate_target_string(adapter, target_str)
                            .with_context(|| format!("secret {key} (vendor: {vendor})"))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_adapter(&self, adapter: &str) -> Result<()> {
        match adapter {
            "env" if self.adapters.env.is_some() => Ok(()),
            "cloudflare" if self.adapters.cloudflare.is_some() => Ok(()),
            "convex" if self.adapters.convex.is_some() => Ok(()),
            "onepassword" => bail!(
                "'onepassword' should be configured under 'plugins:', not 'adapters:'. \
                 Move your onepassword config from adapters to plugins in lockbox.yaml."
            ),
            _ => bail!("adapter '{adapter}' is not configured"),
        }
    }

    fn validate_plugins(&self) -> Result<()> {
        for (name, value) in &self.plugins {
            match name.as_str() {
                "onepassword" => {
                    let _: OnePasswordPluginConfig = serde_yaml::from_value(value.clone())
                        .context("invalid onepassword plugin config")?;
                }
                _ => {
                    // Check for type field to identify cloud_file plugins
                    if let Some(type_val) = value.get("type") {
                        let type_str = type_val
                            .as_str()
                            .context("plugin 'type' must be a string")?;
                        match type_str {
                            "cloud_file" => {
                                let _: CloudFilePluginConfig =
                                    serde_yaml::from_value(value.clone()).with_context(|| {
                                        format!("invalid cloud_file plugin config for '{name}'")
                                    })?;
                            }
                            other => bail!("unknown plugin type '{other}' for '{name}'"),
                        }
                    } else {
                        bail!("unknown plugin '{name}' (missing 'type' field)");
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_target_string(&self, adapter: &str, target: &str) -> Result<()> {
        let resolved = self.parse_target(adapter, target)?;
        if !self.environments.contains(&resolved.environment) {
            bail!(
                "unknown environment '{}' in target '{target}'",
                resolved.environment
            );
        }
        if let Some(app) = &resolved.app {
            if !self.apps.contains_key(app) {
                bail!("unknown app '{app}' in target '{target}'");
            }
        }
        Ok(())
    }

    /// Parse a target string like `"web:dev"` or `"dev"` into a `ResolvedTarget`.
    pub fn parse_target(&self, adapter: &str, target: &str) -> Result<ResolvedTarget> {
        if let Some((app, env)) = target.split_once(':') {
            Ok(ResolvedTarget {
                adapter: adapter.to_string(),
                app: Some(app.to_string()),
                environment: env.to_string(),
            })
        } else {
            Ok(ResolvedTarget {
                adapter: adapter.to_string(),
                app: None,
                environment: target.to_string(),
            })
        }
    }

    /// Resolve all secrets from config into a flat list.
    pub fn resolve_secrets(&self) -> Result<Vec<ResolvedSecret>> {
        let mut result = Vec::new();
        for (vendor, secrets) in &self.secrets {
            for (key, def) in secrets {
                let mut targets = Vec::new();
                for (adapter, target_strs) in &def.targets {
                    for target_str in target_strs {
                        targets.push(self.parse_target(adapter, target_str)?);
                    }
                }
                result.push(ResolvedSecret {
                    key: key.clone(),
                    vendor: vendor.clone(),
                    description: def.description.clone(),
                    targets,
                });
            }
        }
        Ok(result)
    }

    /// Find a secret definition by key. Returns (vendor, &SecretDef).
    pub fn find_secret(&self, key: &str) -> Option<(String, &SecretDef)> {
        for (vendor, secrets) in &self.secrets {
            if let Some(def) = secrets.get(key) {
                return Some((vendor.clone(), def));
            }
        }
        None
    }

    /// Resolve the env file path for an (app, env) pair.
    pub fn resolve_env_path(&self, app: &str, env: &str) -> Result<PathBuf> {
        let env_config = self
            .adapters
            .env
            .as_ref()
            .context("env adapter not configured")?;

        let app_config = self
            .apps
            .get(app)
            .with_context(|| format!("unknown app '{app}'"))?;

        let suffix = env_config.env_suffix.get(env).cloned().unwrap_or_default();

        let path = env_config
            .pattern
            .replace("{app_path}", &app_config.path)
            .replace("{env_suffix}", &suffix);

        Ok(self.root.join(path))
    }

    /// Get the parsed 1Password plugin config, if configured.
    pub fn onepassword_plugin_config(&self) -> Option<OnePasswordPluginConfig> {
        self.plugins
            .get("onepassword")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok())
    }

    /// Get all cloud_file plugin configs: (name, config) pairs.
    pub fn cloud_file_plugin_configs(&self) -> Vec<(String, CloudFilePluginConfig)> {
        self.plugins
            .iter()
            .filter_map(|(name, value)| {
                if name == "onepassword" {
                    return None;
                }
                let type_val = value.get("type")?.as_str()?;
                if type_val != "cloud_file" {
                    return None;
                }
                let cfg: CloudFilePluginConfig = serde_yaml::from_value(value.clone()).ok()?;
                Some((name.clone(), cfg))
            })
            .collect()
    }

    /// Get the set of configured adapter names.
    pub fn adapter_names(&self) -> Vec<&str> {
        let mut names = Vec::new();
        if self.adapters.env.is_some() {
            names.push("env");
        }
        if self.adapters.cloudflare.is_some() {
            names.push("cloudflare");
        }
        if self.adapters.convex.is_some() {
            names.push("convex");
        }
        names
    }
}

impl std::fmt::Display for ResolvedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.adapter)?;
        if let Some(app) = &self.app {
            write!(f, ":{app}")?;
        }
        write!(f, ":{}", self.environment)
    }
}

/// Add a secret key to a config file under the given group, preserving
/// comments and formatting. Uses string-based YAML insertion rather than
/// serde round-tripping to avoid stripping comments.
pub fn add_secret_to_config(config_path: &Path, key: &str, group: &str) -> Result<()> {
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;

    let lines: Vec<&str> = content.lines().collect();

    // Find the `secrets:` top-level line
    let secrets_idx = lines
        .iter()
        .position(|line| *line == "secrets:" || line.starts_with("secrets:"));

    let new_content = match secrets_idx {
        Some(secrets_idx) => {
            // Find the extent of the secrets section (next top-level key or EOF)
            let secrets_end = lines
                .iter()
                .enumerate()
                .skip(secrets_idx + 1)
                .find(|(_, line)| {
                    !line.is_empty()
                        && !line.starts_with(' ')
                        && !line.starts_with('#')
                        && !line.starts_with('\t')
                })
                .map(|(i, _)| i)
                .unwrap_or(lines.len());

            // Look for the group within the secrets section
            let group_line = format!("  {}:", group);
            let group_idx = lines
                .iter()
                .enumerate()
                .skip(secrets_idx + 1)
                .take(secrets_end - secrets_idx - 1)
                .find(|(_, line)| line.trim_end() == group_line.trim_end())
                .map(|(i, _)| i);

            match group_idx {
                Some(group_idx) => {
                    // Check if key already exists in this group
                    let key_line = format!("    {}:", key);
                    let group_end = lines
                        .iter()
                        .enumerate()
                        .skip(group_idx + 1)
                        .find(|(i, line)| {
                            *i >= secrets_end
                                || (!line.is_empty()
                                    && !line.starts_with("    ")
                                    && !line.starts_with('#'))
                        })
                        .map(|(i, _)| i)
                        .unwrap_or(lines.len());

                    let key_exists = lines
                        .iter()
                        .skip(group_idx + 1)
                        .take(group_end - group_idx - 1)
                        .any(|line| {
                            line.starts_with(&key_line)
                                && (line.len() == key_line.len()
                                    || line.as_bytes().get(key_line.len()) == Some(&b' ')
                                    || line.as_bytes().get(key_line.len()) == Some(&b'\n'))
                        });

                    if key_exists {
                        return Ok(()); // Already present, no-op
                    }

                    // Insert after last line of this group
                    let mut parts = Vec::new();
                    for line in &lines[..group_end] {
                        parts.push(line.to_string());
                    }
                    parts.push(format!("    {}: {{}}", key));
                    for line in &lines[group_end..] {
                        parts.push(line.to_string());
                    }
                    let mut out = parts.join("\n");
                    if content.ends_with('\n') {
                        out.push('\n');
                    }
                    out
                }
                None => {
                    // Group doesn't exist — insert at end of secrets section
                    let mut parts = Vec::new();
                    for line in &lines[..secrets_end] {
                        parts.push(line.to_string());
                    }
                    parts.push(format!("  {}:", group));
                    parts.push(format!("    {}: {{}}", key));
                    for line in &lines[secrets_end..] {
                        parts.push(line.to_string());
                    }
                    let mut out = parts.join("\n");
                    if content.ends_with('\n') {
                        out.push('\n');
                    }
                    out
                }
            }
        }
        None => {
            // No secrets section — append at end
            let mut out = content.clone();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("secrets:\n  {}:\n    {}: {{}}\n", group, key));
            out
        }
    };

    // Atomic write: temp file + rename
    let dir = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .context("failed to create temp file for config write")?;
    std::fs::write(tmp.path(), &new_content)
        .context("failed to write temp config file")?;
    tmp.persist(config_path)
        .context("failed to persist config file")?;

    Ok(())
}

/// Return the sorted list of group (vendor) names from config secrets.
pub fn secret_group_names(config: &Config) -> Vec<String> {
    config.secrets.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_yaml(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("lockbox.yaml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn find_in_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lockbox.yaml"),
            "project: x\nenvironments: [dev]",
        )
        .unwrap();
        let found = Config::find(dir.path()).unwrap();
        assert_eq!(found, dir.path().join("lockbox.yaml"));
    }

    #[test]
    fn find_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lockbox.yaml"),
            "project: x\nenvironments: [dev]",
        )
        .unwrap();
        let child = dir.path().join("sub");
        std::fs::create_dir(&child).unwrap();
        let found = Config::find(&child).unwrap();
        assert_eq!(found, dir.path().join("lockbox.yaml"));
    }

    #[test]
    fn find_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::find(dir.path()).unwrap_err();
        assert!(err.to_string().contains("lockbox.yaml not found"));
    }

    #[test]
    fn load_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: testapp\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.project, "testapp");
        assert_eq!(config.environments, vec!["dev"]);
        assert!(config.apps.is_empty());
        assert!(config.secrets.is_empty());
    }

    #[test]
    fn load_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev, prod]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env{env_suffix}"
    env_suffix:
      dev: ""
      prod: ".production"
  cloudflare:
    env_flags:
      prod: "--env production"
  convex:
    path: apps/api
plugins:
  onepassword:
    vault: Eng
    item_pattern: "{project} - {Environment}"
secrets:
  Stripe:
    KEY:
      targets:
        env: [web:dev, web:prod]
        cloudflare: [web:prod]
        convex: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.project, "myapp");
        assert_eq!(config.environments.len(), 2);
        assert!(config.adapters.env.is_some());
        assert!(config.adapters.cloudflare.is_some());
        assert!(config.adapters.convex.is_some());
        assert!(config.onepassword_plugin_config().is_some());
    }

    #[test]
    fn load_sets_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.root, dir.path());
    }

    #[test]
    fn load_nonexistent_file() {
        let err = Config::load(Path::new("/nonexistent/lockbox.yaml")).unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }

    #[test]
    fn load_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "not: [valid: yaml: {{}}");
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validate_empty_environments() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: []");
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("at least one environment"));
    }

    #[test]
    fn validate_unknown_adapter_reference() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
adapters:
  env:
    pattern: "test"
secrets:
  G:
    KEY:
      targets:
        cloudflare: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("adapter 'cloudflare' is not configured"));
    }

    #[test]
    fn validate_adapter_known_but_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
secrets:
  G:
    KEY:
      targets:
        env: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("adapter 'env' is not configured"));
    }

    #[test]
    fn validate_unknown_environment_in_target() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "test"
secrets:
  G:
    KEY:
      targets:
        env: [web:staging]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("unknown environment 'staging'"));
    }

    #[test]
    fn validate_unknown_app_in_target() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "test"
secrets:
  G:
    KEY:
      targets:
        env: [api:dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("unknown app 'api'"));
    }

    #[test]
    fn validate_all_three_adapter_types() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "test"
  cloudflare: {}
  convex:
    path: apps/api
secrets:
  G:
    A:
      targets:
        env: [web:dev]
        cloudflare: [web:dev]
        convex: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        Config::load(&path).unwrap(); // Should not error
    }

    #[test]
    fn validate_onepassword_under_adapters_gives_helpful_error() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
secrets:
  G:
    KEY:
      targets:
        onepassword: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("plugins:"));
    }

    #[test]
    fn validate_plugins_onepassword() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  onepassword:
    vault: V
    item_pattern: test
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let op = config.onepassword_plugin_config().unwrap();
        assert_eq!(op.vault, "V");
        assert_eq!(op.item_pattern, "test");
    }

    #[test]
    fn validate_plugins_cloud_file() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  dropbox:
    type: cloud_file
    path: ~/Dropbox/secrets
    format: encrypted
  gdrive:
    type: cloud_file
    path: "~/Google Drive/secrets"
    format: cleartext
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let cfs = config.cloud_file_plugin_configs();
        assert_eq!(cfs.len(), 2);
    }

    #[test]
    fn validate_plugins_unknown_type_errors() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  foo:
    type: unknown_thing
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("unknown plugin type"));
    }

    #[test]
    fn validate_plugins_unknown_name_no_type_errors() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  foo:
    bar: baz
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("unknown plugin 'foo'"));
    }

    #[test]
    fn parse_target_with_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let target = config.parse_target("env", "web:dev").unwrap();
        assert_eq!(target.adapter, "env");
        assert_eq!(target.app, Some("web".to_string()));
        assert_eq!(target.environment, "dev");
    }

    #[test]
    fn parse_target_without_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let target = config.parse_target("cloudflare", "prod").unwrap();
        assert_eq!(target.adapter, "cloudflare");
        assert_eq!(target.app, None);
        assert_eq!(target.environment, "prod");
    }

    #[test]
    fn parse_target_multiple_colons() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let target = config.parse_target("env", "a:b:c").unwrap();
        assert_eq!(target.app, Some("a".to_string()));
        assert_eq!(target.environment, "b:c"); // split_once on first colon
    }

    #[test]
    fn resolve_secrets_flat_list() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "test"
secrets:
  Stripe:
    KEY_A:
      targets:
        env: [web:dev, web:prod]
    KEY_B:
      targets:
        env: [web:dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let resolved = config.resolve_secrets().unwrap();
        assert_eq!(resolved.len(), 2);
        // KEY_A has 2 targets, KEY_B has 1
        let key_a = resolved.iter().find(|s| s.key == "KEY_A").unwrap();
        assert_eq!(key_a.targets.len(), 2);
        assert_eq!(key_a.vendor, "Stripe");
    }

    #[test]
    fn resolve_secrets_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let resolved = config.resolve_secrets().unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_secrets_multi_vendor() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: w
adapters:
  env:
    pattern: "t"
secrets:
  Stripe:
    SK:
      targets:
        env: [web:dev]
  Convex:
    URL:
      targets:
        env: [web:dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let resolved = config.resolve_secrets().unwrap();
        let vendors: Vec<&str> = resolved.iter().map(|s| s.vendor.as_str()).collect();
        assert!(vendors.contains(&"Stripe"));
        assert!(vendors.contains(&"Convex"));
    }

    #[test]
    fn find_secret_exists() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
adapters:
  env:
    pattern: "t"
apps:
  web:
    path: w
secrets:
  Stripe:
    API_KEY:
      description: test
      targets:
        env: [web:dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let (vendor, def) = config.find_secret("API_KEY").unwrap();
        assert_eq!(vendor, "Stripe");
        assert_eq!(def.description.as_deref(), Some("test"));
    }

    #[test]
    fn find_secret_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        assert!(config.find_secret("NOPE").is_none());
    }

    #[test]
    fn validate_duplicate_key_across_vendors() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: w
adapters:
  env:
    pattern: "t"
secrets:
  Alpha:
    DUP:
      targets:
        env: [web:dev]
  Beta:
    DUP:
      targets:
        env: [web:dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("defined in multiple vendors"));
        assert!(err.to_string().contains("DUP"));
    }

    #[test]
    fn resolve_env_path_with_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      prod: ".production"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let env_path = config.resolve_env_path("web", "prod").unwrap();
        assert_eq!(env_path, dir.path().join("apps/web/.env.production.local"));
    }

    #[test]
    fn resolve_env_path_empty_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let env_path = config.resolve_env_path("web", "dev").unwrap();
        assert_eq!(env_path, dir.path().join("apps/web/.env.local"));
    }

    #[test]
    fn resolve_env_path_no_env_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let err = config.resolve_env_path("web", "dev").unwrap_err();
        assert!(err.to_string().contains("env adapter not configured"));
    }

    #[test]
    fn resolve_env_path_unknown_app() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
adapters:
  env:
    pattern: "test"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let err = config.resolve_env_path("nope", "dev").unwrap_err();
        assert!(err.to_string().contains("unknown app 'nope'"));
    }

    #[test]
    fn adapter_names_returns_configured() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
adapters:
  env:
    pattern: "test"
  cloudflare: {}
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let names = config.adapter_names();
        assert!(names.contains(&"env"));
        assert!(names.contains(&"cloudflare"));
        assert!(!names.contains(&"convex"));
    }

    #[test]
    fn resolved_target_display_with_app() {
        let t = ResolvedTarget {
            adapter: "env".to_string(),
            app: Some("web".to_string()),
            environment: "dev".to_string(),
        };
        assert_eq!(t.to_string(), "env:web:dev");
    }

    #[test]
    fn resolved_target_display_without_app() {
        let t = ResolvedTarget {
            adapter: "cloudflare".to_string(),
            app: None,
            environment: "prod".to_string(),
        };
        assert_eq!(t.to_string(), "cloudflare:prod");
    }

    // --- add_secret_to_config tests ---

    #[test]
    fn add_secret_to_existing_group() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]\nsecrets:\n  Stripe:\n    EXISTING_KEY: {}\n";
        let path = write_yaml(dir.path(), yaml);
        add_secret_to_config(&path, "NEW_KEY", "Stripe").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("    NEW_KEY: {}"));
        // Should still have the existing key
        assert!(content.contains("    EXISTING_KEY: {}"));
        // Verify it parses
        let config = Config::load(&path).unwrap();
        assert!(config.find_secret("NEW_KEY").is_some());
        assert!(config.find_secret("EXISTING_KEY").is_some());
    }

    #[test]
    fn add_secret_to_new_group() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]\nsecrets:\n  Stripe:\n    SK: {}\n";
        let path = write_yaml(dir.path(), yaml);
        add_secret_to_config(&path, "CONVEX_URL", "Convex").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("  Convex:"));
        assert!(content.contains("    CONVEX_URL: {}"));
        // Verify it parses with both groups
        let config = Config::load(&path).unwrap();
        assert!(config.find_secret("SK").is_some());
        assert!(config.find_secret("CONVEX_URL").is_some());
    }

    #[test]
    fn add_secret_no_secrets_section() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]\n";
        let path = write_yaml(dir.path(), yaml);
        add_secret_to_config(&path, "API_KEY", "General").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("secrets:"));
        assert!(content.contains("  General:"));
        assert!(content.contains("    API_KEY: {}"));
        let config = Config::load(&path).unwrap();
        assert!(config.find_secret("API_KEY").is_some());
    }

    #[test]
    fn add_secret_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]\n# My secrets\nsecrets:\n  Stripe:\n    SK: {}\n";
        let path = write_yaml(dir.path(), yaml);
        add_secret_to_config(&path, "NEW", "Stripe").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# My secrets"));
        assert!(content.contains("    NEW: {}"));
    }

    #[test]
    fn add_secret_idempotent_when_key_exists() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]\nsecrets:\n  Stripe:\n    SK: {}\n";
        let path = write_yaml(dir.path(), yaml);
        add_secret_to_config(&path, "SK", "Stripe").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // Should only appear once
        assert_eq!(content.matches("    SK:").count(), 1);
    }
}
