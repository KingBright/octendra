use super::*;
use clap::Parser;
use std::cell::RefCell;
struct Wire {
    status: RefCell<Value>,
    record: RefCell<Record>,
    calls: RefCell<Vec<&'static str>>,
    lose_apply: bool,
}
impl RecordClient for Wire {
    async fn request(&self, r: &Request) -> Result<Value> {
        let value = match r {
            Request::Status => {
                self.calls.borrow_mut().push("status");
                self.status.borrow().clone()
            }
            Request::Plan { .. } => {
                self.calls.borrow_mut().push("plan");
                serde_json::to_value(self.record.borrow().clone())?
            }
            Request::Receipt { .. } => {
                self.calls.borrow_mut().push("receipt");
                serde_json::to_value(self.record.borrow().clone())?
            }
            Request::Apply { .. } => {
                self.calls.borrow_mut().push("apply");
                if self.lose_apply {
                    anyhow::bail!("lost_reply")
                };
                self.record.borrow_mut().state = State::Succeeded;
                serde_json::to_value(self.record.borrow().clone())?
            }
            Request::Revoke => {
                self.calls.borrow_mut().push("revoke");
                self.status.borrow_mut()["enabled"] = json!(false);
                self.status.borrow_mut()["revoked"] = json!(true);
                json!({"enabled":false,"revoked":true})
            }
        };
        Ok(json!({"ok":true,"result":value}))
    }
}
fn fixture() -> (Wire, String, String) {
    let device = uuid::Uuid::new_v4().to_string();
    let id = uuid::Uuid::new_v4().to_string();
    let plan = crate::protocol::Plan {
        protocol: 1,
        action: managed_service::ACTION.into(),
        request_id: id.clone(),
        device_id: device.clone(),
        caller_uid: 1000,
        policy_sha256: "a".repeat(64),
        created_at: 1000,
        expires_at: 1300,
        snapshot: crate::protocol::Snapshot::default(),
    };
    let record = Record {
        plan_sha256: digest(&plan).unwrap(),
        plan,
        state: State::Prepared,
        steps: vec![],
        error: None,
        after: None,
        updated_at: 1000,
    };
    let status = json!({"protocol":1,"available":true,"device_id":device,"caller_uid":1000,"allowed_action":managed_service::ACTION,"arbitrary_commands":false,"enabled":true,"revoked":false,"policy_sha256":"a".repeat(64)});
    (
        Wire {
            status: RefCell::new(status),
            record: RefCell::new(record),
            calls: RefCell::new(vec![]),
            lose_apply: false,
        },
        device,
        id,
    )
}
#[tokio::test]
async fn full_bound_cli_workflow_and_revocation_keep_receipt_readable() {
    let (w, d, id) = fixture();
    let p = plan_using(&w, &d, &id, 1000, Some(&"a".repeat(64)))
        .await
        .unwrap();
    let r = apply_using(&w, &d, 1000, &p, &p.record.plan_sha256, false)
        .await
        .unwrap();
    assert_eq!(r.state, State::Succeeded);
    assert_eq!(
        w.calls.borrow().as_slice(),
        ["status", "plan", "status", "receipt", "apply"]
    );
    run_client(
        Command::Revoke {
            device_id: d.clone(),
        },
        &w,
        1000,
    )
    .await
    .unwrap();
    let status = run_client(
        Command::Status {
            device_id: d.clone(),
        },
        &w,
        1000,
    )
    .await
    .unwrap();
    assert_eq!(status["capability"]["helper"], "verified");
    assert_eq!(status["capability"]["grant"], "revoked");
    assert_eq!(status["capability"]["execution_available"], false);
    let receipt = run_client(
        Command::Receipt {
            device_id: d,
            request_id: id,
            output: None,
        },
        &w,
        1000,
    )
    .await
    .unwrap();
    assert_eq!(receipt["record"]["state"], "succeeded");
}
#[tokio::test]
async fn mismatched_device_uid_profile_and_policy_do_not_queue_plan() {
    for index in 0..4 {
        let (w, d, id) = fixture();
        match index {
            0 => w.status.borrow_mut()["device_id"] = json!(uuid::Uuid::new_v4().to_string()),
            1 => w.status.borrow_mut()["caller_uid"] = json!(1001),
            2 => w.status.borrow_mut()["allowed_action"] = json!("exact_source_dns_repair"),
            _ => w.status.borrow_mut()["policy_sha256"] = json!("b".repeat(64)),
        }
        assert!(
            plan_using(&w, &d, &id, 1000, Some(&"a".repeat(64)))
                .await
                .is_err()
        );
        assert!(!w.calls.borrow().contains(&"plan"));
    }
}
#[tokio::test]
async fn explicit_digest_and_unknown_guard_prevent_apply_submission() {
    let (w, d, _) = fixture();
    let p = saved(w.record.borrow().clone()).unwrap();
    assert!(
        apply_using(&w, &d, 1000, &p, &"b".repeat(64), false)
            .await
            .is_err()
    );
    assert!(w.calls.borrow().is_empty());
    assert!(
        apply_using(&w, &d, 1000, &p, &p.record.plan_sha256, true)
            .await
            .is_err()
    );
    assert!(!w.calls.borrow().contains(&"apply"));
}
#[tokio::test]
async fn lost_reply_is_not_retried_and_complete_receipt_does_not_apply_again() {
    let (mut w, d, _) = fixture();
    let p = saved(w.record.borrow().clone()).unwrap();
    w.lose_apply = true;
    assert!(
        apply_using(&w, &d, 1000, &p, &p.record.plan_sha256, false)
            .await
            .is_err()
    );
    assert_eq!(
        w.calls.borrow().iter().filter(|s| **s == "apply").count(),
        1
    );
    w.record.borrow_mut().state = State::Succeeded;
    w.calls.borrow_mut().clear();
    let r = apply_using(&w, &d, 1000, &p, &p.record.plan_sha256, false)
        .await
        .unwrap();
    assert_eq!(r.state, State::Succeeded);
    assert_eq!(w.calls.borrow().as_slice(), ["status", "receipt"]);
}
#[test]
fn disabled_handshake_never_claims_grant_or_execution() {
    let (w, _, _) = fixture();
    w.status.borrow_mut()["enabled"] = json!(false);
    w.status.borrow_mut()["policy_sha256"] = Value::Null;
    let cap = capability(&w.status.borrow());
    assert_eq!(cap.helper, Helper::Verified);
    assert_eq!(cap.grant, Grant::NotGranted);
    assert!(!cap.execution_available);
    assert!(!cap.arbitrary_root_commands);
}
#[derive(Parser)]
struct Parse {
    #[command(subcommand)]
    command: Command,
}
#[test]
fn native_entries_are_present_but_arbitrary_commands_and_service_names_are_not() {
    for args in [
        vec![
            "helper",
            "status",
            "--device-id",
            "00000000-0000-0000-0000-000000000001",
        ],
        vec![
            "helper",
            "serve",
            "--policy",
            "/policy",
            "--state-dir",
            "/state",
            "--grant",
            "/grant",
        ],
        vec![
            "helper",
            "verify-package",
            "--bundle",
            "/bundle",
            "--manifest-sha256",
            "a",
        ],
    ] {
        assert!(Parse::try_parse_from(args).is_ok());
    }
    for args in [
        vec!["helper", "shell", "-c", "id"],
        vec!["helper", "apply", "--service", "sing-box"],
        vec!["helper", "serve", "--command", "/bin/sh"],
    ] {
        assert!(Parse::try_parse_from(args).is_err());
    }
}
#[tokio::test]
async fn unsupported_platform_server_never_touches_requested_paths() {
    if !cfg!(target_os = "linux") {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("missing");
        assert!(
            run(
                Command::Serve {
                    policy: p.clone(),
                    state_dir: p.clone(),
                    grant: p.clone()
                },
                &p
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("linux_native_server")
        );
        assert!(!p.exists());
    }
}
