use anyhow::Result;
use console::style;

use std::fmt::Write;

use crate::verify::{
    Findings, Outcome, PresenceVerdict, ScopeReport, Tally, ValueVerdict, VerifyReport,
};

/// Render the report.
///
/// The summary never states a single total. `Tally` has six mutually exclusive
/// buckets precisely so "3 verified, 5 presence-only, 1 write-only, 2
/// unreachable" cannot be printed as "11 passed", and the renderer must not
/// undo that in the format string. Every bucket that is non-zero is named, in
/// its own vocabulary.
pub(super) fn render(report: &VerifyReport, all: bool) -> Result<()> {
    let tally = report.tally();
    let outcome = report.outcome();

    cliclack::intro(style(" esk verify ").on_cyan().black())?;

    if report.scopes.is_empty() {
        cliclack::log::info("No configured scopes matched.")?;
        cliclack::outro("Nothing to verify")?;
        return Ok(());
    }

    for scope in &report.scopes {
        render_scope(scope, all)?;
    }

    cliclack::log::info(summary_line(&tally))?;

    // The closing line states what esk established, never a bare pass. Even
    // `Clean` names how many scopes were actually read, so a run that verified
    // nothing cannot read as a run that verified everything.
    let outro = match outcome {
        Outcome::Clean => format!(
            "{} {} scope{} read back and matching",
            style("✔").green(),
            tally.verified(),
            plural(tally.verified()),
        ),
        Outcome::CleanWithGaps => format!(
            "{} no drift found, but {} scope{} could not be checked",
            style("○").yellow(),
            unresolved_count(&tally),
            plural(unresolved_count(&tally)),
        ),
        Outcome::Drift => format!(
            "{} {} scope{} disagree with the store",
            style("✖").red(),
            tally.drifted(),
            plural(tally.drifted()),
        ),
        Outcome::Inconclusive => format!(
            "{} could not determine the state of {} scope{}",
            style("!").yellow(),
            tally.unreachable + tally.malformed,
            plural(tally.unreachable + tally.malformed),
        ),
    };
    cliclack::outro(outro)?;
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Scopes esk did not establish the state of.
///
/// Excludes `skipped`, which counts scopes that manage no keys: those are
/// described as "nothing to check" in the summary, and calling the same scopes
/// "could not be checked" in the outro would give one bucket two meanings.
fn unresolved_count(tally: &Tally) -> usize {
    tally.unverifiable + tally.unreachable + tally.malformed
}

/// Name every non-empty bucket. Buckets are never summed into one figure.
fn summary_line(tally: &Tally) -> String {
    let parts: Vec<String> = [
        ("verified", tally.value_clean),
        ("drifted", tally.value_drifted),
        ("present (values unchecked)", tally.presence_clean),
        ("presence drift", tally.presence_drifted),
        ("write-only", tally.unverifiable),
        ("unreachable", tally.unreachable),
        ("malformed", tally.malformed),
        ("nothing to check", tally.skipped),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(label, count)| format!("{count} {label}"))
    .collect();

    if parts.is_empty() {
        "no scopes verified".to_string()
    } else {
        parts.join(" · ")
    }
}

fn scope_label(scope: &ScopeReport) -> String {
    match &scope.app {
        Some(app) => format!("{} {app}:{}", scope.target, scope.env),
        None => format!("{} {}", scope.target, scope.env),
    }
}

fn render_scope(scope: &ScopeReport, all: bool) -> Result<()> {
    let label = scope_label(scope);

    match &scope.findings {
        Findings::Values { verdicts, extra } => {
            let bad: Vec<&String> = verdicts
                .iter()
                .filter(|(_, v)| **v != ValueVerdict::Matches)
                .map(|(k, _)| k)
                .collect();
            if bad.is_empty() && extra.is_empty() {
                if all {
                    cliclack::log::success(format!(
                        "{label}  {} key{} read back and matching",
                        verdicts.len(),
                        plural(verdicts.len())
                    ))?;
                }
                return Ok(());
            }
            let mut lines = vec![format!("{label}")];
            for (key, verdict) in verdicts {
                // Values are never printed, only the verdict about them.
                let note = match verdict {
                    ValueVerdict::Matches => continue,
                    ValueVerdict::Differs => "differs from the store",
                    ValueVerdict::Missing => "missing from the target",
                };
                lines.push(format!("  {} {key} — {note}", style("✖").red()));
            }
            for key in extra {
                lines.push(format!(
                    "  {} {key} — on the target but not managed by esk",
                    style("?").yellow()
                ));
            }
            cliclack::log::error(lines.join("\n"))?;
        }
        Findings::Presence {
            verdicts,
            extra,
            note,
        } => {
            let missing: Vec<&String> = verdicts
                .iter()
                .filter(|(_, v)| **v != PresenceVerdict::Present)
                .map(|(k, _)| k)
                .collect();
            // Always says "values unchecked", including on the clean path. A
            // presence target cannot tell you a value is right, and the output
            // must not let a reader believe otherwise.
            if missing.is_empty() && extra.is_empty() {
                if all {
                    let mut line = format!(
                        "{label}  {} key{} present, values unchecked",
                        verdicts.len(),
                        plural(verdicts.len())
                    );
                    if let Some(note) = note {
                        let _ = write!(line, " ({note})");
                    }
                    cliclack::log::info(line)?;
                }
                return Ok(());
            }
            let mut lines = vec![format!("{label}  (values unchecked)")];
            for key in missing {
                lines.push(format!(
                    "  {} {key} — missing from the target",
                    style("✖").red()
                ));
            }
            for key in extra {
                lines.push(format!(
                    "  {} {key} — on the target but not managed by esk",
                    style("?").yellow()
                ));
            }
            cliclack::log::error(lines.join("\n"))?;
        }
        Findings::Unverifiable { reason } => {
            cliclack::log::info(format!(
                "{label}  {}",
                style(format!("not checkable — {reason}")).dim()
            ))?;
        }
        Findings::Unreachable { error } => {
            cliclack::log::warning(format!("{label}  could not be read — {error}"))?;
        }
        Findings::Malformed { declared, received } => {
            cliclack::log::warning(format!(
                "{label}  target bug: declared {} fidelity but returned {}",
                declared.as_str(),
                shape_str(*received),
            ))?;
        }
    }
    Ok(())
}

const fn shape_str(shape: crate::verify::EvidenceShape) -> &'static str {
    match shape {
        crate::verify::EvidenceShape::Values => "values",
        crate::verify::EvidenceShape::Names => "names only",
        crate::verify::EvidenceShape::Unreadable => "nothing",
    }
}

/// Stable JSON form.
///
/// Mirrors the text path's refusal to aggregate: the tally is emitted as its
/// separate buckets, and there is no `passed` or `ok` field for a consumer to
/// key off. `outcome` is the one field that summarizes, and it has four values.
pub(super) fn to_json(report: &VerifyReport) -> serde_json::Value {
    let tally = report.tally();
    let scopes: Vec<serde_json::Value> = report.scopes.iter().map(scope_json).collect();

    serde_json::json!({
        "outcome": outcome_str(report.outcome()),
        "tally": {
            "value_clean": tally.value_clean,
            "value_drifted": tally.value_drifted,
            "presence_clean": tally.presence_clean,
            "presence_drifted": tally.presence_drifted,
            "unverifiable": tally.unverifiable,
            "unreachable": tally.unreachable,
            "malformed": tally.malformed,
            "skipped": tally.skipped,
            "has_gaps": tally.has_gaps(),
        },
        "scopes": scopes,
    })
}

const fn outcome_str(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Clean => "clean",
        Outcome::CleanWithGaps => "clean_with_gaps",
        Outcome::Drift => "drift",
        Outcome::Inconclusive => "inconclusive",
    }
}

fn scope_json(scope: &ScopeReport) -> serde_json::Value {
    let mut value = serde_json::json!({
        "target": scope.target,
        "app": scope.app,
        "environment": scope.env,
        "fidelity": scope.fidelity.as_str(),
    });

    // Never emits a value, only a verdict about one.
    let detail = match &scope.findings {
        Findings::Values { verdicts, extra } => serde_json::json!({
            "state": "values",
            "keys": verdicts.iter().map(|(key, verdict)| serde_json::json!({
                "key": key,
                "verdict": match verdict {
                    ValueVerdict::Matches => "matches",
                    ValueVerdict::Differs => "differs",
                    ValueVerdict::Missing => "missing",
                },
            })).collect::<Vec<_>>(),
            "unmanaged_keys": extra,
        }),
        Findings::Presence {
            verdicts,
            extra,
            note,
        } => serde_json::json!({
            "state": "presence",
            "values_checked": false,
            "keys": verdicts.iter().map(|(key, verdict)| serde_json::json!({
                "key": key,
                "verdict": match verdict {
                    PresenceVerdict::Present => "present",
                    PresenceVerdict::Missing => "missing",
                },
            })).collect::<Vec<_>>(),
            "unmanaged_keys": extra,
            "note": note,
        }),
        Findings::Unverifiable { reason } => serde_json::json!({
            "state": "unverifiable",
            "reason": reason,
        }),
        Findings::Unreachable { error } => serde_json::json!({
            "state": "unreachable",
            "error": error,
        }),
        Findings::Malformed { declared, received } => serde_json::json!({
            "state": "malformed",
            "declared": declared.as_str(),
            "received": shape_str(*received),
        }),
    };
    value["findings"] = detail;
    value
}
