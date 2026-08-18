//! Read-back verification: what a target actually holds, not what esk sent.
//!
//! The deploy index records esk's own claim about a write ([`crate::deploy_tracker`]).
//! A claim written by the command whose success it certifies cannot disagree
//! with that command, so this module exists to obtain independent evidence.
//!
//! The central rule is that **targets never produce verdicts**. A target
//! returns [`Evidence`] — the keys and values it read back — and [`compare`]
//! turns that into verdicts here. Targets are handed key *names* only, never
//! the values esk expects, so an implementation that returns something it did
//! not actually read has nothing to fabricate toward: its answer mismatches and
//! surfaces as drift. Honesty is a property of the signature rather than of
//! reviewer discipline.
//!
//! Fidelity tiers are permanent, not transitional. Roughly half of esk's
//! targets can return stored values; several can only list key names; secrets
//! written to Docker Swarm cannot be read back at all. Reporting must therefore
//! express partial knowledge forever, which is why [`Tally`] has no single
//! "passed" count to collapse into.

use std::collections::{BTreeMap, BTreeSet};

use zeroize::Zeroizing;

/// What a target can prove about the secrets it holds.
///
/// Declared by a target, but never trusted on its own: [`compare`] checks the
/// declaration against the evidence actually returned and refuses to issue
/// value verdicts to a target that did not supply values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// The target returns stored values, so drift can be detected exactly.
    Value,
    /// The target lists key names but never their values.
    Presence,
    /// The target cannot be read back at all.
    None,
}

impl Fidelity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Presence => "presence",
            Self::None => "none",
        }
    }
}

/// Raw observation returned by a target. The only thing a target may construct.
///
/// There is deliberately no variant meaning "everything is fine": absence of
/// evidence is [`Evidence::Unreadable`], which is reported as a gap rather than
/// as a pass.
pub enum Evidence {
    /// Keys and values read back from the target.
    ///
    /// Keys absent from this map are treated as missing from the target, so an
    /// implementation must return `Err` rather than a short map when it could
    /// not enumerate everything — a paginated listing it did not exhaust, or a
    /// read its credentials only partly permitted. A truncated map is reported
    /// as drift the operator cannot act on, which is worse than admitting the
    /// read failed.
    Values(BTreeMap<String, Zeroizing<String>>),
    /// Key names read back from the target, without values.
    ///
    /// `note` carries provider-side detail for display, such as a digest the
    /// provider computes over its own copy. It is never compared: esk's own
    /// hashes are HMAC-keyed (see [`crate::deploy_tracker::DeployIndex::hash_value`]),
    /// so equality with any provider digest is impossible by construction.
    Names {
        present: BTreeSet<String>,
        note: Option<String>,
    },
    /// This target cannot be read back. The reason is a fixed, value-free string.
    Unreadable(&'static str),
}

/// Per-key verdict for a [`Fidelity::Value`] target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueVerdict {
    /// The target holds exactly what the store holds.
    Matches,
    /// The target holds a different value.
    Differs,
    /// The target does not hold this key.
    Missing,
}

/// Per-key verdict for a [`Fidelity::Presence`] target.
///
/// There is no "matches" variant, and that is the point: a target that cannot
/// return values must be unable to claim a value matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceVerdict {
    /// The key exists on the target. Its value was not checked.
    Present,
    /// The key does not exist on the target.
    Missing,
}

/// The verified state of one target in one environment.
pub enum Findings {
    /// A value-fidelity target was read successfully.
    Values {
        verdicts: BTreeMap<String, ValueVerdict>,
        /// Keys the target holds that esk does not manage here.
        extra: Vec<String>,
    },
    /// A presence-fidelity target was read successfully. Values are unchecked.
    Presence {
        verdicts: BTreeMap<String, PresenceVerdict>,
        extra: Vec<String>,
        note: Option<String>,
    },
    /// The target cannot be read back. A permanent property, not a failure.
    Unverifiable { reason: &'static str },
    /// The target could not be reached, or its read failed.
    ///
    /// Distinct from every other variant on purpose. Collapsing "could not
    /// look" into "looks fine" is exactly the defect this module exists to
    /// prevent, so this can never be counted as a pass.
    Unreachable { error: String },
    /// A target returned evidence that does not match the fidelity it declared.
    ///
    /// Separate from [`Findings::Unreachable`] because the response differs: a
    /// timeout is worth retrying, whereas this is a bug in the target's
    /// implementation and needs fixing. Keeping the distinction in the type
    /// rather than in a message is what lets a caller act on it.
    Malformed { declared: Fidelity },
}

impl Findings {
    /// Whether esk actually established this scope's state.
    ///
    /// Ask this before trusting [`Findings::observed_drift`]: an unread scope
    /// reports no drift because nothing was observed, not because it is
    /// healthy. Requiring both questions keeps "could not look" from being
    /// answered as "looks fine".
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Values { .. } | Self::Presence { .. })
    }

    /// Whether anything read back disagrees with the store.
    ///
    /// Named for what it observed rather than for the scope's health: this is
    /// `false` for a target esk could not reach, which is honest but is not a
    /// pass. See [`Findings::is_resolved`].
    pub fn observed_drift(&self) -> bool {
        match self {
            Self::Values { verdicts, extra } => {
                !extra.is_empty() || verdicts.values().any(|v| *v != ValueVerdict::Matches)
            }
            Self::Presence {
                verdicts, extra, ..
            } => !extra.is_empty() || verdicts.values().any(|v| *v != PresenceVerdict::Present),
            Self::Unverifiable { .. } | Self::Unreachable { .. } | Self::Malformed { .. } => false,
        }
    }
}

/// Turn a target's raw evidence into verdicts.
///
/// The sole producer of [`Findings::Values`] and [`Findings::Presence`], so no
/// target implementation can assert a match it did not demonstrate.
///
/// `expected` never reaches the target; it is supplied here so the comparison
/// happens in one audited place.
pub fn compare(
    fidelity: Fidelity,
    evidence: anyhow::Result<Evidence>,
    expected: &BTreeMap<String, Zeroizing<String>>,
) -> Findings {
    let evidence = match evidence {
        Ok(evidence) => evidence,
        Err(error) => {
            return Findings::Unreachable {
                error: scrub(&format!("{error:#}"), expected),
            }
        }
    };

    match (fidelity, evidence) {
        (Fidelity::Value, Evidence::Values(actual)) => {
            let verdicts = expected
                .iter()
                .map(|(key, want)| {
                    let verdict = match actual.get(key) {
                        None => ValueVerdict::Missing,
                        // Plain equality: both sides are already plaintext in
                        // this process, which has the store key, so there is no
                        // timing oracle a constant-time compare would close.
                        Some(got) if got.as_bytes() == want.as_bytes() => ValueVerdict::Matches,
                        Some(_) => ValueVerdict::Differs,
                    };
                    (key.clone(), verdict)
                })
                .collect();
            // Key names come from the provider, so they are scrubbed too: a
            // name that happens to contain a secret would otherwise reach a
            // report through a channel the error path already guards.
            let extra = actual
                .keys()
                .filter(|key| !expected.contains_key(*key))
                .map(|key| scrub(key, expected))
                .collect();
            Findings::Values { verdicts, extra }
        }
        (Fidelity::Presence, Evidence::Names { present, note }) => {
            let verdicts = expected
                .keys()
                .map(|key| {
                    let verdict = if present.contains(key) {
                        PresenceVerdict::Present
                    } else {
                        PresenceVerdict::Missing
                    };
                    (key.clone(), verdict)
                })
                .collect();
            let extra = present
                .iter()
                .filter(|key| !expected.contains_key(*key))
                .map(|key| scrub(key, expected))
                .collect();
            Findings::Presence {
                verdicts,
                extra,
                // Never compared, but it is displayed, so it is scrubbed like
                // any other provider-supplied text.
                note: note.map(|note| scrub(&note, expected)),
            }
        }
        (_, Evidence::Unreadable(reason)) => Findings::Unverifiable { reason },
        // The target declared more than it delivered. Never quietly downgrade
        // to a pass: this is a bug in the target, and it is reported as one.
        (declared, _) => Findings::Malformed { declared },
    }
}

/// Remove any secret value that a provider echoed back in an error.
fn scrub(message: &str, expected: &BTreeMap<String, Zeroizing<String>>) -> String {
    crate::targets::redact_secrets(message, expected.values().map(|v| v.as_str()))
}

/// One verified target in one environment.
pub struct ScopeReport {
    pub target: String,
    pub app: Option<String>,
    pub env: String,
    pub fidelity: Fidelity,
    pub findings: Findings,
}

impl ScopeReport {
    /// Whether this scope manages no keys at all.
    ///
    /// Such a scope is vacuously consistent with the store, which is not the
    /// same as having been verified, so it is left out of the tally entirely.
    fn is_empty(&self) -> bool {
        match &self.findings {
            Findings::Values { verdicts, extra } => verdicts.is_empty() && extra.is_empty(),
            Findings::Presence {
                verdicts, extra, ..
            } => verdicts.is_empty() && extra.is_empty(),
            _ => false,
        }
    }
}

/// Counts across a verification run.
///
/// Every scope lands in exactly one bucket and the buckets are never summed.
/// A single "passed" number is precisely the collapse this type exists to
/// prevent: it would let six unchecked targets read as six healthy ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Values read back and all matching.
    pub value_clean: usize,
    /// Values read back with at least one mismatch, absence, or extra key.
    pub value_drifted: usize,
    /// All keys present. Values were **not** checked.
    pub presence_clean: usize,
    /// At least one key missing, or an unmanaged key present.
    pub presence_drifted: usize,
    /// Cannot be read back at all.
    pub unverifiable: usize,
    /// Could not be reached, or the read failed.
    pub unreachable: usize,
    /// Returned evidence inconsistent with the fidelity it declared.
    pub malformed: usize,
}

impl Tally {
    /// Whether any scope was left unresolved, by limitation or by failure.
    ///
    /// A run with unresolved scopes is inconclusive, never clean.
    pub const fn has_gaps(&self) -> bool {
        self.unverifiable > 0 || self.unreachable > 0 || self.malformed > 0
    }

    pub const fn drifted(&self) -> usize {
        self.value_drifted + self.presence_drifted
    }
}

/// How a verification run ended.
///
/// `Inconclusive` is checked before drift: a scope esk could not read might
/// hold worse drift than the ones it could, so "could not look" outranks
/// "looked and found a problem".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every scope was read back and agreed with the store.
    Clean,
    /// No drift found, but some scopes cannot be read back at all.
    CleanWithGaps,
    /// At least one target disagrees with the store.
    Drift,
    /// At least one target could not be reached.
    Inconclusive,
}

/// A whole verification run.
///
/// Deliberately has no aggregate success field; ask [`VerifyReport::outcome`].
pub struct VerifyReport {
    pub scopes: Vec<ScopeReport>,
}

impl VerifyReport {
    pub fn tally(&self) -> Tally {
        let mut tally = Tally::default();
        for scope in &self.scopes {
            // A scope with nothing to check is not evidence of anything, so it
            // must not inflate the verified count.
            if scope.is_empty() {
                continue;
            }
            match &scope.findings {
                Findings::Values { .. } => {
                    if scope.findings.observed_drift() {
                        tally.value_drifted += 1;
                    } else {
                        tally.value_clean += 1;
                    }
                }
                Findings::Presence { .. } => {
                    if scope.findings.observed_drift() {
                        tally.presence_drifted += 1;
                    } else {
                        tally.presence_clean += 1;
                    }
                }
                Findings::Unverifiable { .. } => tally.unverifiable += 1,
                Findings::Unreachable { .. } => tally.unreachable += 1,
                Findings::Malformed { .. } => tally.malformed += 1,
            }
        }
        tally
    }

    pub fn outcome(&self) -> Outcome {
        let tally = self.tally();
        if tally.unreachable > 0 || tally.malformed > 0 {
            return Outcome::Inconclusive;
        }
        if tally.drifted() > 0 {
            return Outcome::Drift;
        }
        if tally.unverifiable > 0 {
            return Outcome::CleanWithGaps;
        }
        Outcome::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected(pairs: &[(&str, &str)]) -> BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    fn values(pairs: &[(&str, &str)]) -> Evidence {
        Evidence::Values(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
                .collect(),
        )
    }

    #[test]
    fn matching_values_are_clean() {
        let want = expected(&[("A", "1"), ("B", "2")]);
        let findings = compare(Fidelity::Value, Ok(values(&[("A", "1"), ("B", "2")])), &want);
        assert!(!findings.observed_drift());
    }

    #[test]
    fn differing_value_is_drift() {
        let want = expected(&[("A", "1")]);
        let findings = compare(Fidelity::Value, Ok(values(&[("A", "wrong")])), &want);
        assert!(findings.observed_drift());
        let Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["A"], ValueVerdict::Differs);
    }

    #[test]
    fn absent_key_is_missing_not_matching() {
        let want = expected(&[("A", "1")]);
        let findings = compare(Fidelity::Value, Ok(values(&[])), &want);
        let Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["A"], ValueVerdict::Missing);
    }

    #[test]
    fn unmanaged_key_on_target_is_reported_as_extra() {
        let want = expected(&[("A", "1")]);
        let findings = compare(
            Fidelity::Value,
            Ok(values(&[("A", "1"), ("STRAY", "x")])),
            &want,
        );
        let Findings::Values { extra, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(extra, &["STRAY".to_string()]);
        assert!(findings.observed_drift());
    }

    #[test]
    fn presence_target_cannot_report_a_value_match() {
        // The presence path produces `PresenceVerdict`, which has no variant
        // meaning "the value matched". This is the type-level guarantee.
        let want = expected(&[("A", "1")]);
        let evidence = Evidence::Names {
            present: ["A".to_string()].into_iter().collect(),
            note: None,
        };
        let findings = compare(Fidelity::Presence, Ok(evidence), &want);
        let Findings::Presence { verdicts, .. } = &findings else {
            panic!("expected presence findings");
        };
        assert_eq!(verdicts["A"], PresenceVerdict::Present);
    }

    #[test]
    fn read_failure_is_unreachable_not_a_pass() {
        let want = expected(&[("A", "1")]);
        let findings = compare(
            Fidelity::Value,
            Err(anyhow::anyhow!("connection refused")),
            &want,
        );
        assert!(matches!(findings, Findings::Unreachable { .. }));
        // An unreachable scope reports no drift, but must never be counted
        // as clean either; that distinction lives in `Tally`.
        assert!(!findings.observed_drift());
    }

    #[test]
    fn read_failure_never_leaks_a_secret_value() {
        let want = expected(&[("A", "hunter2")]);
        let findings = compare(
            Fidelity::Value,
            Err(anyhow::anyhow!("provider rejected value hunter2")),
            &want,
        );
        let Findings::Unreachable { error } = &findings else {
            panic!("expected unreachable");
        };
        assert!(!error.contains("hunter2"), "error was: {error}");
    }

    #[test]
    fn declaring_more_fidelity_than_delivered_is_not_a_pass() {
        // A presence-only target that claims Value fidelity must not receive
        // value verdicts; it is reported loudly instead.
        let want = expected(&[("A", "1")]);
        let evidence = Evidence::Names {
            present: ["A".to_string()].into_iter().collect(),
            note: None,
        };
        let findings = compare(Fidelity::Value, Ok(evidence), &want);
        assert!(
            matches!(findings, Findings::Malformed { .. }),
            "an implementation bug must be distinguishable from an unreachable target"
        );
        assert!(!findings.is_resolved(), "and must never count as resolved");
    }

    #[test]
    fn unreadable_target_is_unverifiable_not_clean() {
        let want = expected(&[("A", "1")]);
        let findings = compare(
            Fidelity::None,
            Ok(Evidence::Unreadable("write-only by design")),
            &want,
        );
        assert!(matches!(findings, Findings::Unverifiable { .. }));
    }

    fn scope(fidelity: Fidelity, findings: Findings) -> ScopeReport {
        ScopeReport {
            target: "t".to_string(),
            app: None,
            env: "dev".to_string(),
            fidelity,
            findings,
        }
    }

    #[test]
    fn unreachable_outranks_drift_in_the_outcome() {
        let report = VerifyReport {
            scopes: vec![
                scope(
                    Fidelity::Value,
                    compare(
                        Fidelity::Value,
                        Ok(values(&[("A", "wrong")])),
                        &expected(&[("A", "1")]),
                    ),
                ),
                scope(
                    Fidelity::Value,
                    Findings::Unreachable {
                        error: "timeout".to_string(),
                    },
                ),
            ],
        };
        assert_eq!(report.outcome(), Outcome::Inconclusive);
    }

    #[test]
    fn unverifiable_scopes_prevent_a_plain_clean_result() {
        let report = VerifyReport {
            scopes: vec![
                scope(
                    Fidelity::Value,
                    compare(
                        Fidelity::Value,
                        Ok(values(&[("A", "1")])),
                        &expected(&[("A", "1")]),
                    ),
                ),
                scope(
                    Fidelity::None,
                    Findings::Unverifiable {
                        reason: "write-only",
                    },
                ),
            ],
        };
        assert_eq!(report.outcome(), Outcome::CleanWithGaps);
        assert!(report.tally().has_gaps());
    }

    #[test]
    fn presence_clean_is_not_counted_as_value_verified() {
        let report = VerifyReport {
            scopes: vec![scope(
                Fidelity::Presence,
                compare(
                    Fidelity::Presence,
                    Ok(Evidence::Names {
                        present: ["A".to_string()].into_iter().collect(),
                        note: None,
                    }),
                    &expected(&[("A", "1")]),
                ),
            )],
        };
        let tally = report.tally();
        assert_eq!(tally.presence_clean, 1);
        assert_eq!(
            tally.value_clean, 0,
            "presence evidence must never be tallied as value verification"
        );
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn expected(pairs: &[(&str, &str)]) -> BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    /// An unread scope must not look healthy to a caller.
    #[test]
    fn unreachable_is_not_resolved_even_though_no_drift_was_seen() {
        let findings = Findings::Unreachable {
            error: "timeout".to_string(),
        };
        assert!(!findings.observed_drift(), "nothing was observed");
        assert!(
            !findings.is_resolved(),
            "but the scope was never established, so it is not a pass"
        );
    }

    /// A provider key name is scrubbed like any other provider-supplied text.
    #[test]
    fn extra_key_name_cannot_leak_a_secret_value() {
        let want = expected(&[("A", "hunter2")]);
        let actual = [
            ("A".to_string(), Zeroizing::new("hunter2".to_string())),
            ("hunter2".to_string(), Zeroizing::new("x".to_string())),
        ]
        .into_iter()
        .collect();
        let findings = compare(Fidelity::Value, Ok(Evidence::Values(actual)), &want);
        let Findings::Values { extra, .. } = &findings else {
            panic!("expected value findings");
        };
        assert!(
            !extra.iter().any(|key| key.contains("hunter2")),
            "extra was: {extra:?}"
        );
    }

    /// The display-only note is scrubbed too: never compared is not never shown.
    #[test]
    fn presence_note_cannot_leak_a_secret_value() {
        let want = expected(&[("A", "hunter2")]);
        let evidence = Evidence::Names {
            present: ["A".to_string()].into_iter().collect(),
            note: Some("digest of hunter2".to_string()),
        };
        let findings = compare(Fidelity::Presence, Ok(evidence), &want);
        let Findings::Presence { note, .. } = &findings else {
            panic!("expected presence findings");
        };
        let note = note.as_deref().unwrap_or_default();
        assert!(!note.contains("hunter2"), "note was: {note}");
    }

    /// A malformed response is inconclusive, never clean.
    #[test]
    fn malformed_evidence_makes_the_run_inconclusive() {
        let report = VerifyReport {
            scopes: vec![ScopeReport {
                target: "t".to_string(),
                app: None,
                env: "dev".to_string(),
                fidelity: Fidelity::Value,
                findings: Findings::Malformed {
                    declared: Fidelity::Value,
                },
            }],
        };
        assert_eq!(report.outcome(), Outcome::Inconclusive);
        assert!(report.tally().has_gaps());
    }
}

#[cfg(test)]
mod empty_scope_tests {
    use super::*;

    /// A scope managing no keys must not be counted as verified.
    #[test]
    fn scope_with_nothing_to_check_does_not_inflate_the_verified_count() {
        let report = VerifyReport {
            scopes: vec![ScopeReport {
                target: "t".to_string(),
                app: None,
                env: "dev".to_string(),
                fidelity: Fidelity::Value,
                findings: compare(
                    Fidelity::Value,
                    Ok(Evidence::Values(BTreeMap::new())),
                    &BTreeMap::new(),
                ),
            }],
        };
        let tally = report.tally();
        assert_eq!(
            tally.value_clean, 0,
            "a scope with no keys is vacuously consistent, not verified"
        );
        assert_eq!(tally.drifted(), 0);
    }
}
