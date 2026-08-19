pub mod types;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;

use crate::cli::deploy::DeployOptions;
use crate::cli::status::types::Dashboard;
use crate::config::Config;
use crate::deploy_tracker::{DeployIndex, DeployStatus};
use crate::store::SecretStore;
use crate::validate;

use types::{
    DeleteResponse, DeployResponse, EnvVersion, GenerateResponse, GetResponse, ListResponse,
    ListSecret, ListSecretEnv, SetResponse, StatusCoverageGap, StatusCrossFieldViolation,
    StatusMissing, StatusNextStep, StatusResponse, StatusWarning,
};

// ---------------------------------------------------------------------------
// Param structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParams {
    /// Secret key name (e.g. "DATABASE_URL")
    pub key: String,
    /// Environment name (e.g. "dev", "prod")
    pub env: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetParams {
    /// Secret key name
    pub key: String,
    /// Environment name
    pub env: String,
    /// Secret value to store
    pub value: String,
    /// Skip value validation
    #[serde(default)]
    pub skip_validation: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteParams {
    /// Secret key name
    pub key: String,
    /// Environment name
    pub env: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Filter by environment (omit to list all)
    #[serde(default)]
    pub env: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusParams {
    /// Filter by environment (omit for all)
    #[serde(default)]
    pub env: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployParams {
    /// Filter by environment (omit for all)
    #[serde(default)]
    pub env: Option<String>,
    /// Force deploy even if hashes match
    #[serde(default)]
    pub force: bool,
    /// Show what would be deployed without deploying
    #[serde(default)]
    pub dry_run: bool,
    /// Remove orphaned secrets from targets
    #[serde(default)]
    pub prune: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateParams {
    /// Output format: "dts", "ts", or "env-example" (omit to run all configured)
    #[serde(default)]
    pub format: Option<String>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EskMcpServer {
    tool_router: ToolRouter<Self>,
}

impl Default for EskMcpServer {
    fn default() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

impl EskMcpServer {
    pub fn new() -> Self {
        Self::default()
    }
}

#[tool_router]
impl EskMcpServer {
    #[tool(
        name = "esk_get",
        description = "Retrieve a secret value from the encrypted store"
    )]
    async fn get(&self, params: Parameters<GetParams>) -> Result<CallToolResult, ErrorData> {
        match do_get(params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_set",
        description = "Set a secret value in the encrypted store. Does NOT auto-deploy or auto-sync — call esk_deploy explicitly after setting secrets."
    )]
    async fn set(&self, params: Parameters<SetParams>) -> Result<CallToolResult, ErrorData> {
        match do_set(params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_delete",
        description = "Delete a secret value from the encrypted store. Does NOT auto-deploy — call esk_deploy explicitly if needed."
    )]
    async fn delete(&self, params: Parameters<DeleteParams>) -> Result<CallToolResult, ErrorData> {
        match do_delete(params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_list",
        description = "List all secrets with their status per environment and deploy target. Returns structured JSON with deploy state (deployed/pending/failed/unset/not_targeted) for each secret×environment pair. Note: 'deployed' means esk successfully SENT the value to the target, recorded in its local deploy index — it is not a read-back confirmation that the target still holds it. Only `esk verify` queries targets directly, and only some targets support it."
    )]
    async fn list(&self, params: Parameters<ListParams>) -> Result<CallToolResult, ErrorData> {
        match do_list(&params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_status",
        description = "Show project deploy and sync status: pending/failed/sent counts, validation warnings, missing required secrets, coverage gaps, and recommended next steps. Counts reflect what esk last sent; for targets esk cannot read back, they do not confirm the target's current contents."
    )]
    async fn status(&self, params: Parameters<StatusParams>) -> Result<CallToolResult, ErrorData> {
        match do_status(&params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_deploy",
        description = "Deploy secrets to configured targets (env files, Cloudflare, Vercel, etc.). Skips secrets that haven't changed unless force=true, except that a generated file esk can read back is regenerated when it no longer matches the store."
    )]
    async fn deploy(&self, params: Parameters<DeployParams>) -> Result<CallToolResult, ErrorData> {
        match do_deploy(&params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_generate",
        description = "Generate code or config files from secret definitions. Formats: 'dts' (TypeScript declarations), 'ts' (runtime module), 'ts-lazy' (lazy runtime module), 'zod' (Zod schema), 'env-example' (.env.example). Omit format to run all configured outputs."
    )]
    async fn generate(
        &self,
        params: Parameters<GenerateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match do_generate(&params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }
}

#[tool_handler]
impl ServerHandler for EskMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "esk — encrypted secrets management. Use esk_list or esk_status to understand \
                 the project state, esk_get/esk_set/esk_delete to manage secret values, \
                 esk_deploy to push to targets, and esk_generate to create config files."
                    .into(),
            ),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementations (sync, called from async wrappers)
// ---------------------------------------------------------------------------

fn do_get(params: GetParams) -> anyhow::Result<GetResponse> {
    let config = Config::find_and_load()?;
    do_get_with_config(&config, params)
}

fn do_get_with_config(config: &Config, params: GetParams) -> anyhow::Result<GetResponse> {
    ensure_env_allowed(config, &params.env)?;
    let store = SecretStore::open(&config.root)?;
    let value = store.get(&params.key, &params.env)?;
    Ok(GetResponse {
        key: params.key,
        env: params.env,
        value: value.map(|value| {
            if config.mcp.expose_values {
                value
            } else {
                "<redacted>".to_string()
            }
        }),
    })
}

fn do_set(params: SetParams) -> anyhow::Result<SetResponse> {
    let config = Config::find_and_load()?;
    do_set_with_config(&config, params)
}

fn do_set_with_config(config: &Config, params: SetParams) -> anyhow::Result<SetResponse> {
    ensure_writable(config)?;
    ensure_env_allowed(config, &params.env)?;

    // Run validation if the secret has a validation spec
    if !params.skip_validation {
        if let Some((_, def)) = config.find_secret(&params.key) {
            if let Some(ref spec) = def.validate {
                validate::validate_value(&params.key, &params.value, spec)?;
            }
        }
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.set(&params.key, &params.env, &params.value)?;
    Ok(SetResponse {
        key: params.key,
        env: params.env,
        version: payload.version,
    })
}

fn do_delete(params: DeleteParams) -> anyhow::Result<DeleteResponse> {
    let config = Config::find_and_load()?;
    do_delete_with_config(&config, params)
}

fn do_delete_with_config(config: &Config, params: DeleteParams) -> anyhow::Result<DeleteResponse> {
    ensure_writable(config)?;
    ensure_env_allowed(config, &params.env)?;
    let store = SecretStore::open(&config.root)?;
    let payload = store.delete(&params.key, &params.env)?;
    Ok(DeleteResponse {
        key: params.key,
        env: params.env,
        version: payload.version,
    })
}

fn do_list(params: &ListParams) -> anyhow::Result<ListResponse> {
    let config = Config::find_and_load()?;
    do_list_with_config(&config, params)
}

fn do_list_with_config(config: &Config, params: &ListParams) -> anyhow::Result<ListResponse> {
    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let resolved = config.resolve_secrets()?;
    let index_path = config.root.join(".esk/deploy-index.json");
    let (index, _) = DeployIndex::load(&index_path);
    let target_names: Vec<&str> = config.target_names();

    let envs = permitted_envs(config, params.env.as_deref())?;

    let mut secrets = Vec::new();
    for secret in &resolved {
        let mut environments = Vec::new();

        for env_name in &envs {
            let composite = format!("{}:{}", secret.key, env_name);
            let has_value = payload.secrets.contains_key(&composite);

            // Find targets for this env
            let env_targets: Vec<_> = secret
                .targets
                .iter()
                .filter(|t| {
                    t.environment == *env_name && target_names.contains(&t.service.as_str())
                })
                .collect();

            let status = if env_targets.is_empty() {
                "not_targeted".to_string()
            } else if !has_value {
                "unset".to_string()
            } else {
                // Check deploy status across all targets for this env
                let mut worst = "deployed";
                for target in &env_targets {
                    let tracker_key = DeployIndex::tracker_key(
                        &secret.key,
                        &target.service,
                        target.app.as_deref(),
                        &target.environment,
                    );
                    match index.records.get(&tracker_key) {
                        None => {
                            worst = "pending";
                            break;
                        }
                        Some(rec) if rec.last_deploy_status == DeployStatus::Failed => {
                            worst = "failed";
                            break;
                        }
                        Some(rec) => {
                            let current_hash = DeployIndex::hash_value(
                                payload.secrets.get(&composite).unwrap_or(&String::new()),
                                store.master_key(),
                            );
                            if current_hash != rec.value_hash {
                                worst = "pending";
                            }
                        }
                    }
                }
                worst.to_string()
            };

            environments.push(ListSecretEnv {
                env: env_name.to_string(),
                has_value,
                status,
            });
        }

        secrets.push(ListSecret {
            key: secret.key.clone(),
            group: secret.group.clone(),
            description: secret.description.clone(),
            environments,
        });
    }

    Ok(ListResponse {
        secrets,
        environments: envs,
    })
}

fn do_status(params: &StatusParams) -> anyhow::Result<StatusResponse> {
    let config = Config::find_and_load()?;
    do_status_with_config(&config, params)
}

fn do_status_with_config(config: &Config, params: &StatusParams) -> anyhow::Result<StatusResponse> {
    let env = match params.env.as_deref() {
        Some(env) => {
            ensure_env_allowed(config, env)?;
            Some(env)
        }
        None if !config.mcp.envs.is_empty() => {
            anyhow::bail!("MCP env policy requires esk_status to specify --env")
        }
        None => None,
    };
    let dashboard = Dashboard::build(config, env)?;

    Ok(StatusResponse {
        project: dashboard.project,
        version: dashboard.version,
        env_versions: dashboard
            .env_versions
            .into_iter()
            .map(|(env, version)| EnvVersion { env, version })
            .collect(),
        pending: dashboard.pending.len(),
        failed: dashboard.failed.len(),
        deployed: dashboard.deployed.len(),
        unset: dashboard.unset.len(),
        validation_warnings: dashboard
            .validation_warnings
            .iter()
            .map(|w| StatusWarning {
                key: w.key.clone(),
                env: w.env.clone(),
                message: w.message.clone(),
                violations: w.violations.clone(),
            })
            .collect(),
        cross_field_violations: dashboard
            .cross_field_violations
            .iter()
            .map(|v| StatusCrossFieldViolation {
                key: v.key().to_string(),
                env: v.env().to_string(),
                code: v.code(),
                references: v.references().to_vec(),
                message: v.message().to_string(),
            })
            .collect(),
        missing_required: dashboard
            .missing_required
            .iter()
            .map(|m| StatusMissing {
                key: m.key.clone(),
                env: m.env.clone(),
            })
            .collect(),
        coverage_gaps: dashboard
            .coverage_gaps
            .into_iter()
            .map(|g| StatusCoverageGap {
                key: g.key,
                missing_envs: g.missing_envs,
                present_envs: g.present_envs,
            })
            .collect(),
        next_steps: dashboard
            .next_steps
            .into_iter()
            .map(|s| StatusNextStep {
                command: s.command,
                description: s.description,
            })
            .collect(),
    })
}

fn do_deploy(params: &DeployParams) -> anyhow::Result<DeployResponse> {
    let config = Config::find_and_load()?;
    do_deploy_with_config(&config, params)
}

fn do_deploy_with_config(config: &Config, params: &DeployParams) -> anyhow::Result<DeployResponse> {
    ensure_writable(config)?;
    if let Some(env) = params.env.as_deref() {
        ensure_env_allowed(config, env)?;
    } else if !config.mcp.envs.is_empty() {
        anyhow::bail!("MCP env policy requires esk_deploy to specify --env")
    }
    let opts = DeployOptions {
        env: params.env.as_deref(),
        force: params.force,
        dry_run: params.dry_run,
        verbose: false,
        skip_validation: false,
        strict: false,
        allow_empty: false,
        prune: params.prune,
    };

    match crate::cli::deploy::run(config, &opts) {
        Ok(()) => Ok(DeployResponse {
            success: true,
            message: if params.dry_run {
                "Dry run completed".to_string()
            } else {
                "Deploy completed successfully".to_string()
            },
        }),
        Err(e) => Ok(DeployResponse {
            success: false,
            message: format!("{e:#}"),
        }),
    }
}

fn do_generate(params: &GenerateParams) -> anyhow::Result<GenerateResponse> {
    let config = Config::find_and_load()?;
    do_generate_with_config(&config, params)
}

fn do_generate_with_config(
    config: &Config,
    params: &GenerateParams,
) -> anyhow::Result<GenerateResponse> {
    let format = match &params.format {
        Some(f) => {
            let parsed: crate::config::GenerateFormat = match f.as_str() {
                "dts" => crate::config::GenerateFormat::Dts,
                "ts" => crate::config::GenerateFormat::Ts,
                "ts-lazy" => crate::config::GenerateFormat::TsLazy,
                "zod" => crate::config::GenerateFormat::Zod,
                "env-example" => crate::config::GenerateFormat::EnvExample,
                other => {
                    anyhow::bail!("unknown format '{other}': use 'dts', 'ts', 'ts-lazy', 'zod', or 'env-example'")
                }
            };
            Some(parsed)
        }
        None => None,
    };

    match crate::cli::generate::run(config, format.as_ref(), None, false) {
        Ok(()) => Ok(GenerateResponse {
            success: true,
            message: "Generate completed successfully".to_string(),
        }),
        Err(e) => Ok(GenerateResponse {
            success: false,
            message: format!("{e:#}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ensure_writable(config: &Config) -> anyhow::Result<()> {
    if config.mcp.read_only {
        anyhow::bail!("MCP server is read-only for this project")
    }
    Ok(())
}

fn ensure_env_allowed(config: &Config, env: &str) -> anyhow::Result<()> {
    config.validate_env(env)?;
    if !config.mcp.envs.is_empty() && !config.mcp.envs.iter().any(|allowed| allowed == env) {
        anyhow::bail!("environment '{env}' is not allowed by the MCP policy")
    }
    Ok(())
}

fn permitted_envs(config: &Config, requested: Option<&str>) -> anyhow::Result<Vec<String>> {
    if let Some(env) = requested {
        ensure_env_allowed(config, env)?;
        return Ok(vec![env.to_string()]);
    }

    Ok(config
        .environments
        .iter()
        .filter(|env| {
            config.mcp.envs.is_empty() || config.mcp.envs.iter().any(|allowed| allowed == *env)
        })
        .cloned()
        .collect())
}

fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| ErrorData::internal_error(format!("JSON serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn error_result(err: &anyhow::Error) -> CallToolResult {
    if let Some(validation) = err.downcast_ref::<validate::ValidationError>() {
        let body = serde_json::json!({
            "message": validation.to_string(),
            "violations": validation.violations(),
        });
        return CallToolResult::error(vec![Content::text(body.to_string())]);
    }
    CallToolResult::error(vec![Content::text(format!("{err:#}"))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SecretStore;

    fn project(mcp: &str) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            "project: demo\nenvironments: [dev, prod]\nmcp:\n{mcp}secrets:\n  App:\n    API_KEY:\n      description: API credential\n"
        );
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        SecretStore::load_or_create(dir.path())
            .unwrap()
            .set("API_KEY", "dev", "sentinel-value")
            .unwrap();
        (dir, Config::load(&path).unwrap())
    }

    fn validation_project() -> (tempfile::TempDir, Config, [&'static str; 4]) {
        let candidate = "candidate-sentinel";
        let allowed = "allowed-sentinel";
        let pattern = "^pattern-sentinel$";
        let predicate = "predicate-sentinel";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(
            &path,
            format!(
                "project: demo\nenvironments: [dev]\nsecrets:\n  App:\n    TOKEN:\n      validate:\n        enum: [{allowed}]\n        pattern: '{pattern}'\n    REQUIRED:\n      validate:\n        required_if:\n          SWITCH: {predicate}\n    SWITCH: {{}}\n"
            ),
        )
        .unwrap();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("TOKEN", "dev", candidate).unwrap();
        store.set("SWITCH", "dev", predicate).unwrap();
        (
            dir,
            Config::load(&path).unwrap(),
            [candidate, allowed, pattern, predicate],
        )
    }

    #[test]
    fn validation_responses_do_not_disclose_secret_or_constraint_values() {
        let (_dir, config, secrets) = validation_project();

        let status = do_status_with_config(
            &config,
            &StatusParams {
                env: Some("dev".into()),
            },
        )
        .unwrap();
        let status_json = serde_json::to_string(&status).unwrap();
        for secret_material in secrets {
            assert!(!status_json.contains(secret_material), "{status_json}");
        }
        assert!(!status.validation_warnings[0].violations.is_empty());
        assert_eq!(
            status.cross_field_violations[0].code,
            validate::ValidationCode::RequiredIf
        );

        let set_err = do_set_with_config(
            &config,
            SetParams {
                key: "TOKEN".into(),
                env: "dev".into(),
                value: "set-candidate-sentinel".into(),
                skip_validation: false,
            },
        )
        .unwrap_err();
        let set_message = set_err.to_string();
        for secret_material in ["set-candidate-sentinel", secrets[1], secrets[2]] {
            assert!(!set_message.contains(secret_material), "{set_message}");
        }
        let set_result_json = serde_json::to_string(&error_result(&set_err)).unwrap();
        assert!(set_result_json.contains("violations"), "{set_result_json}");
        for secret_material in ["set-candidate-sentinel", secrets[1], secrets[2]] {
            assert!(
                !set_result_json.contains(secret_material),
                "{set_result_json}"
            );
        }

        let deploy = do_deploy_with_config(
            &config,
            &DeployParams {
                env: Some("dev".into()),
                force: false,
                dry_run: false,
                prune: false,
            },
        )
        .unwrap();
        assert!(!deploy.success);
        assert!(!deploy.message.contains(secrets[3]), "{}", deploy.message);
    }

    #[test]
    fn get_redacts_values_by_default() {
        let (_dir, config) = project("");
        let response = do_get_with_config(
            &config,
            GetParams {
                key: "API_KEY".into(),
                env: "dev".into(),
            },
        )
        .unwrap();
        assert_eq!(response.value.as_deref(), Some("<redacted>"));
    }

    #[test]
    fn get_can_expose_values_only_when_opted_in() {
        let (_dir, mut config) = project("  expose_values: true\n");
        let response = do_get_with_config(
            &config,
            GetParams {
                key: "API_KEY".into(),
                env: "dev".into(),
            },
        )
        .unwrap();
        assert_eq!(response.value.as_deref(), Some("sentinel-value"));
        config.mcp.expose_values = false;
        let response = do_get_with_config(
            &config,
            GetParams {
                key: "API_KEY".into(),
                env: "dev".into(),
            },
        )
        .unwrap();
        assert_eq!(response.value.as_deref(), Some("<redacted>"));
    }

    #[test]
    fn environment_policy_applies_to_get_and_list() {
        let (_dir, config) = project("  envs: [dev]\n");
        let err = do_get_with_config(
            &config,
            GetParams {
                key: "API_KEY".into(),
                env: "prod".into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not allowed"));

        let response = do_list_with_config(&config, &ListParams { env: None }).unwrap();
        assert_eq!(response.environments, vec!["dev"]);
    }

    #[test]
    fn read_only_policy_blocks_set_and_delete_without_mutating_store() {
        let (_dir, config) = project("  read_only: true\n");
        let set_err = do_set_with_config(
            &config,
            SetParams {
                key: "API_KEY".into(),
                env: "dev".into(),
                value: "new-value".into(),
                skip_validation: false,
            },
        )
        .unwrap_err();
        assert!(set_err.to_string().contains("read-only"));
        let delete_err = do_delete_with_config(
            &config,
            DeleteParams {
                key: "API_KEY".into(),
                env: "dev".into(),
            },
        )
        .unwrap_err();
        assert!(delete_err.to_string().contains("read-only"));
        assert_eq!(
            SecretStore::open(&config.root)
                .unwrap()
                .get("API_KEY", "dev")
                .unwrap()
                .as_deref(),
            Some("sentinel-value")
        );
    }

    #[test]
    fn restricted_status_and_deploy_require_an_allowed_environment() {
        let (_dir, config) = project("  envs: [dev]\n");
        let status_err = do_status_with_config(&config, &StatusParams { env: None }).unwrap_err();
        assert!(status_err.to_string().contains("specify --env"));
        let deploy_err = do_deploy_with_config(
            &config,
            &DeployParams {
                env: None,
                force: false,
                dry_run: true,
                prune: false,
            },
        )
        .unwrap_err();
        assert!(deploy_err.to_string().contains("specify --env"));
    }
}
