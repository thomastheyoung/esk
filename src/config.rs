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
    #[serde(default)]
    pub onepassword: Option<OnePasswordAdapterConfig>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnePasswordAdapterConfig {
    pub vault: String,
    pub item_pattern: String,
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
        // Validate secret targets reference known adapters, apps, and environments
        for (vendor, secrets) in &self.secrets {
            for (key, def) in secrets {
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
            "onepassword" if self.adapters.onepassword.is_some() => Ok(()),
            _ => bail!("adapter '{adapter}' is not configured"),
        }
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

    /// Get the 1Password item name for an environment.
    pub fn onepassword_item_name(&self, env: &str) -> Result<String> {
        let op_config = self
            .adapters
            .onepassword
            .as_ref()
            .context("onepassword adapter not configured")?;

        // Capitalize first letter of env for pattern
        let env_capitalized = {
            let mut chars = env.chars();
            match chars.next() {
                Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        };

        Ok(op_config
            .item_pattern
            .replace("{project}", &self.project)
            .replace("{Environment}", &env_capitalized)
            .replace("{environment}", env))
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
        onepassword: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.project, "myapp");
        assert_eq!(config.environments.len(), 2);
        assert!(config.adapters.env.is_some());
        assert!(config.adapters.cloudflare.is_some());
        assert!(config.adapters.convex.is_some());
        assert!(config.adapters.onepassword.is_some());
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
    fn validate_all_four_adapter_types() {
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
  onepassword:
    vault: V
    item_pattern: test
secrets:
  G:
    A:
      targets:
        env: [web:dev]
        cloudflare: [web:dev]
        convex: [dev]
        onepassword: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        Config::load(&path).unwrap(); // Should not error
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
    fn find_secret_first_vendor_wins() {
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
        let config = Config::load(&path).unwrap();
        let (vendor, _) = config.find_secret("DUP").unwrap();
        // BTreeMap iteration is alphabetical — Alpha comes first
        assert_eq!(vendor, "Alpha");
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
    fn onepassword_item_name_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
adapters:
  onepassword:
    vault: V
    item_pattern: "{project} - {Environment}"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.onepassword_item_name("dev").unwrap(), "myapp - Dev");
    }

    #[test]
    fn onepassword_item_name_lowercase() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
adapters:
  onepassword:
    vault: V
    item_pattern: "{environment}"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.onepassword_item_name("dev").unwrap(), "dev");
    }

    #[test]
    fn onepassword_item_name_empty_env() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
adapters:
  onepassword:
    vault: V
    item_pattern: "{project} - {Environment}"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let name = config.onepassword_item_name("").unwrap();
        assert_eq!(name, "myapp - ");
    }

    #[test]
    fn onepassword_item_name_no_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let err = config.onepassword_item_name("dev").unwrap_err();
        assert!(err
            .to_string()
            .contains("onepassword adapter not configured"));
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
}
