use anyhow::Result;
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, ResolvedTarget};
use crate::store::SecretStore;
use crate::targets::{target_candidates, CommandRunner};
use crate::verify::{compare, Findings, ScopeReport, VerifyReport};

use super::VerifyOptions;

/// One (target, app, env) tuple.
type ScopeKey = (String, Option<String>, String);

/// What esk expects a scope to hold, and what it declares but has no value for.
#[derive(Default)]
struct Scope {
    expected: BTreeMap<String, Zeroizing<String>>,
    unset: Vec<String>,
}

/// Build the report by reading every configured scope back.
///
/// Iterates [`target_candidates`] rather than `build_targets` deliberately.
/// `build_targets` drops targets whose preflight fails, which is right for
/// deploying — you cannot write to an unreachable target — but wrong here: a
/// target that vanishes from the list contributes no scopes and so reads as
/// "no drift found". An unreachable target is precisely what verification must
/// report, so each preflight failure becomes [`Findings::Unreachable`].
pub(super) fn build(
    config: &Config,
    opts: &VerifyOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<VerifyReport> {
    // Reject an unknown filter before doing any work. A typo would otherwise
    // select no scopes at all, and a run with nothing in it has no bucket to
    // land in — it reports as clean, which is the exact "esk never looked but
    // said it was fine" failure this command exists to prevent. Every other
    // filtered command validates its `--env` the same way.
    if let Some(env) = opts.env {
        config.validate_env(env)?;
    }
    if let Some(name) = opts.target {
        let names = config.target_names();
        if !names.contains(&name) {
            anyhow::bail!("{}", crate::suggest::unknown_target(name, &names));
        }
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let resolved = config.resolve_secrets()?;

    // Group the store's values by the scope that should hold them. Built from
    // config and the store only: what the deploy index claims esk sent is
    // deliberately not an input, since the index is the very artifact whose
    // honesty this command exists to check.
    let mut scopes: BTreeMap<ScopeKey, Scope> = BTreeMap::new();
    for secret in &resolved {
        for target in &secret.targets {
            if !matches_filter(target, opts) {
                continue;
            }
            let entry = scopes
                .entry((
                    target.service.clone(),
                    target.app.clone(),
                    target.environment.clone(),
                ))
                .or_default();
            match payload
                .secrets
                .get(&format!("{}:{}", secret.key, target.environment))
            {
                Some(value) => {
                    entry
                        .expected
                        .insert(secret.key.clone(), Zeroizing::new(value.clone()));
                }
                // Declared here but never given a value. There is nothing to
                // compare, so it gets no verdict — but it is recorded, because
                // dropping it silently would let a scope esk only partly knows
                // about report as fully verified.
                None => entry.unset.push(secret.key.clone()),
            }
        }
    }

    let candidates = target_candidates(config, runner);
    let mut report = VerifyReport { scopes: Vec::new() };

    // Preflight is a property of the target, not of the scope: it checks that a
    // CLI is installed and authenticated, which cannot differ between two
    // (app, env) pairs of the same service. Running it per scope would shell
    // out once per pair — a config with three apps across three environments
    // would run `wrangler whoami` nine times for one answer. Memoized per
    // target so the cost matches every other consumer's.
    //
    // The failure is cached alongside the success, so a target whose CLI is not
    // authenticated reports the same actionable message on each of its scopes
    // rather than re-paying for it.
    let mut preflight_results: BTreeMap<String, Result<(), String>> = BTreeMap::new();

    for ((service, app, env), Scope { expected, unset }) in scopes {
        let Some(candidate) = candidates.iter().find(|c| c.target.name() == service) else {
            // Configured as a secret's target but not as a configured target.
            // `config` validation rejects this, so reaching it means the two
            // disagree; report it rather than silently dropping the scope.
            report.scopes.push(ScopeReport {
                target: service.clone(),
                app,
                env,
                fidelity: crate::verify::Fidelity::None,
                findings: Findings::Unreachable {
                    error: format!("'{service}' is not a configured target"),
                },
                unset,
            });
            continue;
        };

        let target = &candidate.target;
        let fidelity = target.verify_fidelity();
        let resolved_target = ResolvedTarget {
            service: service.clone(),
            app: app.clone(),
            environment: env.clone(),
        };

        // Only the key names cross this boundary. `expected` stays here.
        let keys: BTreeSet<String> = expected.keys().cloned().collect();

        // Preflight before reading: without it, an unauthenticated CLI's error
        // arrives as an opaque read failure rather than as the actionable
        // "run `wrangler login`" message preflight produces.
        let preflight = preflight_results
            .entry(service.clone())
            .or_insert_with(|| target.preflight().map_err(|error| format!("{error:#}")));

        let evidence = match preflight {
            Ok(()) => target.read_back(&keys, &resolved_target),
            Err(error) => Err(anyhow::anyhow!(error.clone())),
        };

        report.scopes.push(ScopeReport {
            target: service,
            app,
            env,
            fidelity,
            findings: compare(fidelity, evidence, &expected),
            unset,
        });
    }

    Ok(report)
}

fn matches_filter(target: &ResolvedTarget, opts: &VerifyOptions<'_>) -> bool {
    opts.env.is_none_or(|env| target.environment == env)
        && opts.target.is_none_or(|name| target.service == name)
}
