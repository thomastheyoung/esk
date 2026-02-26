use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::store::{validate_app, validate_environment, validate_key, validate_project};
use crate::suggest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub project: String,
    pub environments: Vec<String>,
    #[serde(default)]
    pub apps: BTreeMap<String, AppConfig>,
    #[serde(default)]
    pub targets: TargetsConfig,
    #[serde(default)]
    pub remotes: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub secrets: BTreeMap<String, BTreeMap<String, SecretDef>>,
    /// Root directory containing esk.yaml
    #[serde(skip)]
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetsConfig {
    #[serde(default)]
    pub env: Option<EnvTargetConfig>,
    #[serde(default)]
    pub cloudflare: Option<CloudflareTargetConfig>,
    #[serde(default)]
    pub convex: Option<ConvexTargetConfig>,
    #[serde(default)]
    pub fly: Option<FlyTargetConfig>,
    #[serde(default)]
    pub netlify: Option<NetlifyTargetConfig>,
    #[serde(default)]
    pub vercel: Option<VercelTargetConfig>,
    #[serde(default)]
    pub github: Option<GithubTargetConfig>,
    #[serde(default)]
    pub heroku: Option<HerokuTargetConfig>,
    #[serde(default)]
    pub supabase: Option<SupabaseTargetConfig>,
    #[serde(default)]
    pub railway: Option<RailwayTargetConfig>,
    #[serde(default)]
    pub gitlab: Option<GitlabTargetConfig>,
    // Phase 2: Cloud infrastructure
    #[serde(default)]
    pub aws_ssm: Option<AwsSsmTargetConfig>,
    // Phase 4: Full cloud coverage
    #[serde(default)]
    pub kubernetes: Option<KubernetesTargetConfig>,
    #[serde(default)]
    pub docker: Option<DockerTargetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvTargetConfig {
    pub pattern: String,
    #[serde(default)]
    pub env_suffix: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareTargetConfig {
    /// Mode: "workers" (default) or "pages".
    #[serde(default = "default_cloudflare_mode")]
    pub mode: String,
    /// Pages project name (required when mode is "pages").
    #[serde(default)]
    pub pages_project: Option<String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

fn default_cloudflare_mode() -> String {
    "workers".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvexTargetConfig {
    pub path: String,
    #[serde(default)]
    pub deployment_source: Option<String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlyTargetConfig {
    /// Maps esk app name → Fly app name (e.g. web → my-fly-app).
    pub app_names: BTreeMap<String, String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetlifyTargetConfig {
    /// Optional Netlify site ID or name.
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VercelTargetConfig {
    /// Maps esk env name → Vercel env name (e.g. prod → production).
    pub env_names: BTreeMap<String, String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubTargetConfig {
    /// Optional GitHub repo in owner/repo format.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HerokuTargetConfig {
    /// Maps esk app name → Heroku app name.
    pub app_names: BTreeMap<String, String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupabaseTargetConfig {
    /// Supabase project reference ID.
    pub project_ref: String,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RailwayTargetConfig {
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitlabTargetConfig {
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwsSsmTargetConfig {
    /// Path prefix with interpolation, e.g. "/{project}/{environment}/".
    pub path_prefix: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    /// Parameter type: SecureString (default), String, or StringList.
    #[serde(default = "default_ssm_parameter_type")]
    pub parameter_type: String,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

fn default_ssm_parameter_type() -> String {
    "SecureString".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KubernetesTargetConfig {
    /// Maps esk env → Kubernetes namespace.
    pub namespace: BTreeMap<String, String>,
    /// Secret resource name (default: "{project}-secrets").
    #[serde(default)]
    pub secret_name: Option<String>,
    /// Maps esk env → kubectl context.
    #[serde(default)]
    pub context: BTreeMap<String, String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerTargetConfig {
    /// Name pattern for Docker secrets with `{project}`, `{environment}`, `{key}` placeholders.
    #[serde(default = "default_docker_name_pattern")]
    pub name_pattern: String,
    /// Static labels applied to all created secrets.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub env_flags: BTreeMap<String, String>,
}

fn default_docker_name_pattern() -> String {
    "{project}-{environment}-{key}".to_string()
}

// --- Remote config types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnePasswordRemoteConfig {
    pub vault: String,
    pub item_pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudFileRemoteConfig {
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
pub struct AwsSecretsManagerRemoteConfig {
    /// Secret name pattern, e.g. "{project}/{environment}".
    pub secret_name: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitwardenRemoteConfig {
    pub project_id: String,
    /// Secret name pattern, e.g. "{project}-{environment}".
    pub secret_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashicorpVaultRemoteConfig {
    /// KV path pattern, e.g. "secret/data/{project}/{environment}".
    pub path: String,
    #[serde(default)]
    pub addr: Option<String>,
    #[serde(default = "default_kv_version")]
    pub kv_version: u8,
}

fn default_kv_version() -> u8 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3RemoteConfig {
    pub bucket: String,
    #[serde(default)]
    pub prefix: Option<String>,
    /// Custom endpoint for S3-compatible services (R2, MinIO, DO Spaces).
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default = "default_cloud_file_format")]
    pub format: CloudFileFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcpSecretManagerRemoteConfig {
    pub gcp_project: String,
    /// Secret name pattern, e.g. "{project}-{environment}".
    pub secret_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AzureKeyVaultRemoteConfig {
    pub vault_name: String,
    /// Secret name pattern, e.g. "{project}-{environment}".
    pub secret_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DopplerRemoteConfig {
    pub project: String,
    /// Maps esk env → Doppler config name.
    pub config_map: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SopsRemoteConfig {
    /// File path pattern with {environment} interpolation.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretDef {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub targets: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub validate: Option<crate::validate::Validation>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedTarget {
    pub service: String,
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
    pub validation: Option<crate::validate::Validation>,
}

/// Check whether `target` stays within `root` after normalizing `..` components.
/// Does not require paths to exist on disk.
fn is_within_root(root: &Path, target: &Path) -> bool {
    let mut normalized = PathBuf::new();
    for component in target.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            c => normalized.push(c),
        }
    }
    normalized.starts_with(root)
}

/// Return the closest existing ancestor (including `path` itself).
fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    let mut cur = path;
    loop {
        if cur.exists() {
            return Some(cur);
        }
        cur = cur.parent()?;
    }
}

impl Config {
    /// Walk up from `start` looking for `esk.yaml`.
    pub fn find(start: &Path) -> Result<PathBuf> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join("esk.yaml");
            if candidate.is_file() {
                return Ok(candidate);
            }
            if !dir.pop() {
                bail!(
                    "esk.yaml not found (searched from {} upward)",
                    start.display()
                );
            }
        }
    }

    /// Parse and validate a esk.yaml file.
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
        validate_project(&self.project)?;
        for env in &self.environments {
            validate_environment(env)?;
        }
        for app_name in self.apps.keys() {
            validate_app(app_name)?;
        }
        // Validate env target pattern and env_suffix for unsafe path characters
        if let Some(env_config) = &self.targets.env {
            if env_config.pattern.contains("..") {
                bail!("env target pattern must not contain '..'");
            }
            for (env, suffix) in &env_config.env_suffix {
                if suffix.contains("..")
                    || suffix.contains('/')
                    || suffix.contains('\\')
                    || suffix.contains('\0')
                {
                    bail!("env_suffix for '{env}' contains unsafe path characters");
                }
            }
        }
        self.validate_remotes()?;
        // Validate secret targets reference known targets, apps, and environments
        // Check for duplicate key names across vendors
        let mut key_vendors: BTreeMap<&str, &str> = BTreeMap::new();
        for (vendor, secrets) in &self.secrets {
            for (key, def) in secrets {
                validate_key(key)
                    .with_context(|| format!("secret '{key}' in vendor '{vendor}'"))?;
                if let Some(prev_vendor) = key_vendors.get(key.as_str()) {
                    bail!("secret '{key}' is defined in multiple vendors: {prev_vendor}, {vendor}");
                }
                key_vendors.insert(key, vendor);

                if let Some(ref spec) = def.validate {
                    crate::validate::validate_spec(key, spec)
                        .with_context(|| format!("secret {key} (vendor: {vendor})"))?;
                }

                for (service, targets) in &def.targets {
                    self.validate_service(service)
                        .with_context(|| format!("secret {key} (vendor: {vendor})"))?;
                    for target_str in targets {
                        self.validate_target_string(service, target_str)
                            .with_context(|| format!("secret {key} (vendor: {vendor})"))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_service(&self, service: &str) -> Result<()> {
        if service == "1password" {
            bail!(
                "'1password' should be configured under 'remotes:', not 'targets:'. \
                 Move your 1password config from targets to remotes in esk.yaml."
            );
        }
        let names = self.target_names();
        if names.contains(&service) {
            Ok(())
        } else {
            bail!("{}", suggest::unknown_target(service, &names))
        }
    }

    fn validate_remotes(&self) -> Result<()> {
        for (name, value) in &self.remotes {
            match name.as_str() {
                "1password" => {
                    let _: OnePasswordRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid 1password remote config")?;
                }
                "aws_secrets_manager" => {
                    let _: AwsSecretsManagerRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid aws_secrets_manager remote config")?;
                }
                "bitwarden" => {
                    let _: BitwardenRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid bitwarden remote config")?;
                }
                "vault" => {
                    let _: HashicorpVaultRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid vault remote config")?;
                }
                "s3" => {
                    let _: S3RemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid s3 remote config")?;
                }
                "gcp" => {
                    let _: GcpSecretManagerRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid gcp remote config")?;
                }
                "azure" => {
                    let _: AzureKeyVaultRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid azure remote config")?;
                }
                "doppler" => {
                    let _: DopplerRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid doppler remote config")?;
                }
                "sops" => {
                    let _: SopsRemoteConfig = serde_yaml::from_value(value.clone())
                        .context("invalid sops remote config")?;
                }
                _ => {
                    // Check for type field to identify cloud_file remotes
                    if let Some(type_val) = value.get("type") {
                        let type_str = type_val
                            .as_str()
                            .context("remote 'type' must be a string")?;
                        match type_str {
                            "cloud_file" => {
                                let _: CloudFileRemoteConfig =
                                    serde_yaml::from_value(value.clone()).with_context(|| {
                                        format!("invalid cloud_file remote config for '{name}'")
                                    })?;
                            }
                            other => bail!("unknown remote type '{other}' for '{name}'"),
                        }
                    } else {
                        bail!("unknown remote '{name}' (missing 'type' field)");
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_target_string(&self, service: &str, target: &str) -> Result<()> {
        let resolved = self.parse_target(service, target)?;
        if !self.environments.contains(&resolved.environment) {
            bail!(
                "{}",
                suggest::unknown_env_in_target(&resolved.environment, target, &self.environments)
            );
        }
        if let Some(app) = &resolved.app {
            if !self.apps.contains_key(app) {
                let app_names: Vec<String> = self.apps.keys().cloned().collect();
                bail!(
                    "{}",
                    suggest::unknown_app_in_target(app, target, &app_names)
                );
            }
        }
        Ok(())
    }

    /// Parse a target string like `"web:dev"` or `"dev"` into a `ResolvedTarget`.
    pub fn parse_target(&self, service: &str, target: &str) -> Result<ResolvedTarget> {
        if let Some((app, env)) = target.split_once(':') {
            Ok(ResolvedTarget {
                service: service.to_string(),
                app: Some(app.to_string()),
                environment: env.to_string(),
            })
        } else {
            Ok(ResolvedTarget {
                service: service.to_string(),
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
                for (service, target_strs) in &def.targets {
                    for target_str in target_strs {
                        targets.push(self.parse_target(service, target_str)?);
                    }
                }
                result.push(ResolvedSecret {
                    key: key.clone(),
                    vendor: vendor.clone(),
                    description: def.description.clone(),
                    targets,
                    validation: def.validate.clone(),
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
            .targets
            .env
            .as_ref()
            .context("env target not configured")?;

        let app_config = self
            .apps
            .get(app)
            .with_context(|| format!("unknown app '{app}'"))?;

        let suffix = env_config.env_suffix.get(env).cloned().unwrap_or_default();

        let path = env_config
            .pattern
            .replace("{app_path}", &app_config.path)
            .replace("{env_suffix}", &suffix);

        let resolved = self.root.join(&path);

        // Defence-in-depth: ensure resolved path stays within project root
        if !is_within_root(&self.root, &resolved) {
            bail!(
                "env path '{}' resolves outside project root",
                resolved.display()
            );
        }

        // Additional defence: reject symlink escapes (e.g. apps -> /tmp/outside).
        let root_real = std::fs::canonicalize(&self.root).with_context(|| {
            format!(
                "failed to canonicalize project root {}",
                self.root.display()
            )
        })?;
        let existing = nearest_existing_ancestor(&resolved)
            .context("env path has no existing ancestor to validate")?;
        let existing_real = std::fs::canonicalize(existing)
            .with_context(|| format!("failed to canonicalize {}", existing.display()))?;
        if !existing_real.starts_with(&root_real) {
            bail!(
                "env path '{}' escapes project root via symlinked components",
                resolved.display()
            );
        }

        Ok(resolved)
    }

    /// Get the parsed 1Password remote config, if configured.
    pub fn onepassword_remote_config(&self) -> Option<OnePasswordRemoteConfig> {
        self.remotes
            .get("1password")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok())
    }

    /// Get a typed remote config by name.
    pub fn remote_config<T: serde::de::DeserializeOwned>(&self, name: &str) -> Option<T> {
        self.remotes
            .get(name)
            .and_then(|v| serde_yaml::from_value(v.clone()).ok())
    }

    /// Get all cloud_file remote configs: (name, config) pairs.
    pub fn cloud_file_remote_configs(&self) -> Vec<(String, CloudFileRemoteConfig)> {
        self.remotes
            .iter()
            .filter_map(|(name, value)| {
                if name == "1password" {
                    return None;
                }
                let type_val = value.get("type")?.as_str()?;
                if type_val != "cloud_file" {
                    return None;
                }
                let cfg: CloudFileRemoteConfig = serde_yaml::from_value(value.clone()).ok()?;
                Some((name.clone(), cfg))
            })
            .collect()
    }

    /// Get the set of configured target names.
    pub fn target_names(&self) -> Vec<&str> {
        let mut names = Vec::new();
        if self.targets.env.is_some() {
            names.push("env");
        }
        if self.targets.cloudflare.is_some() {
            names.push("cloudflare");
        }
        if self.targets.convex.is_some() {
            names.push("convex");
        }
        if self.targets.fly.is_some() {
            names.push("fly");
        }
        if self.targets.netlify.is_some() {
            names.push("netlify");
        }
        if self.targets.vercel.is_some() {
            names.push("vercel");
        }
        if self.targets.github.is_some() {
            names.push("github");
        }
        if self.targets.heroku.is_some() {
            names.push("heroku");
        }
        if self.targets.supabase.is_some() {
            names.push("supabase");
        }
        if self.targets.railway.is_some() {
            names.push("railway");
        }
        if self.targets.gitlab.is_some() {
            names.push("gitlab");
        }
        if self.targets.aws_ssm.is_some() {
            names.push("aws_ssm");
        }
        if self.targets.kubernetes.is_some() {
            names.push("kubernetes");
        }
        if self.targets.docker.is_some() {
            names.push("docker");
        }
        names
    }
}

impl std::fmt::Display for ResolvedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.service)?;
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
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .context("failed to create temp file for config write")?;
    std::fs::write(tmp.path(), &new_content).context("failed to write temp config file")?;
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
        let path = dir.join("esk.yaml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn find_in_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("esk.yaml"),
            "project: x\nenvironments: [dev]",
        )
        .unwrap();
        let found = Config::find(dir.path()).unwrap();
        assert_eq!(found, dir.path().join("esk.yaml"));
    }

    #[test]
    fn find_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("esk.yaml"),
            "project: x\nenvironments: [dev]",
        )
        .unwrap();
        let child = dir.path().join("sub");
        std::fs::create_dir(&child).unwrap();
        let found = Config::find(&child).unwrap();
        assert_eq!(found, dir.path().join("esk.yaml"));
    }

    #[test]
    fn find_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::find(dir.path()).unwrap_err();
        assert!(err.to_string().contains("esk.yaml not found"));
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
targets:
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
remotes:
  1password:
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
        assert!(config.targets.env.is_some());
        assert!(config.targets.cloudflare.is_some());
        assert!(config.targets.convex.is_some());
        assert!(config.onepassword_remote_config().is_some());
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
        let err = Config::load(Path::new("/nonexistent/esk.yaml")).unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }

    #[test]
    fn load_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "not: [valid: yaml: {{}}");
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validate_project_name_with_path_separator() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: ../escape\nenvironments: [dev]");
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("invalid project"));
    }

    #[test]
    fn validate_environment_with_colon() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: myapp\nenvironments: [\"dev:test\"]");
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("invalid environment"));
    }

    #[test]
    fn validate_environment_with_space() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: myapp\nenvironments: [\"dev test\"]");
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("invalid environment"));
    }

    #[test]
    fn validate_app_name_with_slash() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: myapp\nenvironments: [dev]\napps:\n  web/api:\n    path: apps/api";
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("invalid app"));
    }

    #[test]
    fn validate_empty_environments() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: []");
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("at least one environment"));
    }

    #[test]
    fn validate_unknown_target_reference() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
targets:
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
        let chain = console::strip_ansi_codes(&format!("{err:?}")).to_string();
        assert!(chain.contains("target 'cloudflare' is not configured"));
    }

    #[test]
    fn validate_service_known_but_unconfigured() {
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
        let chain = console::strip_ansi_codes(&format!("{err:?}")).to_string();
        assert!(chain.contains("target 'env' is not configured"));
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
targets:
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
        let chain = console::strip_ansi_codes(&format!("{err:?}")).to_string();
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
targets:
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
        let chain = console::strip_ansi_codes(&format!("{err:?}")).to_string();
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
targets:
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
        1password: [dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        let chain = format!("{err:?}");
        assert!(chain.contains("remotes:"));
    }

    #[test]
    fn validate_remotes_onepassword() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let op = config.onepassword_remote_config().unwrap();
        assert_eq!(op.vault, "V");
        assert_eq!(op.item_pattern, "test");
    }

    #[test]
    fn validate_remotes_cloud_file() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
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
        let cfs = config.cloud_file_remote_configs();
        assert_eq!(cfs.len(), 2);
    }

    #[test]
    fn validate_remotes_unknown_type_errors() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
  foo:
    type: unknown_thing
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("unknown remote type"));
    }

    #[test]
    fn validate_remotes_unknown_name_no_type_errors() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
  foo:
    bar: baz
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("unknown remote 'foo'"));
    }

    #[test]
    fn parse_target_with_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let target = config.parse_target("env", "web:dev").unwrap();
        assert_eq!(target.service, "env");
        assert_eq!(target.app, Some("web".to_string()));
        assert_eq!(target.environment, "dev");
    }

    #[test]
    fn parse_target_without_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_yaml(dir.path(), "project: x\nenvironments: [dev]");
        let config = Config::load(&path).unwrap();
        let target = config.parse_target("cloudflare", "prod").unwrap();
        assert_eq!(target.service, "cloudflare");
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
targets:
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
targets:
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
targets:
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
    fn validate_invalid_secret_key_name() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
targets:
  env:
    pattern: "t"
apps:
  web:
    path: w
secrets:
  G:
    INVALID-KEY:
      targets:
        env: [web:dev]
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid secret key"), "error was: {msg}");
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
targets:
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
targets:
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
targets:
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
        assert!(err.to_string().contains("env target not configured"));
    }

    #[test]
    fn resolve_env_path_unknown_app() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
targets:
  env:
    pattern: "test"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let err = config.resolve_env_path("nope", "dev").unwrap_err();
        assert!(err.to_string().contains("unknown app 'nope'"));
    }

    #[test]
    fn target_names_returns_configured() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
targets:
  env:
    pattern: "test"
  cloudflare: {}
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let names = config.target_names();
        assert!(names.contains(&"env"));
        assert!(names.contains(&"cloudflare"));
        assert!(!names.contains(&"convex"));
    }

    #[test]
    fn resolved_target_display_with_app() {
        let t = ResolvedTarget {
            service: "env".to_string(),
            app: Some("web".to_string()),
            environment: "dev".to_string(),
        };
        assert_eq!(t.to_string(), "env:web:dev");
    }

    #[test]
    fn resolved_target_display_without_app() {
        let t = ResolvedTarget {
            service: "cloudflare".to_string(),
            app: None,
            environment: "prod".to_string(),
        };
        assert_eq!(t.to_string(), "cloudflare:prod");
    }

    #[test]
    fn validate_env_pattern_rejects_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
targets:
  env:
    pattern: "../../../etc/{env_suffix}"
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("must not contain '..'"));
    }

    #[test]
    fn validate_env_suffix_rejects_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}"
    env_suffix:
      dev: "../../etc/passwd"
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("unsafe path characters"));
    }

    #[test]
    fn validate_env_suffix_rejects_slash() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}"
    env_suffix:
      dev: "/etc/passwd"
"#;
        let path = write_yaml(dir.path(), yaml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("unsafe path characters"));
    }

    #[test]
    fn validate_env_suffix_allows_safe_values() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev, prod]
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}"
    env_suffix:
      dev: ""
      prod: ".production"
"#;
        let path = write_yaml(dir.path(), yaml);
        assert!(Config::load(&path).is_ok());
    }

    #[test]
    fn resolve_env_path_rejects_traversal_via_app_path() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: "../../../etc"
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}"
    env_suffix:
      dev: ""
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let err = config.resolve_env_path("web", "dev").unwrap_err();
        assert!(err.to_string().contains("resolves outside project root"));
    }

    #[test]
    fn resolve_env_path_allows_normal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}"
    env_suffix:
      dev: ".local"
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let result = config.resolve_env_path("web", "dev");
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn resolve_env_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("apps")).unwrap();

        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}"
    env_suffix:
      dev: ""
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let err = config.resolve_env_path("web", "dev").unwrap_err();
        assert!(err
            .to_string()
            .contains("escapes project root via symlinked components"));
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
        let yaml =
            "project: x\nenvironments: [dev]\n# My secrets\nsecrets:\n  Stripe:\n    SK: {}\n";
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

    #[test]
    fn parse_validate_block() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
secrets:
  General:
    PORT:
      validate:
        format: integer
        range: [1, 65535]
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let (_, def) = config.find_secret("PORT").unwrap();
        let v = def.validate.as_ref().unwrap();
        assert_eq!(v.format, Some(crate::validate::Format::Integer));
        assert_eq!(v.range, Some((1.0, 65535.0)));
    }

    #[test]
    fn parse_validate_with_enum() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
secrets:
  General:
    NODE_ENV:
      validate:
        enum: [development, staging, production]
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let (_, def) = config.find_secret("NODE_ENV").unwrap();
        let v = def.validate.as_ref().unwrap();
        assert!(v.enum_values.is_some());
        assert_eq!(v.enum_values.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn validate_rejects_bad_regex_in_spec() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
secrets:
  General:
    KEY:
      validate:
        pattern: "[invalid"
"#;
        let path = write_yaml(dir.path(), yaml);
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validate_rejects_range_on_string_format() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
secrets:
  General:
    KEY:
      validate:
        format: string
        range: [1, 10]
"#;
        let path = write_yaml(dir.path(), yaml);
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn no_validate_block_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
secrets:
  General:
    KEY: {}
"#;
        let path = write_yaml(dir.path(), yaml);
        let config = Config::load(&path).unwrap();
        let (_, def) = config.find_secret("KEY").unwrap();
        assert!(def.validate.is_none());
    }
}
