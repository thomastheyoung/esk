//! `esk verify` — read back what targets actually hold.
//!
//! Separate from `deploy` on purpose. Deploy's exit code answers "did my
//! writes succeed?", which is a yes/no question. Verification answers "what is
//! actually out there?", and [`Outcome`] has four states because a scope esk
//! could not read is neither a pass nor a failure. Folding the second question
//! into deploy's boolean would collapse `Inconclusive` into one of the two, and
//! that collapse is the whole defect [`crate::verify`] exists to prevent.
//!
//! Verification is opt-in rather than part of every deploy: it roughly doubles
//! the external calls a deploy makes, and the first CI run after a fresh clone
//! sees an empty deploy index — every secret reads as never-deployed, so an
//! automatic verify would fan out to every target at once.

mod build;
mod render;

use anyhow::Result;

use crate::config::Config;
use crate::targets::{CommandRunner, RealCommandRunner};
use crate::verify::{Outcome, VerifyReport};

/// Options for a verification run.
pub struct VerifyOptions<'a> {
    /// Restrict to one environment.
    pub env: Option<&'a str>,
    /// Restrict to one target service.
    pub target: Option<&'a str>,
    /// List every scope, including those that agree with the store.
    pub all: bool,
}

/// Process exit codes.
///
/// Each [`Outcome`] gets its own code rather than mapping onto success/failure.
/// A caller scripting against `esk verify` must be able to tell "esk looked and
/// everything agreed" from "esk could not look", and a two-value space cannot
/// carry that distinction.
pub const EXIT_CLEAN: i32 = 0;
pub const EXIT_DRIFT: i32 = 3;
pub const EXIT_INCONCLUSIVE: i32 = 4;

pub fn run(config: &Config, opts: &VerifyOptions<'_>) -> Result<i32> {
    run_with_runner(config, opts, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    opts: &VerifyOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<i32> {
    let report = build::build(config, opts, runner)?;
    render::render(&report, opts.all)?;
    Ok(exit_code(&report))
}

/// Emit the verification report as stable JSON.
pub fn run_json(config: &Config, opts: &VerifyOptions<'_>) -> Result<i32> {
    run_json_with_runner(config, opts, &RealCommandRunner)
}

pub fn run_json_with_runner(
    config: &Config,
    opts: &VerifyOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<i32> {
    let report = build::build(config, opts, runner)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&render::to_json(&report))?
    );
    Ok(exit_code(&report))
}

/// Build the report without rendering it.
///
/// Exposed so tests can assert which bucket a scope landed in. The exit code
/// alone cannot distinguish "read back and matching" from "never checked" —
/// both exit 0 — so a test that only asserts the code cannot tell whether a
/// target was verified or merely silent.
///
/// `pub` rather than `#[cfg(test)]` because the integration suite links this
/// crate as an external consumer. Hidden from the docs; it exposes no secret
/// values, since `ScopeReport` carries verdicts rather than values.
#[doc(hidden)]
pub fn report_for_test(
    config: &Config,
    opts: &VerifyOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<VerifyReport> {
    build::build(config, opts, runner)
}

/// The JSON form of a report, for tests that assert on its shape.
#[doc(hidden)]
pub fn to_json_for_test(report: &VerifyReport) -> serde_json::Value {
    render::to_json(report)
}

/// Map an outcome to a process exit code.
///
/// `CleanWithGaps` exits 0: esk found no disagreement, and the gaps it could
/// not read are a permanent property of those targets, not a new problem. The
/// gaps are always named in the output, so a clean exit never implies full
/// coverage.
fn exit_code(report: &VerifyReport) -> i32 {
    match report.outcome() {
        Outcome::Clean | Outcome::CleanWithGaps => EXIT_CLEAN,
        Outcome::Drift => EXIT_DRIFT,
        Outcome::Inconclusive => EXIT_INCONCLUSIVE,
    }
}
