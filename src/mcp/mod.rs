pub mod types;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;

use crate::cli::deploy::DeployOptions;
use crate::cli::status::Dashboard;
use crate::config::Config;
use crate::store::SecretStore;
use crate::targets::RealCommandRunner;
use crate::validate;

use types::{
    DeleteResponse, DeployResponse, EnvVersion, GenerateResponse, GetResponse, ListResponse,
    ListSecret, ListSecretEnv, SetResponse, StatusCoverageGap, StatusMissing, StatusNextStep,
    StatusResponse, StatusWarning,
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
        description = "List all secrets with their status per environment and deploy target. Returns structured JSON with deploy state (deployed/pending/failed/unset/not_targeted) for each secret×environment pair."
    )]
    async fn list(&self, params: Parameters<ListParams>) -> Result<CallToolResult, ErrorData> {
        match do_list(&params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_status",
        description = "Show project deploy and sync status: pending/failed/deployed counts, validation warnings, missing required secrets, coverage gaps, and recommended next steps."
    )]
    async fn status(&self, params: Parameters<StatusParams>) -> Result<CallToolResult, ErrorData> {
        match do_status(&params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_deploy",
        description = "Deploy secrets to configured targets (env files, Cloudflare, Vercel, etc.). Skips secrets that haven't changed unless force=true."
    )]
    async fn deploy(&self, params: Parameters<DeployParams>) -> Result<CallToolResult, ErrorData> {
        match do_deploy(&params.0) {
            Ok(resp) => json_result(&resp),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "esk_generate",
        description = "Generate code or config files from secret definitions. Formats: 'dts' (TypeScript declarations), 'ts' (runtime module), 'env-example' (.env.example). Omit format to run all configured outputs."
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
    let store = SecretStore::open(&config.root)?;
    let value = store.get(&params.key, &params.env)?;
    Ok(GetResponse {
        key: params.key,
        env: params.env,
        value,
    })
}

fn do_set(params: SetParams) -> anyhow::Result<SetResponse> {
    let config = Config::find_and_load()?;

    // Run validation if the secret has a validation spec
    if !params.skip_validation {
        if let Some((_, def)) = config.find_secret(&params.key) {
            if let Some(ref spec) = def.validate {
                validate::validate_value(&params.key, &params.value, spec).map_err(|e| {
                    anyhow::anyhow!("validation failed for {}: {}", params.key, e.message)
                })?;
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
    let store = SecretStore::open(&config.root)?;
    let payload = store.delete(&params.key, &params.env)?;
    Ok(DeleteResponse {
        key: params.key,
        env: params.env,
        version: payload.version,
    })
}

fn do_list(params: &ListParams) -> anyhow::Result<ListResponse> {
    use crate::deploy_tracker::{DeployIndex, DeployStatus};

    let config = Config::find_and_load()?;
    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let resolved = config.resolve_secrets()?;
    let index_path = config.root.join(".esk/deploy-index.json");
    let index = DeployIndex::load(&index_path);
    let target_names: Vec<&str> = config.target_names();

    let envs: Vec<&str> = match &params.env {
        Some(e) => vec![e.as_str()],
        None => config.environments.iter().map(String::as_str).collect(),
    };

    let mut secrets = Vec::new();
    for secret in &resolved {
        let mut environments = Vec::new();

        for &env_name in &envs {
            let composite = format!("{}:{}", secret.key, env_name);
            let has_value = payload.secrets.contains_key(&composite);

            // Find targets for this env
            let env_targets: Vec<_> = secret
                .targets
                .iter()
                .filter(|t| t.environment == env_name && target_names.contains(&t.service.as_str()))
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
                            let current_hash =
                                DeployIndex::hash_value(payload.secrets.get(&composite).unwrap());
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
            group: secret.vendor.clone(),
            description: secret.description.clone(),
            environments,
        });
    }

    Ok(ListResponse {
        secrets,
        environments: envs.iter().map(|s| (*s).to_string()).collect(),
    })
}

fn do_status(params: &StatusParams) -> anyhow::Result<StatusResponse> {
    let config = Config::find_and_load()?;
    let runner = RealCommandRunner;
    let dashboard = Dashboard::build(&config, params.env.as_deref(), &runner)?;

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
    let opts = DeployOptions {
        env: params.env.as_deref(),
        force: params.force,
        dry_run: params.dry_run,
        verbose: false,
        skip_validation: false,
        skip_requirements: false,
        allow_empty: true,
        prune: params.prune,
    };

    match crate::cli::deploy::run(&config, &opts) {
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

    let format = match &params.format {
        Some(f) => {
            let parsed: crate::config::GenerateFormat = match f.as_str() {
                "dts" => crate::config::GenerateFormat::Dts,
                "ts" => crate::config::GenerateFormat::Ts,
                "env-example" => crate::config::GenerateFormat::EnvExample,
                other => {
                    anyhow::bail!("unknown format '{other}': use 'dts', 'ts', or 'env-example'")
                }
            };
            Some(parsed)
        }
        None => None,
    };

    match crate::cli::generate::run(&config, format.as_ref(), None) {
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

fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| ErrorData::internal_error(format!("JSON serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn error_result(err: &anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!("{err:#}"))])
}
