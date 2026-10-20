use super::*;
use crate::protocol::{Plan, UnitState};
fn fixture() -> (tempfile::TempDir, Store, Policy, Record, Snapshot, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store {
        dir: dir.path().to_owned(),
        owner: nix::unistd::geteuid().as_raw(),
    };
    let policy = Policy {
        protocol: 1,
        device_id: uuid::Uuid::new_v4().to_string(),
        grant_id: uuid::Uuid::new_v4().to_string(),
        allowed_uid: 1000,
        allowed_gid: 1000,
        home: "/home/liang".into(),
        platform: "linux".into(),
        enabled: true,
    };
    let snapshot = Snapshot {
        current_service_active: true,
        units: vec![UnitState {
            name: managed_service::SERVICE.into(),
            load: "loaded".into(),
            active: "active".into(),
            enabled: "enabled".into(),
            fragment: "/etc/systemd/system/octendra-code-agent.service".into(),
            drop_ins: String::new(),
        }],
        ..Snapshot::default()
    };
    let plan = Plan {
        protocol: 1,
        action: managed_service::ACTION.into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        device_id: policy.device_id.clone(),
        caller_uid: 1000,
        policy_sha256: "a".repeat(64),
        created_at: 100,
        expires_at: 400,
        snapshot: snapshot.clone(),
    };
    let record = Record {
        plan_sha256: digest(&plan).unwrap(),
        plan,
        state: State::Running,
        steps: vec!["managed_service_restart_accepted".into()],
        error: Some("outcome_unknown_observe_original".into()),
        after: None,
        updated_at: 101,
    };
    store.save(&record).unwrap();
    store.revoke(1000, 102).unwrap();
    (
        dir,
        store,
        policy,
        record,
        snapshot,
        uuid::Uuid::new_v4().to_string(),
    )
}
#[test]
fn closure_preserves_original_unknown_and_revocation_without_claiming_execution() {
    let (_d, s, p, r, after, boot) = fixture();
    let out = close_verified(
        &s,
        &p,
        &p.device_id,
        &r.plan.request_id,
        &digest(&r).unwrap(),
        &observation_sha(&after, &boot).unwrap(),
        after,
        &boot,
        103,
    )
    .unwrap();
    assert_eq!(out.state, State::ClosedUnknown);
    assert_eq!(out.plan_sha256, r.plan_sha256);
    assert_eq!(out.error, r.error);
    assert_eq!(out.steps[0], r.steps[0]);
    assert!(s.revoked().unwrap());
    assert_eq!(
        s.load(&r.plan.request_id).unwrap().unwrap().state,
        State::ClosedUnknown
    );
}
#[test]
fn wrong_device_request_digest_observation_or_non_running_cannot_close() {
    for variant in 0..5 {
        let (_d, s, p, mut r, mut after, boot) = fixture();
        let mut device = p.device_id.clone();
        let mut hash = digest(&r).unwrap();
        let obs = observation_sha(&after, &boot).unwrap();
        match variant {
            0 => device = uuid::Uuid::new_v4().to_string(),
            1 => hash = "b".repeat(64),
            2 => after.current_service_active = false,
            3 => {
                r.state = State::Succeeded;
                s.save(&r).unwrap();
            }
            _ => {
                r.plan.request_id = uuid::Uuid::new_v4().to_string();
            }
        }
        assert!(
            close_verified(
                &s,
                &p,
                &device,
                &r.plan.request_id,
                &hash,
                &obs,
                after,
                &boot,
                103
            )
            .is_err()
        );
    }
}
#[test]
fn revoked_grant_and_original_resource_binding_are_required() {
    let (_d, s, p, r, after, boot) = fixture();
    std::fs::remove_file(s.dir.join("revoked.json")).unwrap();
    assert!(
        close_verified(
            &s,
            &p,
            &p.device_id,
            &r.plan.request_id,
            &digest(&r).unwrap(),
            &observation_sha(&after, &boot).unwrap(),
            after,
            &boot,
            103
        )
        .is_err()
    );
}
#[test]
fn changed_record_and_boot_never_overwrite_original() {
    let (_d, s, p, r, after, boot) = fixture();
    let hash = digest(&r).unwrap();
    let obs = observation_sha(&after, &boot).unwrap();
    let mut changed = r.clone();
    changed.steps.push("new evidence".into());
    s.save(&changed).unwrap();
    assert!(
        close_verified(
            &s,
            &p,
            &p.device_id,
            &r.plan.request_id,
            &hash,
            &obs,
            after.clone(),
            &boot,
            103
        )
        .is_err()
    );
    assert!(
        close_verified(
            &s,
            &p,
            &p.device_id,
            &r.plan.request_id,
            &digest(&changed).unwrap(),
            &obs,
            after,
            &uuid::Uuid::new_v4().to_string(),
            103
        )
        .is_err()
    );
    assert_eq!(
        digest(&s.load(&r.plan.request_id).unwrap().unwrap()).unwrap(),
        digest(&changed).unwrap()
    );
}
#[tokio::test]
async fn native_entry_rejects_non_linux_or_non_root_before_accessing_paths() {
    if !cfg!(target_os = "linux") || !nix::unistd::geteuid().is_root() {
        assert!(
            run(
                &uuid::Uuid::new_v4().to_string(),
                &uuid::Uuid::new_v4().to_string(),
                None
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("separate_linux_root")
        );
    }
}
#[test]
fn closed_original_is_idempotent_and_never_prepared_again() {
    let (_d, s, p, r, after, boot) = fixture();
    let out = close_verified(
        &s,
        &p,
        &p.device_id,
        &r.plan.request_id,
        &digest(&r).unwrap(),
        &observation_sha(&after, &boot).unwrap(),
        after.clone(),
        &boot,
        103,
    )
    .unwrap();
    let again = close_verified(
        &s,
        &p,
        &p.device_id,
        &r.plan.request_id,
        "old-original-digest",
        "old-observation",
        after,
        &boot,
        104,
    )
    .unwrap();
    assert_eq!(digest(&out).unwrap(), digest(&again).unwrap());
}
