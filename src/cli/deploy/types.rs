use std::collections::{BTreeMap, BTreeSet};

use zeroize::Zeroizing;

use crate::targets::SecretValue;

/// Maximum number of orphans allowed without `--force`.
pub(crate) const PRUNE_THRESHOLD: usize = 10;

pub(crate) const DEPLOY_LINE_WIDTH: usize = 20;

pub(crate) struct BatchGroup {
    pub target_name: String,
    pub app: Option<String>,
    pub secrets: Vec<SecretValue>,
    pub tombstoned_keys: BTreeSet<String>,
    pub target_idx: usize,
}

#[derive(Default)]
pub(crate) struct EnvWorkPlan {
    pub batch_groups: Vec<BatchGroup>,
    pub individual: Vec<(String, Zeroizing<String>, crate::config::ResolvedTarget)>,
    pub tombstones: Vec<(String, crate::config::ResolvedTarget)>,
    pub prune_individual: Vec<crate::orphan::TargetOrphan>,
    pub batch_prune: BTreeMap<(String, Option<String>), Vec<crate::orphan::TargetOrphan>>,
}

impl EnvWorkPlan {
    pub fn has_work(&self) -> bool {
        self.batch_groups
            .iter()
            .any(|bg| !bg.secrets.is_empty() || !bg.tombstoned_keys.is_empty())
            || !self.individual.is_empty()
            || !self.tombstones.is_empty()
            || !self.prune_individual.is_empty()
            || !self.batch_prune.is_empty()
    }
}

pub(crate) struct PlanOutput {
    pub env_plans: BTreeMap<String, EnvWorkPlan>,
    pub unset: Vec<super::report::DeployEntry>,
    pub skipped: Vec<super::report::DeployEntry>,
    pub unavailable_orphans: Vec<crate::orphan::TargetOrphan>,
}

impl PlanOutput {
    pub fn is_empty(&self) -> bool {
        self.env_plans.values().all(|p| !p.has_work())
    }
}

/// A single display line: one key with all its target names.
pub(crate) struct KeyLine {
    pub key: String,
    pub targets: Vec<String>,
    pub total_ops: usize,
}

#[derive(Default)]
pub(crate) struct KeyResult {
    pub completed_ops: usize,
    pub total_ops: usize,
    pub failed: Vec<(String, String)>, // (target_display, error)
}

impl KeyResult {
    pub fn is_done(&self) -> bool {
        self.completed_ops >= self.total_ops
    }
    pub fn has_failure(&self) -> bool {
        !self.failed.is_empty()
    }
}
