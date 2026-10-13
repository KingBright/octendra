//! Bounded diagnostics only. No authority, input, replay or recovery decisions.
use crate::store::Store;
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(any(target_os = "linux", test))]
use std::path::Path;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use uuid::Uuid;

const MAX_EVENTS: usize = 32;
const MAX_LINE: usize = 1024;
const MAX_BYTES: usize = 32 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    protocol: u32,
    request_id: Uuid,
    pid: u32,
    phase: String,
}
pub fn process_identity(pid: u32) -> Value {
    #[cfg(target_os = "linux")]
    {
        linux_identity(Path::new("/proc"), pid)
            .unwrap_or_else(|| json!({"pid":pid,"identity_state":"unavailable"}))
    }
    #[cfg(not(target_os = "linux"))]
    json!({"pid":pid,"identity_state":"platform_identity_unavailable"})
}
#[cfg(any(target_os = "linux", test))]
fn linux_identity(root: &Path, pid: u32) -> Option<Value> {
    let stat = std::fs::read_to_string(root.join(pid.to_string()).join("stat")).ok()?;
    let end = stat.rfind(')')?;
    let ticks = stat[end + 1..]
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()?;
    let boot = std::fs::read_to_string(root.join("sys/kernel/random/boot_id")).ok()?;
    let boot = Uuid::parse_str(boot.trim()).ok()?;
    Some(json!({"pid":pid,"identity_state":"observed","start_ticks":ticks,"boot_id":boot}))
}
pub fn interrupted(evidence: &Value, current_parent: &Value) -> bool {
    evidence["parent_identity"]["identity_state"] == "observed"
        && evidence["parent_identity"] != *current_parent
        && evidence["worker_exit"]["reaped"] != true
}
fn valid_phase(s: &str) -> bool {
    matches!(
        s,
        "spawned"
            | "open_sent"
            | "reply_received"
            | "cleanup_started"
            | "child_exit_observed"
            | "approval_verified"
            | "open_received"
            | "portal_request_started"
            | "portal_session_ready"
            | "capture_attached"
            | "operation_timeout"
            | "shutdown_started"
            | "shutdown_confirmed"
            | "shutdown_unknown"
            | "reply_written"
            | "worker_loop_ended"
    )
}
#[derive(Clone)]
pub struct Tracker {
    value: Arc<Mutex<Value>>,
    writes: Arc<tokio::sync::Mutex<()>>,
    requests: Arc<Mutex<BTreeSet<Uuid>>>,
    sink: Option<(Store, String)>,
    operation: Uuid,
    pid: u32,
}
impl Tracker {
    pub fn new(operation: Uuid, pid: u32, sink: Option<(Store, String)>) -> Self {
        Self {
            value: Arc::new(Mutex::new(json!({
                "protocol":1,"operation_id":operation,"worker_identity":process_identity(pid),
                "parent_identity":process_identity(std::process::id()),"events":[],
                "worker_exit":null,"persistence_failed":false,"binding":null,"shutdown_status":null,
                "outcome":"unknown_until_operation_receipt"
            }))),
            writes: Arc::new(tokio::sync::Mutex::new(())),
            requests: Arc::new(Mutex::new(BTreeSet::from([operation]))),
            sink,
            operation,
            pid,
        }
    }
    pub fn admit(&self, request: Uuid) {
        let mut requests = self.requests.lock().expect("diagnostic requests");
        if requests.len() < 128 {
            requests.insert(request);
        }
    }
    pub fn snapshot(&self) -> Value {
        self.value.lock().expect("diagnostic mutex").clone()
    }
    fn append(&self, phase: &str, request: Uuid) {
        let mut v = self.value.lock().expect("diagnostic mutex");
        let events = v["events"].as_array_mut().expect("diagnostic events");
        if events.len() < MAX_EVENTS {
            events
                .push(json!({"phase":phase,"request_id":request,"observed_at_ms":crate::now_ms()}));
        }
    }
    pub async fn phase(&self, phase: &str, request: Uuid) -> bool {
        self.append(phase, request);
        self.persist().await
    }
    pub async fn bind(&self, binding: &remote_hosts_desktop_protocol::Binding) -> bool {
        self.value.lock().expect("diagnostic mutex")["binding"] =
            serde_json::to_value(binding).expect("binding serialization");
        self.persist().await
    }
    pub async fn exit(&self, exit: Value) -> bool {
        self.value.lock().expect("diagnostic mutex")["worker_exit"] = exit;
        self.phase("child_exit_observed", self.operation).await
    }
    pub async fn persist(&self) -> bool {
        let Some((store, key)) = &self.sink else {
            return true;
        };
        let _write = self.writes.lock().await;
        let snapshot = self.snapshot();
        let written = tokio::time::timeout(Duration::from_millis(250), async {
            let mut guard = store
                .get::<Value>("desktop_guard", key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("diagnostic guard missing"))?;
            anyhow::ensure!(
                guard["operation_id"] == self.operation.to_string(),
                "diagnostic operation identity changed"
            );
            if !snapshot["worker_exit"].is_null() {
                guard["worker_exit"] = snapshot["worker_exit"].clone();
                guard["worker_exit_observed_at_ms"] = json!(crate::now_ms());
            }
            guard["worker_lifecycle"] = snapshot;
            guard["worker_pid"] = json!(self.pid);
            store.put("desktop_guard", key, &guard, i64::MAX).await
        })
        .await;
        let ok = matches!(written, Ok(Ok(())));
        if !ok {
            self.value.lock().expect("diagnostic mutex")["persistence_failed"] = json!(true);
        }
        ok
    }
    fn admitted(&self, id: &Uuid) -> bool {
        self.requests
            .lock()
            .expect("diagnostic requests")
            .contains(id)
    }
    pub async fn read<R: AsyncRead + Unpin>(&self, mut input: R) {
        let mut buffer = [0u8; 512];
        let mut line = Vec::new();
        let mut bytes = 0usize;
        let mut oversized = false;
        loop {
            let Ok(n) = input.read(&mut buffer).await else {
                break;
            };
            if n == 0 {
                break;
            }
            for b in &buffer[..n] {
                bytes = bytes.saturating_add(1);
                if *b == b'\n' {
                    if !oversized
                        && bytes <= MAX_BYTES
                        && let Ok(event) = serde_json::from_slice::<Event>(&line)
                        && event.protocol == 1
                        && event.pid == self.pid
                        && valid_phase(&event.phase)
                        && self.admitted(&event.request_id)
                    {
                        // Only the pinned child can supply shutdown confirmation.
                        if matches!(
                            event.phase.as_str(),
                            "shutdown_confirmed" | "shutdown_unknown"
                        ) {
                            self.value.lock().expect("diagnostic mutex")["shutdown_status"] =
                                json!(if event.phase == "shutdown_confirmed" {
                                    "confirmed"
                                } else {
                                    "unknown"
                                });
                        }
                        self.phase(&event.phase, event.request_id).await;
                    }
                    line.clear();
                    oversized = false;
                } else if line.len() < MAX_LINE && bytes <= MAX_BYTES {
                    line.push(*b);
                } else {
                    oversized = true;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../../desktop-worker/src/lifecycle.rs"]
mod worker_shutdown;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_process_identity_uses_boot_and_creation_ticks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("17")).unwrap();
        std::fs::create_dir_all(d.path().join("sys/kernel/random")).unwrap();
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[19] = "912";
        std::fs::write(
            d.path().join("17/stat"),
            format!("17 (name with ) space) {}", fields.join(" ")),
        )
        .unwrap();
        std::fs::write(
            d.path().join("sys/kernel/random/boot_id"),
            "11111111-1111-4111-8111-111111111111\n",
        )
        .unwrap();
        let p = linux_identity(d.path(), 17).unwrap();
        assert_eq!(p["start_ticks"], 912);
        assert!(interrupted(
            &json!({"parent_identity":p,"worker_exit":null}),
            &json!({"pid":17,"identity_state":"observed","start_ticks":913,"boot_id":"11111111-1111-4111-8111-111111111111"})
        ));
        assert!(!interrupted(
            &json!({"parent_identity":p,"worker_exit":{"reaped":true}}),
            &json!({})
        ));
    }
    #[tokio::test]
    async fn malformed_or_overlong_stderr_never_enters_diagnostics() {
        let op = Uuid::new_v4();
        let t = Tracker::new(op, 17, None);
        let data = format!(
            "{}\n{{\"protocol\":1,\"request_id\":\"{op}\",\"pid\":17,\"phase\":\"portal_request_started\"}}\n{{\"protocol\":1,\"request_id\":\"{op}\",\"pid\":99,\"phase\":\"shutdown_confirmed\"}}\npassword=hidden\n",
            "x".repeat(2000)
        );
        t.read(data.as_bytes()).await;
        let v = t.snapshot();
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
        assert!(!v.to_string().contains("hidden"));
        assert!(!v.to_string().contains("shutdown_confirmed"));
    }
}
