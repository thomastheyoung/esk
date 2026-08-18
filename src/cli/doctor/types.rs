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

/// Counts of check outcomes across a report.
///
/// Both the text and JSON paths derive their pass/warn/fail totals from this
/// one type, so the two can never disagree about whether a run failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) pass: usize,
    pub(crate) warn: usize,
    pub(crate) fail: usize,
}

impl Tally {
    fn add_checks(&mut self, checks: &[Check]) {
        for c in checks {
            match c.status {
                CheckStatus::Pass => self.pass += 1,
                CheckStatus::Warn => self.warn += 1,
                CheckStatus::Fail => self.fail += 1,
            }
        }
    }

    fn add_section(&mut self, section: &Section) {
        if let Section::Checked(checks) = section {
            self.add_checks(checks);
        }
    }

    /// Record live target/remote probe results, which are computed outside the
    /// `Report` and so must be folded in by the caller.
    pub(crate) fn add_health(&mut self, ok: usize, failed: usize) {
        self.pass += ok;
        self.fail += failed;
    }

    /// Whether any check failed. Warnings are not failures.
    pub(crate) const fn has_failures(&self) -> bool {
        self.fail > 0
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "{} passed, {} warnings, {} failures",
            self.pass, self.warn, self.fail
        )
    }
}

impl Report {
    /// Tally the static checks in this report. Live target/remote probe results
    /// are not included; fold them in with [`Tally::add_health`].
    pub(crate) fn tally(&self) -> Tally {
        let mut tally = Tally::default();
        tally.add_checks(&self.structure);
        tally.add_section(&self.config);
        tally.add_section(&self.store_consistency);
        tally.add_section(&self.secrets_health);
        tally
    }
}
