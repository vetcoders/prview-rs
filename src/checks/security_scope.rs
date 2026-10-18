//! Requested security mode, preflight eligibility, and recorded execution are
//! separate facts. This projection never changes eligibility or policy.

use super::{
    CheckEligibility, CheckResult, CheckStatus, SkippedCheck, get_checks_for_profile,
    is_security_check,
};
use crate::config::Config;
use serde_json::{Value, json};

pub(crate) fn requested_mode(config: &Config) -> &'static str {
    if config.skip_security {
        "disabled"
    } else if config.run_security {
        "extended"
    } else {
        "standard"
    }
}

pub(crate) fn preflight(config: &Config) -> String {
    let checks = get_checks_for_profile(config)
        .into_iter()
        .filter(|check| is_security_check(check.name()))
        .map(|check| (check.name().to_owned(), check.check_eligibility(config)))
        .collect::<Vec<_>>();
    describe_plan(requested_mode(config), &checks)
}

fn describe_plan(requested: &str, checks: &[(String, CheckEligibility)]) -> String {
    let details = checks
        .iter()
        .map(|(name, eligibility)| match eligibility {
            CheckEligibility::Run => format!("{name}: eligible"),
            CheckEligibility::Skip(reason) => format!("{name}: skipped ({reason})"),
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("requested={requested}; {details}; execution recorded after checks")
}

/// Only observed rows, never a second `which` lookup after the tools ran.
/// In particular a cached pass is not a fresh scan, and an error is not proof
/// that a tool spawned. Omitted rows remain unknown rather than a clean scan.
pub(crate) fn recorded(config: &Config, checks: &[CheckResult], skipped: &[SkippedCheck]) -> Value {
    let mut rows = Vec::new();
    for check in checks.iter().filter(|check| is_security_check(&check.name)) {
        let execution = if check.cached {
            "cached"
        } else {
            match check.status {
                CheckStatus::Skipped => "skipped_at_runtime",
                CheckStatus::Error => "error",
                _ => "executed",
            }
        };
        rows.push(json!({
            "id": crate::check_id::check_id_from_name(&check.name),
            "name": check.name,
            "execution": execution,
            "status": check.status.as_str(),
            "reason": matches!(check.status, CheckStatus::Skipped | CheckStatus::Error).then_some(&check.output),
        }));
    }
    for check in skipped
        .iter()
        .filter(|check| is_security_check(&check.name))
    {
        rows.push(json!({ "id": check.id, "name": check.name, "execution": "not_executed", "status": "skipped", "reason": check.reason }));
    }
    json!({ "requested": requested_mode(config), "source": "recorded_check_results", "checks": rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use std::time::Duration;

    #[test]
    fn dogfood_security_preflight_distinguishes_requested_from_eligible() {
        let plan = describe_plan(
            "extended",
            &[(
                "Semgrep scan".into(),
                CheckEligibility::Skip("semgrep not available".into()),
            )],
        );
        assert!(plan.contains("requested=extended"));
        assert!(plan.contains("skipped (semgrep not available)"));
        assert!(!plan.contains("security+"));
        let eligible = describe_plan(
            "standard",
            &[("Semgrep scan".into(), CheckEligibility::Run)],
        );
        assert!(eligible.contains("eligible"));
        assert!(!eligible.contains("executed"));
    }

    #[test]
    fn dogfood_security_scope_keeps_skip_cache_and_error_distinct() {
        let mut config = test_config();
        config.run_security = true;
        let skipped = vec![SkippedCheck {
            id: "semgrep_scan".into(),
            name: "Semgrep scan".into(),
            reason: "semgrep not available".into(),
        }];
        let value = recorded(&config, &[], &skipped);
        assert_eq!(value["requested"], "extended");
        assert_eq!(value["checks"][0]["execution"], "not_executed");
        for (cached, status, execution) in [
            (true, CheckStatus::Passed, "cached"),
            (false, CheckStatus::Passed, "executed"),
            (false, CheckStatus::Error, "error"),
            (false, CheckStatus::Skipped, "skipped_at_runtime"),
        ] {
            let check = CheckResult {
                name: "Semgrep scan".into(),
                status,
                cached,
                duration: Duration::ZERO,
                output: "observed output".into(),
                provenance: None,
            };
            assert_eq!(
                recorded(&config, &[check], &[])["checks"][0]["execution"],
                execution
            );
        }
        config.skip_security = true;
        assert_eq!(recorded(&config, &[], &[])["requested"], "disabled");
        assert!(
            recorded(&config, &[], &[])["checks"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
