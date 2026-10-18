use super::*;

fn frame(p: &Policy) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&request(p)).unwrap();
    bytes.push(b'\n');
    bytes
}
fn rejects_before_collection(input: crate::windows_caller::AssessmentInput<'_>, bytes: &[u8]) {
    let called = std::cell::Cell::new(false);
    let result = crate::windows_caller::assess_with_collector(input, bytes, || {
        called.set(true);
        anyhow::bail!("collector should not run")
    });
    assert!(result.is_err());
    assert!(!called.get());
}
#[test]
fn malformed_unterminated_multiple_and_claimed_identity_frames_never_collect() {
    let p = policy();
    let valid = frame(&p);
    let mut without_newline = valid.clone();
    without_newline.pop();
    let mut double = valid.clone();
    double.extend_from_slice(&valid);
    let mut crlf = valid.clone();
    crlf.insert(crlf.len() - 1, b'\r');
    let mut unknown = serde_json::to_value(request(&p)).unwrap();
    unknown["caller_sid"] = serde_json::json!("S-1-5-18");
    let mut claimed = serde_json::to_vec(&unknown).unwrap();
    claimed.push(b'\n');
    let duplicate = format!(
        "{{\"request_id\":\"{}\",{}",
        REQUEST,
        std::str::from_utf8(&valid).unwrap().trim_start_matches('{')
    )
    .into_bytes();
    for bytes in [
        vec![],
        without_newline,
        double,
        crlf,
        claimed,
        duplicate,
        vec![0xff, b'\n'],
        b"{} {}\n".to_vec(),
    ] {
        rejects_before_collection(native_input(&p, observation(&p, &request(&p))), &bytes);
    }
}
#[test]
fn frame_binds_original_operation_device_action_policy_and_target_before_collection() {
    let p = policy();
    for i in 0..5 {
        let mut changed = request(&p);
        match i {
            0 => changed.request_id = uuid::Uuid::nil().to_string(),
            1 => changed.device_id = uuid::Uuid::nil().to_string(),
            2 => changed.action = Action::ExactSourceDnsRepair,
            3 => changed.expected_policy_sha256 = "c".repeat(64),
            _ => changed.expected_target_sha256 = "c".repeat(64),
        }
        let mut bytes = serde_json::to_vec(&changed).unwrap();
        bytes.push(b'\n');
        let mut o = observation(&p, &request(&p));
        // Even a matching self-reported digest cannot override the expected plan.
        o.frame_request_sha256 = digest(&changed).unwrap();
        rejects_before_collection(native_input(&p, o), &bytes);
    }
}
#[test]
fn noncanonical_ids_and_malformed_digests_never_reach_native_collection() {
    let p = policy();
    for i in 0..3 {
        let mut changed = request(&p);
        match i {
            0 => changed.request_id = REQUEST.to_uppercase(),
            1 => changed.device_id = "not-a-device".into(),
            _ => changed.expected_target_sha256 = "B".repeat(64),
        }
        let mut bytes = serde_json::to_vec(&changed).unwrap();
        bytes.push(b'\n');
        let mut input = native_input(&p, observation(&p, &request(&p)));
        input.observation.frame_request_sha256 = digest(&changed).unwrap();
        input.request = changed;
        rejects_before_collection(input, &bytes);
    }
}
#[test]
fn mismatched_frame_observation_is_rejected_rather_than_overwritten() {
    let p = policy();
    let mut o = observation(&p, &request(&p));
    o.frame_request_sha256 = "0".repeat(64);
    rejects_before_collection(native_input(&p, o), &frame(&p));
}
#[test]
fn exact_frame_bytes_are_bound_without_becoming_peer_or_grant_authority() {
    let p = policy();
    let o = observation(&p, &request(&p));
    let kernel = o.caller.clone().unwrap();
    let first = frame(&p);
    let mut second = first.clone();
    second.insert(second.len() - 1, b' ');
    let a =
        crate::windows_caller::assess_with_collector(native_input(&p, o.clone()), &first, || {
            Ok(kernel.clone())
        })
        .unwrap();
    let b =
        crate::windows_caller::assess_with_collector(native_input(&p, o), &second, || Ok(kernel))
            .unwrap();
    assert!(a.offline_policy_checks_passed && b.offline_policy_checks_passed);
    assert_ne!(a.frame_sha256, b.frame_sha256);
    assert_ne!(a.binding_sha256, b.binding_sha256);
    assert!(!a.native_peer_authenticated && !a.grants_authority && !a.plan.execution_enabled);
    assert_eq!(a.plan.request.request_id, REQUEST);
    assert_eq!(
        a.plan.expires_at,
        a.plan.created_at + crate::protocol::PLAN_TTL
    );
}
#[test]
fn maximum_frame_is_inclusive_and_overflow_never_queries_identity() {
    let p = policy();
    let o = observation(&p, &request(&p));
    let kernel = o.caller.clone().unwrap();
    let mut bytes = frame(&p);
    bytes.pop();
    bytes.resize(crate::protocol::MAX_FRAME - 1, b' ');
    bytes.push(b'\n');
    let a =
        crate::windows_caller::assess_with_collector(native_input(&p, o.clone()), &bytes, || {
            Ok(kernel)
        })
        .unwrap();
    assert!(a.offline_policy_checks_passed);
    bytes.insert(bytes.len() - 1, b' ');
    rejects_before_collection(native_input(&p, o), &bytes);
}
#[test]
fn framed_request_never_mixes_another_logon_session() {
    let p = policy();
    let o = observation(&p, &request(&p));
    let mut kernel = o.caller.clone().unwrap();
    kernel.logon_sid = sid("S-1-5-5-40-51");
    let a = crate::windows_caller::assess_with_collector(native_input(&p, o), &frame(&p), || {
        Ok(kernel)
    })
    .unwrap();
    assert!(!a.offline_policy_checks_passed && a.denials.contains(&Denial::CallerIdentity));
}
#[test]
fn framed_request_preserves_revocation_original_unknown_and_closed_receipts() {
    let p = policy();
    let o = observation(&p, &request(&p));
    let kernel = o.caller.clone().unwrap();
    for state in [
        State::Running,
        State::ClosedUnknown,
        State::Succeeded,
        State::Failed,
    ] {
        let record = original(state);
        let before = serde_json::to_vec(&record).unwrap();
        let mut input = native_input(&p, o.clone());
        input.previous = Some(&record);
        input.grant = Grant::Revoked;
        let a =
            crate::windows_caller::assess_with_collector(input, &frame(&p), || Ok(kernel.clone()))
                .unwrap();
        assert!(!a.offline_policy_checks_passed && a.denials.contains(&Denial::GrantRevoked));
        assert_eq!(a.plan.observed.grant, Grant::Revoked);
        assert_eq!(
            a.plan.next_action,
            if matches!(state, State::Running | State::ClosedUnknown) {
                NextAction::ObserveOriginal
            } else {
                NextAction::ReadOriginalReceipt
            }
        );
        assert_eq!(serde_json::to_vec(&record).unwrap(), before);
    }
    let mut input = native_input(&p, o.clone());
    input.outcome_unknown = true;
    let a = crate::windows_caller::assess_with_collector(input, &frame(&p), || Ok(kernel.clone()))
        .unwrap();
    assert_eq!(a.plan.next_action, NextAction::ObserveOriginal);
    assert!(!a.offline_policy_checks_passed);
    let mut input = native_input(&p, o);
    input.now = u64::MAX;
    assert!(
        crate::windows_caller::assess_with_collector(input, &frame(&p), || Ok(kernel)).is_err()
    );
}

fn native_input<'a>(p: &'a Policy, o: Observation) -> crate::windows_caller::AssessmentInput<'a> {
    crate::windows_caller::AssessmentInput {
        policy: p,
        request: request(p),
        observation: o,
        grant: Grant::Verified,
        previous: None,
        outcome_unknown: false,
        now: 10,
    }
}
#[test]
fn native_adapter_replaces_claimed_matching_identity_with_collected_mismatch() {
    let p = policy();
    let o = observation(&p, &request(&p));
    let mut kernel = o.caller.clone().unwrap();
    kernel.user_sid = sid("S-1-5-21-10-20-30-1002");
    let a = crate::windows_caller::assess_with_collector(native_input(&p, o), &frame(&p), || {
        Ok(kernel)
    })
    .unwrap();
    assert!(!a.offline_policy_checks_passed);
    assert!(a.denials.contains(&Denial::CallerIdentity));
    assert!(!a.native_peer_authenticated && !a.grants_authority);
}
#[test]
fn native_adapter_query_failure_never_uses_claimed_identity_or_success_flags() {
    let p = policy();
    let o = observation(&p, &request(&p));
    assert!(evaluate(&p, &o).offline_policy_checks_passed);
    let result =
        crate::windows_caller::assess_with_collector(native_input(&p, o), &frame(&p), || {
            anyhow::bail!("OpenThreadToken failed")
        });
    assert!(result.is_err());
}
#[test]
fn native_adapter_uses_collected_identity_but_never_authenticates_other_boundary_claims() {
    let p = policy();
    let mut o = observation(&p, &request(&p));
    let kernel = o.caller.clone().unwrap();
    o.caller = None;
    o.impersonation_succeeded = false;
    o.revert_succeeded = false;
    let a = crate::windows_caller::assess_with_collector(native_input(&p, o), &frame(&p), || {
        Ok(kernel)
    })
    .unwrap();
    assert!(a.offline_policy_checks_passed);
    assert!(
        !a.native_peer_authenticated && !a.grants_authority && !a.plan.observed.execution_available
    );
}
#[test]
fn obsolete_identification_policy_version_is_not_silently_migrated() {
    let mut p = policy();
    p.protocol = 1;
    assert!(p.validate().is_err());
}
use crate::protocol::{Plan, Snapshot, State};
const DEVICE: &str = "ba3bf113-2390-466e-88bc-40d5b4f02884";
const REQUEST: &str = "15ed57df-a740-4d56-935c-950d146b39ba";
fn sid(value: &str) -> Sid {
    Sid::parse(value).unwrap()
}
fn policy() -> Policy {
    Policy {
        protocol: 2,
        device_id: DEVICE.into(),
        grant_id: "3f5b1f70-e600-414f-95bf-9f86c04be0e4".into(),
        enabled: true,
        caller_sid: sid("S-1-5-21-10-20-30-1001"),
        logon_sid: sid("S-1-5-5-40-50"),
        service_sid: sid("S-1-5-80-1-2-3-4-5"),
        helper_sha256: "a".repeat(64),
        target_sha256: "b".repeat(64),
        allowed_actions: vec![Action::ManagedServiceRestart],
    }
}
fn request(p: &Policy) -> PlanRequest {
    PlanRequest {
        request_id: REQUEST.into(),
        device_id: p.device_id.clone(),
        action: Action::ManagedServiceRestart,
        expected_policy_sha256: p.sha256().unwrap(),
        expected_target_sha256: p.target_sha256.clone(),
    }
}
fn observation(p: &Policy, r: &PlanRequest) -> Observation {
    Observation {
        pipe_name: PIPE.into(),
        local_client: true,
        remote_clients_rejected: true,
        first_instance_owned: true,
        handle_inheritable: false,
        descriptor: p.expected_descriptor().unwrap(),
        scm_service_name: HELPER_SERVICE.into(),
        scm_system_service: true,
        scm_pid: 100,
        pipe_server_pid: 100,
        server_user_sid: sid(LOCAL_SYSTEM),
        server_service_sid: p.service_sid.clone(),
        server_service_sid_enabled: true,
        server_service_sid_deny_only: false,
        server_image_sha256: p.helper_sha256.clone(),
        target_service_name: TARGET_SYSTEM_SERVICE.into(),
        target_system_service: true,
        target_image_sha256: p.target_sha256.clone(),
        impersonation_succeeded: true,
        revert_succeeded: true,
        caller: Some(CallerToken {
            source: TokenSource::PipeImpersonation,
            level: ImpersonationLevel::Impersonation,
            user_sid: p.caller_sid.clone(),
            logon_sid: p.logon_sid.clone(),
            logon_group_enabled: true,
            logon_group_is_logon_id: true,
            logon_group_deny_only: false,
            app_container: false,
            restricted: false,
        }),
        frame_request_sha256: digest(r).unwrap(),
    }
}
fn evaluate(p: &Policy, o: &Observation) -> Assessment {
    assess(p, request(p), o, Grant::Verified, None, false, 10).unwrap()
}
fn denied(o: &Observation, reason: Denial) {
    let result = evaluate(&policy(), o);
    assert!(!result.offline_policy_checks_passed);
    assert!(result.denials.contains(&reason), "{:?}", result.denials);
    assert!(!result.grants_authority && !result.native_peer_authenticated);
    assert!(!result.plan.execution_enabled && !result.plan.observed.execution_available);
}
fn original(state: State) -> Record {
    let p = policy();
    let plan = Plan {
        protocol: 1,
        action: "managed_service_restart".into(),
        request_id: REQUEST.into(),
        device_id: DEVICE.into(),
        caller_uid: 1000,
        policy_sha256: p.sha256().unwrap(),
        created_at: 1,
        expires_at: 301,
        snapshot: Snapshot::default(),
    };
    Record {
        plan_sha256: digest(&plan).unwrap(),
        plan,
        state,
        steps: vec![],
        error: None,
        after: None,
        updated_at: 1,
    }
}
#[test]
fn canonical_sid_round_trip_and_malformed_input() {
    let valid = sid("S-1-5-21-4294967295-2-3-1001");
    assert_eq!(
        serde_json::from_str::<Sid>(&serde_json::to_string(&valid).unwrap()).unwrap(),
        valid
    );
    for bad in [
        "",
        "SYSTEM",
        "BA",
        "s-1-5-18",
        "S-01-5-18",
        "S-1-05-18",
        "S-1-5-018",
        "S-1-5-+18",
        "S-1-5--1",
        "S-1-5-4294967296",
        "S-1-5-18\n",
        "S-1-5-١٨",
        "S-1-5-18;D:(A;;GA;;;WD)",
    ] {
        assert!(Sid::parse(bad).is_err(), "{bad}");
    }
    assert!(Sid::parse(&format!("S-1-5-{}", "1-".repeat(100))).is_err());
}
#[test]
fn profile_excludes_groups_system_builtin_and_reserved_accounts() {
    for value in [
        "S-1-5-18",
        "S-1-5-32-544",
        "S-1-5-21-1-2-3-500",
        "S-1-5-21-1-2-3-999",
        "S-1-5-80-1-2-3-4-5",
        "S-1-5-5-1-2",
    ] {
        let mut p = policy();
        p.caller_sid = sid(value);
        assert!(p.validate().is_err());
    }
    let mut p = policy();
    p.logon_sid = p.caller_sid.clone();
    assert!(p.validate().is_err());
    let mut p = policy();
    p.service_sid = sid("S-1-5-80");
    assert!(p.validate().is_err());
}
#[test]
fn policy_is_closed_and_only_fixed_restart_profile_exists() {
    let mut v = serde_json::to_value(policy()).unwrap();
    v["shell"] = serde_json::json!("cmd.exe");
    assert!(serde_json::from_value::<Policy>(v).is_err());
    for actions in [
        vec![],
        vec![Action::VerifiedAgentUpgrade],
        vec![Action::ExactSourceDnsRepair],
        vec![Action::ManagedServiceRestart, Action::ManagedServiceRestart],
    ] {
        let mut p = policy();
        p.allowed_actions = actions;
        assert!(p.validate().is_err());
    }
    for i in 0..5 {
        let mut p = policy();
        match i {
            0 => p.protocol = 3,
            1 => p.device_id = "not_uuid".into(),
            2 => p.grant_id = "not_uuid".into(),
            3 => p.helper_sha256 = "A".repeat(64),
            _ => p.target_sha256 = "0".repeat(63),
        }
        assert!(p.validate().is_err());
    }
}
#[test]
fn passing_fixture_cannot_authenticate_peer_grant_or_enable_execution() {
    let p = policy();
    let r = request(&p);
    let o = observation(&p, &r);
    let a = evaluate(&p, &o);
    assert!(a.offline_policy_checks_passed && a.denials.is_empty());
    assert_eq!(a.plan.request.request_id, REQUEST);
    assert_eq!(a.plan.observed.platform, Platform::Windows);
    assert_eq!(a.plan.observed.helper, Helper::DetectedUnverified);
    assert_eq!(a.plan.observed.grant, Grant::NotChecked);
    assert!(!a.native_peer_authenticated && !a.grants_authority && !a.plan.grants_authority);
    assert!(
        !a.plan.execution_enabled
            && !a.plan.observed.execution_available
            && !a.plan.observed.execution_implemented
    );
    assert_eq!(a.binding_sha256, evaluate(&p, &o).binding_sha256);
    let encoded = serde_json::to_string(&a).unwrap();
    assert!(!encoded.contains("S-1-5-21-"));
}
#[test]
fn minimal_descriptor_uses_logon_sid_and_never_generic_write_or_create_instance() {
    let p = policy();
    let d = p.expected_descriptor().unwrap();
    let aces = d.dacl.unwrap();
    assert_eq!(aces.len(), 3);
    assert_eq!(aces[2].sid, p.logon_sid);
    assert_eq!(aces[2].access, CLIENT_ACCESS);
    assert_eq!(CLIENT_ACCESS & (0x4 | 0xf000_0000 | 0x000c_0000), 0);
    assert_eq!(SERVER_ACCESS & 0x000d_0000, 0);
    assert_eq!(SERVER_ACCESS & 0x4, 0x4);
    assert!(aces.iter().all(|a| a.allow && !a.inherited));
}
#[test]
fn null_empty_broad_inherited_duplicate_or_extra_acl_never_passes() {
    let p = policy();
    let base = observation(&p, &request(&p));
    for i in 0..10 {
        let mut o = base.clone();
        match i {
            0 => o.descriptor.dacl = None,
            1 => o.descriptor.dacl = Some(vec![]),
            2 => o.descriptor.protected = false,
            3 => o.descriptor.owner = p.caller_sid.clone(),
            4 => o.descriptor.dacl.as_mut().unwrap()[2].access |= 0x4,
            5 => o.descriptor.dacl.as_mut().unwrap()[2].access = 0x4000_0000,
            6 => o.descriptor.dacl.as_mut().unwrap()[2].inherited = true,
            7 => o.descriptor.dacl.as_mut().unwrap()[2].allow = false,
            8 => o.descriptor.dacl.as_mut().unwrap()[2].sid = p.caller_sid.clone(),
            _ => {
                let extra = o.descriptor.dacl.as_ref().unwrap()[2].clone();
                o.descriptor.dacl.as_mut().unwrap().push(extra);
            }
        }
        denied(&o, Denial::DescriptorMismatch);
    }
}
#[test]
fn alternate_remote_inheritable_and_squatted_pipe_rejected() {
    let p = policy();
    let base = observation(&p, &request(&p));
    for i in 0..6 {
        let mut o = base.clone();
        match i {
            0 => o.pipe_name = r"\\other\pipe\RemoteHosts.Admin.v1".into(),
            1 => o.pipe_name.push_str(".other"),
            2 => o.local_client = false,
            3 => o.remote_clients_rejected = false,
            4 => o.first_instance_owned = false,
            _ => o.handle_inheritable = true,
        }
        denied(&o, Denial::PipeBoundary);
    }
}
#[test]
fn primary_process_token_impersonation_and_revert_failure_never_fall_back() {
    let p = policy();
    let base = observation(&p, &request(&p));
    let mut o = base.clone();
    o.caller = None;
    denied(&o, Denial::CallerTokenUnavailable);
    for i in 0..6 {
        let mut o = base.clone();
        match i {
            0 => o.impersonation_succeeded = false,
            1 => o.revert_succeeded = false,
            2 => o.caller.as_mut().unwrap().source = TokenSource::ProcessPrimary,
            3 => o.caller.as_mut().unwrap().level = ImpersonationLevel::Anonymous,
            4 => o.caller.as_mut().unwrap().level = ImpersonationLevel::Identification,
            _ => o.caller.as_mut().unwrap().level = ImpersonationLevel::Delegation,
        }
        denied(&o, Denial::CallerIdentity);
    }
}
#[test]
fn exact_user_and_logon_bindings_required_even_for_same_account() {
    let p = policy();
    let base = observation(&p, &request(&p));
    for i in 0..8 {
        let mut o = base.clone();
        let c = o.caller.as_mut().unwrap();
        match i {
            0 => c.user_sid = sid("S-1-5-21-10-20-30-1002"),
            1 => c.user_sid = sid(LOCAL_SYSTEM),
            2 => c.logon_sid = sid("S-1-5-5-40-51"),
            3 => c.logon_group_enabled = false,
            4 => c.logon_group_deny_only = true,
            5 => c.app_container = true,
            6 => c.logon_group_is_logon_id = false,
            _ => c.restricted = true,
        }
        denied(&o, Denial::CallerIdentity);
    }
}
#[test]
fn scm_pid_service_token_and_image_mismatch_rejected() {
    let p = policy();
    let base = observation(&p, &request(&p));
    for i in 0..9 {
        let mut o = base.clone();
        match i {
            0 => o.scm_service_name = "Other".into(),
            1 => o.scm_system_service = false,
            2 => o.scm_pid = 0,
            3 => o.pipe_server_pid += 1,
            4 => o.server_user_sid = p.caller_sid.clone(),
            5 => o.server_service_sid = sid("S-1-5-80-5-4-3-2-1"),
            6 => o.server_service_sid_enabled = false,
            7 => o.server_service_sid_deny_only = true,
            _ => o.server_image_sha256 = "c".repeat(64),
        }
        denied(&o, Denial::ServiceIdentity);
    }
}
#[test]
fn system_target_is_separate_from_helper_and_user_service_scope() {
    let p = policy();
    let base = observation(&p, &request(&p));
    assert_ne!(HELPER_SERVICE, TARGET_SYSTEM_SERVICE);
    for i in 0..3 {
        let mut o = base.clone();
        match i {
            0 => o.target_service_name = HELPER_SERVICE.into(),
            1 => o.target_system_service = false,
            _ => o.target_image_sha256 = "c".repeat(64),
        }
        denied(&o, Denial::ServiceIdentity);
    }
}
#[test]
fn action_device_policy_target_and_last_frame_are_bound() {
    let p = policy();
    let base = request(&p);
    for i in 0..5 {
        let mut r = base.clone();
        match i {
            0 => r.action = Action::ExactSourceDnsRepair,
            1 => r.device_id = uuid::Uuid::nil().to_string(),
            2 => r.expected_policy_sha256 = "c".repeat(64),
            3 => r.expected_target_sha256 = "c".repeat(64),
            _ => r.request_id = "019d1043-5867-7d48-a17a-48b5f66865e1".into(),
        }
        let o = observation(&p, &base);
        let a = assess(&p, r, &o, Grant::Verified, None, false, 10).unwrap();
        assert!(!a.offline_policy_checks_passed && a.denials.contains(&Denial::RequestBinding));
    }
}
#[test]
fn disabled_missing_and_revoked_grants_block_without_clearing_revocation() {
    let p = policy();
    let r = request(&p);
    let o = observation(&p, &r);
    for g in [Grant::NotChecked, Grant::NotGranted, Grant::Revoked] {
        let a = assess(&p, r.clone(), &o, g, None, false, 10).unwrap();
        assert!(!a.offline_policy_checks_passed);
        if g == Grant::Revoked {
            assert_eq!(a.plan.observed.grant, Grant::Revoked);
            assert!(a.denials.contains(&Denial::GrantRevoked));
        }
    }
    let mut p = p;
    p.enabled = false;
    let o = observation(&p, &request(&p));
    assert!(evaluate(&p, &o).denials.contains(&Denial::Disabled));
}
#[test]
fn original_running_closed_unknown_and_completed_receipts_are_observed_not_replayed() {
    let p = policy();
    let r = request(&p);
    let o = observation(&p, &r);
    for state in [
        State::Running,
        State::ClosedUnknown,
        State::Succeeded,
        State::Failed,
    ] {
        let record = original(state);
        let before = serde_json::to_vec(&record).unwrap();
        let a = assess(&p, r.clone(), &o, Grant::Verified, Some(&record), false, 10).unwrap();
        assert!(!a.offline_policy_checks_passed);
        assert_eq!(
            a.plan.next_action,
            if matches!(state, State::Running | State::ClosedUnknown) {
                NextAction::ObserveOriginal
            } else {
                NextAction::ReadOriginalReceipt
            }
        );
        assert_eq!(serde_json::to_vec(&record).unwrap(), before);
    }
    let a = assess(&p, r, &o, Grant::Verified, None, true, 10).unwrap();
    assert_eq!(a.plan.next_action, NextAction::ObserveOriginal);
    assert!(!a.offline_policy_checks_passed);
}
#[test]
fn foreign_receipt_malformed_ids_and_overflow_fail_closed() {
    let p = policy();
    let r = request(&p);
    let o = observation(&p, &r);
    let mut record = original(State::Running);
    record.plan.request_id = uuid::Uuid::nil().to_string();
    assert!(assess(&p, r.clone(), &o, Grant::Verified, Some(&record), false, 10).is_err());
    let mut other = r.clone();
    other.request_id = "claim".into();
    assert!(assess(&p, other, &o, Grant::Verified, None, false, 10).is_err());
    assert!(assess(&p, r, &o, Grant::Verified, None, false, u64::MAX).is_err());
}
#[test]
fn binding_changes_with_grant_caller_or_original_uncertainty() {
    let p = policy();
    let r = request(&p);
    let o = observation(&p, &r);
    let a = evaluate(&p, &o);
    let b = assess(&p, r.clone(), &o, Grant::Revoked, None, false, 10).unwrap();
    assert_ne!(a.binding_sha256, b.binding_sha256);
    let b = assess(&p, r, &o, Grant::Verified, None, true, 10).unwrap();
    assert_ne!(a.binding_sha256, b.binding_sha256);
    let mut o = o;
    o.caller.as_mut().unwrap().logon_sid = sid("S-1-5-5-40-51");
    assert_ne!(a.binding_sha256, evaluate(&p, &o).binding_sha256);
}
