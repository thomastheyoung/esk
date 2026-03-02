use std::collections::HashMap;

use crate::validate;

// ---------------------------------------------------------------------------
// Dashboard data model
// ---------------------------------------------------------------------------

pub(crate) struct DeployEntry {
    pub(crate) key: String,
    pub(crate) env: String,
    pub(crate) target: String,
    pub(crate) error: Option<String>,
    pub(crate) last_deployed_at: Option<String>,
}

pub(crate) struct CoverageGap {
    pub(crate) key: String,
    pub(crate) missing_envs: Vec<String>,
    pub(crate) present_envs: Vec<String>,
}

pub(crate) struct Orphan {
    pub(crate) key: String,
    pub(crate) env: String,
}

#[derive(Clone)]
pub(crate) enum RemoteStatus {
    Current { version: u64 },
    Stale { pushed: u64, local: u64 },
    Failed { version: u64, error: String },
    NeverSynced,
}

pub(crate) struct RemoteState {
    pub(crate) name: String,
    pub(crate) env: String,
    pub(crate) status: RemoteStatus,
}

pub(crate) struct ValidationWarning {
    pub(crate) key: String,
    pub(crate) env: String,
    pub(crate) message: String,
}

pub(crate) struct EmptyValueWarning {
    pub(crate) key: String,
    pub(crate) env: String,
    pub(crate) kind: &'static str,
}

pub(crate) struct NextStep {
    pub(crate) command: String,
    pub(crate) description: String,
}

pub(crate) struct Dashboard {
    pub(crate) project: String,
    pub(crate) version: u64,
    pub(crate) filtered_env: Option<String>,
    pub(crate) env_versions: Vec<(String, u64)>,
    pub(crate) failed: Vec<DeployEntry>,
    pub(crate) pending: Vec<DeployEntry>,
    pub(crate) deployed: Vec<DeployEntry>,
    pub(crate) unset: Vec<DeployEntry>,
    pub(crate) validation_warnings: Vec<ValidationWarning>,
    pub(crate) cross_field_violations: Vec<validate::CrossFieldViolation>,
    pub(crate) empty_values: Vec<EmptyValueWarning>,
    pub(crate) missing_required: Vec<crate::config::MissingRequirement>,
    pub(crate) coverage_gaps: Vec<CoverageGap>,
    pub(crate) orphans: Vec<Orphan>,
    pub(crate) target_orphans: Vec<crate::orphan::TargetOrphan>,
    pub(crate) remote_states: Vec<RemoteState>,
    pub(crate) next_steps: Vec<NextStep>,
}

// ---------------------------------------------------------------------------
// Grouping helpers (collapse per-target lines into one line per key:env)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub(crate) enum GroupedFreshness {
    NeverDeployed,
    Timestamp(String),
}

#[derive(Debug)]
pub(crate) struct GroupedEntry {
    pub(crate) key: String,
    pub(crate) env: String,
    pub(crate) targets: Vec<String>,
    pub(crate) freshness: GroupedFreshness,
}

/// Whether to keep the oldest or newest timestamp when merging groups.
#[derive(Clone, Copy)]
pub(crate) enum TimestampPick {
    Oldest,
    Newest,
}

/// Groups deploy entries by (key, env), merging their target names.
///
/// Freshness rule: if *any* entry in the group has no `last_deployed_at`,
/// the group is `NeverDeployed`. Otherwise keep the oldest or newest
/// timestamp based on `pick`.
pub(crate) fn group_entries(entries: &[DeployEntry], pick: TimestampPick) -> Vec<GroupedEntry> {
    let mut groups: Vec<GroupedEntry> = Vec::new();
    let mut index: HashMap<(&str, &str), usize> = HashMap::new();

    for entry in entries {
        let map_key = (entry.key.as_str(), entry.env.as_str());
        if let Some(&pos) = index.get(&map_key) {
            let group = &mut groups[pos];
            group.targets.push(entry.target.clone());
            // Update freshness
            if group.freshness != GroupedFreshness::NeverDeployed {
                match &entry.last_deployed_at {
                    None => group.freshness = GroupedFreshness::NeverDeployed,
                    Some(ts) => {
                        if let GroupedFreshness::Timestamp(ref existing) = group.freshness {
                            let replace = match pick {
                                TimestampPick::Newest => ts > existing,
                                TimestampPick::Oldest => ts < existing,
                            };
                            if replace {
                                group.freshness = GroupedFreshness::Timestamp(ts.clone());
                            }
                        }
                    }
                }
            }
        } else {
            let freshness = match &entry.last_deployed_at {
                None => GroupedFreshness::NeverDeployed,
                Some(ts) => GroupedFreshness::Timestamp(ts.clone()),
            };
            let pos = groups.len();
            groups.push(GroupedEntry {
                key: entry.key.clone(),
                env: entry.env.clone(),
                targets: vec![entry.target.clone()],
                freshness,
            });
            index.insert(map_key, pos);
        }
    }

    groups
}
