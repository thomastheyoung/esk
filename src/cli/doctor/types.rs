#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

pub(crate) struct Check {
    pub(crate) status: CheckStatus,
    pub(crate) label: String,
    pub(crate) detail: String,
}

impl Check {
    pub(crate) fn pass(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Pass,
            label: label.into(),
            detail: detail.into(),
        }
    }

    pub(crate) fn warn(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Warn,
            label: label.into(),
            detail: detail.into(),
        }
    }

    pub(crate) fn fail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Fail,
            label: label.into(),
            detail: detail.into(),
        }
    }
}

/// A section that was either checked (with findings) or skipped (with reason).
pub(crate) enum Section {
    Checked(Vec<Check>),
    Skipped(String),
}

pub(crate) struct Suggestion {
    pub(crate) command: String,
    pub(crate) reason: String,
}

pub(crate) struct Report {
    pub(crate) project: Option<String>,
    pub(crate) root: std::path::PathBuf,
    pub(crate) structure: Vec<Check>,
    pub(crate) config: Section,
    pub(crate) store_consistency: Section,
    pub(crate) secrets_health: Section,
    pub(crate) suggestions: Vec<Suggestion>,
}
