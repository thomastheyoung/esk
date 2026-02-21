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
                bail!("lockbox.yaml not found (searched from {} upward)", start.display());
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
            bail!("unknown environment '{}' in target '{target}'", resolved.environment);
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
