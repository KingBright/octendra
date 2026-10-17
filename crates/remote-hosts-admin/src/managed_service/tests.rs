use super::*;

#[tokio::test]
async fn unknown_action_blocks_another_uuid_and_already_prepared_plan() {
    let (_d, e, p) = fixture();
    let first = prepare(&e, &p).await;
    let second = prepare(&e, &p).await;
    e.backend.driver.uncertain.set(true);
    assert_eq!(
        e.handle(&p, 1000, apply(&first), 1001).await.unwrap()["state"],
        "running"
    );
    let error = e
        .handle(
            &p,
            1000,
            Request::Plan {
                request_id: uuid::Uuid::new_v4().to_string(),
            },
            1002,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unresolved_operation"));
    assert!(
        e.handle(&p, 1000, apply(&second), 1002)
            .await
            .unwrap_err()
            .to_string()
            .contains("unresolved_operation")
    );
    assert_eq!(
        e.handle(
            &p,
            1000,
            Request::Receipt {
                request_id: first.plan.request_id
            },
            1002
        )
        .await
        .unwrap()["state"],
        "running"
    );
    assert_eq!(e.backend.driver.calls.get(), 1);
}

struct ReplyClient {
    record: RefCell<Record>,
    calls: RefCell<Vec<&'static str>>,
    lose_apply_reply: bool,
}
impl RecordClient for ReplyClient {
    async fn request(&self, request: &Request) -> Result<serde_json::Value> {
        match request {
            Request::Receipt { .. } => self.calls.borrow_mut().push("receipt"),
            Request::Apply { .. } => {
                self.calls.borrow_mut().push("apply");
                if self.lose_apply_reply {
                    anyhow::bail!("reply_lost");
                }
                self.record.borrow_mut().state = crate::protocol::State::Succeeded;
            }
            _ => anyhow::bail!("unexpected_client_request"),
        }
        Ok(serde_json::json!({"ok":true,"result":self.record.borrow().clone()}))
    }
}
fn expected(record: &Record) -> PlanRequest {
    PlanRequest {
        request_id: record.plan.request_id.clone(),
        device_id: record.plan.device_id.clone(),
        action: Action::ManagedServiceRestart,
        expected_policy_sha256: record.plan.policy_sha256.clone(),
        expected_target_sha256: digest(&record.plan.snapshot).unwrap(),
    }
}
#[tokio::test]
async fn bound_client_observes_before_one_apply_and_never_retries_lost_reply() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    let client = ReplyClient {
        record: RefCell::new(r.clone()),
        calls: RefCell::new(vec![]),
        lose_apply_reply: true,
    };
    assert!(
        apply_bound_using(&client, &expected(&r), &r, false)
            .await
            .is_err()
    );
    assert_eq!(client.calls.borrow().as_slice(), ["receipt", "apply"]);
    assert!(
        apply_bound_using(&client, &expected(&r), &r, true)
            .await
            .is_err()
    );
    assert_eq!(client.calls.borrow().len(), 2);
}
#[tokio::test]
async fn bound_client_only_reads_when_original_receipt_is_running_or_complete() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    for state in [
        crate::protocol::State::Running,
        crate::protocol::State::Succeeded,
    ] {
        let mut fresh = r.clone();
        fresh.state = state;
        let client = ReplyClient {
            record: RefCell::new(fresh),
            calls: RefCell::new(vec![]),
            lose_apply_reply: false,
        };
        assert_eq!(
            apply_bound_using(&client, &expected(&r), &r, false)
                .await
                .unwrap()
                .state,
            state
        );
        assert_eq!(client.calls.borrow().as_slice(), ["receipt"]);
    }
}
#[tokio::test]
async fn bound_client_rejects_changed_original_plan_without_apply() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    let mut fresh = r.clone();
    fresh.plan.expires_at += 1;
    fresh.plan_sha256 = digest(&fresh.plan).unwrap();
    let client = ReplyClient {
        record: RefCell::new(fresh),
        calls: RefCell::new(vec![]),
        lose_apply_reply: false,
    };
    assert!(
        apply_bound_using(&client, &expected(&r), &r, false)
            .await
            .is_err()
    );
    assert_eq!(client.calls.borrow().as_slice(), ["receipt"]);
}

use crate::{engine::Engine, protocol::Request};
use std::{
    cell::{Cell, RefCell},
    os::unix::fs::PermissionsExt,
};

struct IsolatedDriver {
    grant: RefCell<ServiceGrant>,
    snapshot: RefCell<Snapshot>,
    calls: Cell<usize>,
    uncertain: Cell<bool>,
}
impl ServiceDriver for IsolatedDriver {
    fn grant(&self) -> Result<ServiceGrant> {
        Ok(self.grant.borrow().clone())
    }
    async fn inspect(&self, _: &Policy, _: &ServiceGrant) -> Result<Snapshot> {
        Ok(self.snapshot.borrow().clone())
    }
    async fn restart(&self) -> Result<()> {
        self.calls.set(self.calls.get() + 1);
        if self.uncertain.get() {
            Err(OutcomeUnknown.into())
        } else {
            Ok(())
        }
    }
}
type TestEngine = Engine<ManagedServiceBackend<IsolatedDriver>>;
fn fixture() -> (tempfile::TempDir, TestEngine, Policy) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let policy = Policy {
        protocol: 1,
        device_id: uuid::Uuid::new_v4().to_string(),
        grant_id: uuid::Uuid::new_v4().to_string(),
        allowed_uid: 1000,
        allowed_gid: 1000,
        home: "/home/test".into(),
        platform: "linux".into(),
        enabled: true,
    };
    let grant = ServiceGrant {
        protocol: 1,
        device_id: policy.device_id.clone(),
        grant_id: policy.grant_id.clone(),
        enabled: true,
        not_before: 0,
        expires_at: u64::MAX,
        action: Action::ManagedServiceRestart,
        unit: SERVICE.into(),
        unit_sha256: "a".repeat(64),
        executable: "/usr/local/libexec/remote-hosts-code".into(),
        executable_sha256: "b".repeat(64),
        agent_config: "/etc/remote-hosts-code/agent.json".into(),
        agent_config_sha256: "c".repeat(64),
    };
    let snapshot = Snapshot {
        current_service_active: true,
        units: vec![UnitState {
            name: SERVICE.into(),
            load: "loaded".into(),
            active: "active".into(),
            enabled: "enabled".into(),
            fragment: format!("/etc/systemd/system/{SERVICE}"),
            drop_ins: String::new(),
        }],
        ..Snapshot::default()
    };
    let engine = Engine {
        store: Store {
            dir: dir.path().to_owned(),
            owner: nix::unistd::geteuid().as_raw(),
        },
        backend: ManagedServiceBackend {
            driver: IsolatedDriver {
                grant: RefCell::new(grant),
                snapshot: RefCell::new(snapshot),
                calls: Cell::new(0),
                uncertain: Cell::new(false),
            },
        },
    };
    (dir, engine, policy)
}
async fn prepare(engine: &TestEngine, policy: &Policy) -> Record {
    let id = uuid::Uuid::new_v4().to_string();
    serde_json::from_value(
        engine
            .handle(policy, 1000, Request::Plan { request_id: id }, 1000)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn apply(record: &Record) -> Request {
    Request::Apply {
        request_id: record.plan.request_id.clone(),
        plan_sha256: record.plan_sha256.clone(),
    }
}
#[tokio::test]
async fn authenticated_bound_operation_executes_once_and_reuses_receipt() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    assert_eq!(r.plan.action, ACTION);
    for _ in 0..2 {
        let value = e.handle(&p, 1000, apply(&r), 1001).await.unwrap();
        assert_eq!(value["state"], "succeeded");
    }
    assert_eq!(e.backend.driver.calls.get(), 1);
    assert_eq!(
        e.store.load(&r.plan.request_id).unwrap().unwrap().steps,
        ["managed_service_restart_accepted"]
    );
}
#[tokio::test]
async fn peer_uid_and_root_grant_device_are_required_before_plan() {
    let (_d, e, p) = fixture();
    assert!(
        e.handle(
            &p,
            1001,
            Request::Plan {
                request_id: uuid::Uuid::new_v4().to_string()
            },
            1000
        )
        .await
        .is_err()
    );
    let mut other = p.clone();
    other.device_id = uuid::Uuid::new_v4().to_string();
    assert!(
        e.handle(
            &other,
            1000,
            Request::Plan {
                request_id: uuid::Uuid::new_v4().to_string()
            },
            1000
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read_dir(&e.store.dir).unwrap().count(), 0);
}
#[tokio::test]
async fn grant_change_and_target_change_block_prepared_apply() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    e.backend.driver.grant.borrow_mut().expires_at -= 1;
    assert!(e.handle(&p, 1000, apply(&r), 1001).await.is_err());
    e.backend.driver.grant.borrow_mut().expires_at += 1;
    e.backend.driver.snapshot.borrow_mut().units[0].fragment = "/elsewhere".into();
    assert!(e.handle(&p, 1000, apply(&r), 1001).await.is_err());
    assert_eq!(e.backend.driver.calls.get(), 0);
}
#[tokio::test]
async fn revoked_grant_rejects_new_work_but_keeps_original_receipts_readable() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    e.handle(&p, 1000, Request::Revoke, 1001).await.unwrap();
    assert!(e.handle(&p, 1000, apply(&r), 1002).await.is_err());
    assert!(
        e.handle(
            &p,
            1000,
            Request::Plan {
                request_id: uuid::Uuid::new_v4().to_string()
            },
            1002
        )
        .await
        .is_err()
    );
    assert_eq!(
        e.handle(
            &p,
            1000,
            Request::Receipt {
                request_id: r.plan.request_id
            },
            1002
        )
        .await
        .unwrap()["state"],
        "prepared"
    );
    assert_eq!(e.backend.driver.calls.get(), 0);
}
#[tokio::test]
async fn unknown_side_effect_is_durable_and_cannot_be_replayed() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    e.backend.driver.uncertain.set(true);
    let first = e.handle(&p, 1000, apply(&r), 1001).await.unwrap();
    assert_eq!(first["state"], "running");
    assert_eq!(first["recovery_required"], true);
    assert_eq!(first["error"], "outcome_unknown_observe_original");
    let restored = Engine {
        store: e.store.clone(),
        backend: ManagedServiceBackend {
            driver: IsolatedDriver {
                grant: RefCell::new(e.backend.driver.grant().unwrap()),
                snapshot: RefCell::new(e.backend.driver.snapshot.borrow().clone()),
                calls: Cell::new(0),
                uncertain: Cell::new(false),
            },
        },
    };
    assert_eq!(
        restored.handle(&p, 1000, apply(&r), 1002).await.unwrap()["state"],
        "running"
    );
    assert_eq!(restored.backend.driver.calls.get(), 0);
    assert_eq!(e.backend.driver.calls.get(), 1);
}
#[tokio::test]
async fn cross_action_receipts_and_plan_digest_tampering_are_rejected() {
    let (_d, e, p) = fixture();
    let mut r = prepare(&e, &p).await;
    r.plan.action = crate::protocol::ACTION.into();
    r.plan_sha256 = digest(&r.plan).unwrap();
    e.store.save(&r).unwrap();
    assert!(e.handle(&p, 1000, apply(&r), 1001).await.is_err());
    assert!(
        e.handle(
            &p,
            1000,
            Request::Receipt {
                request_id: r.plan.request_id.clone()
            },
            1001
        )
        .await
        .is_err()
    );
    r.plan.action = ACTION.into();
    r.plan_sha256 = "d".repeat(64);
    e.store.save(&r).unwrap();
    assert!(e.handle(&p, 1000, apply(&r), 1001).await.is_err());
    assert_eq!(e.backend.driver.calls.get(), 0);
}
#[tokio::test]
async fn gateway_adapter_validates_original_operation_and_all_digests() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    let expected = PlanRequest {
        request_id: r.plan.request_id.clone(),
        device_id: p.device_id,
        action: Action::ManagedServiceRestart,
        expected_policy_sha256: r.plan.policy_sha256.clone(),
        expected_target_sha256: digest(&r.plan.snapshot).unwrap(),
    };
    validate_operation_binding(&expected, &r).unwrap();
    for index in 0..5 {
        let mut bad = expected.clone();
        match index {
            0 => bad.device_id = uuid::Uuid::new_v4().to_string(),
            1 => bad.request_id = uuid::Uuid::new_v4().to_string(),
            2 => bad.action = Action::ExactSourceDnsRepair,
            3 => bad.expected_policy_sha256 = "d".repeat(64),
            _ => bad.expected_target_sha256 = "e".repeat(64),
        }
        assert!(validate_operation_binding(&bad, &r).is_err());
    }
}
fn unit(g: &ServiceGrant) -> String {
    format!(
        "[Unit]\nDescription=Isolated test\nAfter=network.target\n[Service]\nUser=1000\nGroup=1000\nExecStart={} agent --config {}\nNoNewPrivileges=yes\nType=simple\n[Install]\nWantedBy=multi-user.target\n",
        g.executable, g.agent_config
    )
}
#[test]
fn unit_profile_rejects_shell_scripts_hooks_environment_and_identity_overrides() {
    let (_d, e, _p) = fixture();
    let grant = e.backend.driver.grant().unwrap();
    let valid = unit(&grant);
    validate_unit(&valid, &grant, 1000, 1000).unwrap();
    let attacks = [
        valid.replace(&grant.executable, "/bin/sh"),
        valid.replace("ExecStart=", "ExecStart=+"),
        valid.replace("User=1000", "User=0"),
        valid.replace("Group=1000", "Group=0"),
        valid.replace("NoNewPrivileges=yes", "NoNewPrivileges=no"),
        valid.replace("[Install]", "ExecStartPre=/home/test/mutable.sh\n[Install]"),
        valid.replace("[Install]", "EnvironmentFile=/home/test/env\n[Install]"),
        valid.replace("[Install]", "RootDirectory=/home/test\n[Install]"),
        valid.replace("Description=Isolated test", "Requires=unreviewed.service"),
        valid.replace("Type=simple", "Type=simple\nUser=1000"),
        valid.replace("Type=simple", "Type=oneshot"),
    ];
    for text in attacks {
        assert!(validate_unit(&text, &grant, 1000, 1000).is_err());
    }
}
#[test]
fn action_resources_expiry_and_path_escapes_are_closed() {
    let (_d, e, p) = fixture();
    let grant = e.backend.driver.grant().unwrap();
    grant.validate(&p, 1).unwrap();
    for index in 0..8 {
        let mut bad = grant.clone();
        match index {
            0 => bad.unit = "sing-box.service".into(),
            1 => bad.action = Action::ExactSourceDnsRepair,
            2 => bad.executable = "/usr/bin/bash".into(),
            3 => bad.agent_config = "/etc/../home/test/agent.json".into(),
            4 => bad.executable = "/usr/local/bin/remote-hosts-code;sh".into(),
            5 => bad.expires_at = 1,
            6 => bad.enabled = false,
            _ => bad.unit_sha256 = "not-a-sha".into(),
        }
        assert!(bad.validate(&p, 1).is_err());
    }
    let mut root = p.clone();
    root.allowed_uid = 0;
    assert!(grant.validate(&root, 1).is_err());
    let mut group = p;
    group.allowed_gid = 0;
    assert!(grant.validate(&group, 1).is_err());
}
#[test]
fn caller_writable_resource_and_injected_request_fields_fail_closed() {
    let (dir, _e, _p) = fixture();
    let path = dir.path().join("remote-hosts-code");
    std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
    assert!(trusted_resource(&path, &"a".repeat(64), true).is_err());
    let request = serde_json::json!({"op":"plan","request_id":uuid::Uuid::new_v4().to_string(),"command":"/bin/sh"});
    assert!(serde_json::from_value::<Request>(request).is_err());
}
#[test]
fn native_driver_never_silently_falls_back_to_other_platform_or_sudo() {
    if !cfg!(target_os = "linux") || !nix::unistd::geteuid().is_root() {
        let driver = LinuxServiceDriver::new(PathBuf::from("/does-not-exist"));
        assert!(
            driver
                .grant()
                .unwrap_err()
                .to_string()
                .contains("linux_root_helper_required")
        );
    }
}
#[tokio::test]
async fn disabled_or_expired_root_grant_never_reaches_restart() {
    let (_d, e, p) = fixture();
    let r = prepare(&e, &p).await;
    e.backend.driver.grant.borrow_mut().enabled = false;
    assert!(e.handle(&p, 1000, apply(&r), 1001).await.is_err());
    assert_eq!(
        e.handle(&p, 1000, Request::Status, 1001).await.unwrap()["enabled"],
        false
    );
    assert_eq!(
        e.handle(
            &p,
            1000,
            Request::Receipt {
                request_id: r.plan.request_id
            },
            1001
        )
        .await
        .unwrap()["state"],
        "prepared"
    );
    assert_eq!(e.backend.driver.calls.get(), 0);
}
