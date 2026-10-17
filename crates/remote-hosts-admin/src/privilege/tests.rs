use super::*;
use crate::protocol::{PROTOCOL, Plan, Snapshot};

fn evidence(platform: Platform, identity: Identity) -> Evidence {
    Evidence {
        platform,
        identity,
        no_new_privileges: Some(false),
        helper: Helper::NotInstalled,
        grant: Grant::NotChecked,
    }
}
fn request() -> PlanRequest {
    PlanRequest {
        request_id: "d84bfb53-1fc4-4075-940f-557db1d6cb08".into(),
        device_id: "933a488a-152e-44d3-8409-4b91733986c3".into(),
        action: Action::ExactSourceDnsRepair,
        expected_policy_sha256: "a".repeat(64),
        expected_target_sha256: "b".repeat(64),
    }
}
fn receipt(state: State) -> Record {
    let r = request();
    Record {
        plan: Plan {
            protocol: PROTOCOL,
            action: crate::protocol::ACTION.into(),
            request_id: r.request_id,
            device_id: r.device_id,
            caller_uid: 1000,
            policy_sha256: "a".repeat(64),
            created_at: 1,
            expires_at: 301,
            snapshot: Snapshot::default(),
        },
        plan_sha256: "c".repeat(64),
        state,
        steps: vec![],
        error: None,
        after: None,
        updated_at: 1,
    }
}
#[test]
fn ho5_nnp_and_missing_helper_are_separate_from_unknown_grant() {
    let mut e = evidence(Platform::Linux, Identity::OrdinaryUser);
    e.no_new_privileges = Some(true);
    let c = discover(&e);
    assert!(c.reasons.contains(&Reason::NoNewPrivilegesBlocksSudo));
    assert!(c.reasons.contains(&Reason::HelperNotInstalled));
    assert!(c.reasons.contains(&Reason::GrantNotVerified));
    assert!(!c.reasons.contains(&Reason::GrantNotGranted));
    assert!(!c.execution_available);
}
#[test]
fn malformed_or_missing_nnp_is_unknown() {
    assert_eq!(
        parse_no_new_privileges("Uid:\t1000\nNoNewPrivs:\t1\n"),
        Some(true)
    );
    assert_eq!(parse_no_new_privileges("NoNewPrivs: 0\n"), Some(false));
    for input in [
        "",
        "NoNewPrivs: 2",
        "NoNewPrivs: 0\nNoNewPrivs: 1",
        "NoNewPrivs: 1junk",
    ] {
        assert_eq!(parse_no_new_privileges(input), None);
    }
    let mut e = evidence(Platform::Linux, Identity::OrdinaryUser);
    e.no_new_privileges = None;
    assert!(
        discover(&e)
            .reasons
            .contains(&Reason::NoNewPrivilegesUnknown)
    );
}
#[test]
fn android_app_shell_and_root_have_distinct_backends_without_promising_elevation() {
    for (identity, backend, reason) in [
        (
            Identity::OrdinaryUser,
            "android_application",
            Some(Reason::AndroidAppSandbox),
        ),
        (
            Identity::AdbShell,
            "android_adb_shell",
            Some(Reason::AndroidShellNotRoot),
        ),
        (Identity::Root, "android_root", None),
    ] {
        let c = discover(&evidence(Platform::Android, identity));
        assert_eq!(c.backend, backend);
        if let Some(reason) = reason {
            assert!(c.reasons.contains(&reason));
        }
        assert!(!c.execution_implemented && !c.execution_available && !c.arbitrary_root_commands);
    }
}
#[test]
fn all_platforms_stay_disabled_even_with_claimed_root_and_grant() {
    for platform in [
        Platform::Linux,
        Platform::Macos,
        Platform::Windows,
        Platform::Android,
        Platform::Unsupported,
    ] {
        let mut e = evidence(platform, Identity::Root);
        e.grant = Grant::Verified;
        e.helper = Helper::DetectedUnverified;
        let c = discover(&e);
        assert!(!c.execution_available);
        assert!(c.reasons.contains(&Reason::BackendNotImplemented));
        assert!(c.reasons.contains(&Reason::HelperIdentityUnverified));
    }
}
#[test]
fn grant_denied_and_revoked_are_explicit() {
    for (grant, reason) in [
        (Grant::NotGranted, Reason::GrantNotGranted),
        (Grant::Revoked, Reason::GrantRevoked),
    ] {
        let mut e = evidence(Platform::Macos, Identity::OrdinaryUser);
        e.grant = grant;
        assert!(discover(&e).reasons.contains(&reason));
    }
}
#[test]
fn unknown_or_running_original_never_requests_retry() {
    let c = discover(&evidence(Platform::Linux, Identity::OrdinaryUser));
    let r = receipt(State::Running);
    for (prior, unknown) in [(None, true), (Some(&r), false), (Some(&r), true)] {
        let p = plan(request(), c.clone(), prior, unknown, 7).unwrap();
        assert_eq!(p.next_action, NextAction::ObserveOriginal);
        assert!(!p.execution_enabled && !p.grants_authority);
    }
}
#[test]
fn completed_original_is_observed_and_unknown_takes_precedence() {
    let c = discover(&evidence(Platform::Linux, Identity::OrdinaryUser));
    for state in [State::Succeeded, State::Failed] {
        let r = receipt(state);
        assert_eq!(
            plan(request(), c.clone(), Some(&r), false, 7)
                .unwrap()
                .next_action,
            NextAction::ReadOriginalReceipt
        );
        assert_eq!(
            plan(request(), c.clone(), Some(&r), true, 7)
                .unwrap()
                .next_action,
            NextAction::ObserveOriginal
        );
    }
}
#[test]
fn original_receipt_cannot_be_rebound_to_another_device_or_operation() {
    let c = discover(&evidence(Platform::Linux, Identity::OrdinaryUser));
    let mut r = receipt(State::Running);
    r.plan.device_id = uuid::Uuid::nil().to_string();
    assert!(plan(request(), c.clone(), Some(&r), false, 0).is_err());
    let mut r = receipt(State::Running);
    r.plan.request_id = uuid::Uuid::nil().to_string();
    assert!(plan(request(), c, Some(&r), false, 0).is_err());
}
#[test]
fn plan_validates_closed_fields_hashes_and_canonical_identity() {
    let c = discover(&evidence(Platform::Windows, Identity::Unverified));
    for hash in ["", "AA", &"A".repeat(64), &"g".repeat(64)] {
        let mut r = request();
        r.expected_target_sha256 = hash.into();
        assert!(plan(r, c.clone(), None, false, 0).is_err());
    }
    let mut r = request();
    r.request_id = r.request_id.to_uppercase();
    assert!(plan(r, c.clone(), None, false, 0).is_err());
    let mut value = serde_json::to_value(request()).unwrap();
    value["shell"] = serde_json::json!("anything");
    assert!(serde_json::from_value::<PlanRequest>(value).is_err());
    assert!(plan(request(), c, None, false, u64::MAX).is_err());
}
#[test]
fn digest_binds_action_target_policy_device_and_observation() {
    let c = discover(&evidence(Platform::Linux, Identity::OrdinaryUser));
    let p = plan(request(), c, None, false, 10).unwrap();
    assert_eq!(p.expires_at, 10 + PLAN_TTL);
    assert_eq!(p.sha256().unwrap(), p.clone().sha256().unwrap());
    for changed in 0..5 {
        let mut other = p.clone();
        match changed {
            0 => other.request.action = Action::VerifiedAgentUpgrade,
            1 => other.request.expected_target_sha256 = "c".repeat(64),
            2 => other.request.expected_policy_sha256 = "d".repeat(64),
            3 => other.request.device_id = uuid::Uuid::nil().to_string(),
            _ => other.observed.grant = Grant::Revoked,
        }
        assert_ne!(p.sha256().unwrap(), other.sha256().unwrap());
    }
}
