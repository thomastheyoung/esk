use serde::Serialize;

#[derive(Serialize)]
pub struct GetResponse {
    pub key: String,
    pub env: String,
    pub value: Option<String>,
}

#[derive(Serialize)]
pub struct SetResponse {
    pub key: String,
    pub env: String,
    pub version: u64,
}

#[derive(Serialize)]
pub struct DeleteResponse {
    pub key: String,
    pub env: String,
    pub version: u64,
}

#[derive(Serialize)]
pub struct ListResponse {
    pub secrets: Vec<ListSecret>,
    pub environments: Vec<String>,
}

#[derive(Serialize)]
pub struct ListSecret {
    pub key: String,
    pub group: String,
    pub description: Option<String>,
    pub environments: Vec<ListSecretEnv>,
}

#[derive(Serialize)]
pub struct ListSecretEnv {
    pub env: String,
    pub has_value: bool,
    /// One of: "deployed", "pending", "failed", "unset", "not_targeted"
    pub status: String,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub project: String,
    pub version: u64,
    pub env_versions: Vec<EnvVersion>,
    pub pending: usize,
    pub failed: usize,
    pub deployed: usize,
    pub unset: usize,
    pub validation_warnings: Vec<StatusWarning>,
    pub missing_required: Vec<StatusMissing>,
    pub coverage_gaps: Vec<StatusCoverageGap>,
    pub next_steps: Vec<StatusNextStep>,
}

#[derive(Serialize)]
pub struct EnvVersion {
    pub env: String,
    pub version: u64,
}

#[derive(Serialize)]
pub struct StatusWarning {
    pub key: String,
    pub env: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct StatusMissing {
    pub key: String,
    pub env: String,
}

#[derive(Serialize)]
pub struct StatusCoverageGap {
    pub key: String,
    pub missing_envs: Vec<String>,
    pub present_envs: Vec<String>,
}

#[derive(Serialize)]
pub struct StatusNextStep {
    pub command: String,
    pub description: String,
}

#[derive(Serialize)]
pub struct DeployResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize)]
pub struct GenerateResponse {
    pub success: bool,
    pub message: String,
}
