//! SSH-like zero-DB ephemeral channel for stateless read/observation operations.
//! Active and recently completed entries live strictly in bounded per-device RAM
//! ring buffers and naturally age out without SQLite transactions or fsyncs.
use crate::{gateway::Job, hash, scheduler::Lane};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicI64, Ordering},
    },
};

pub(crate) const MAX_ACTIVE_PER_DEVICE: usize = 256;
pub(crate) const MAX_COMPLETED_PER_DEVICE: usize = 256;
pub(crate) const ENTRY_TTL_SECS: i64 = 180;
pub(crate) const DISPATCH_LEASE_SECS: i64 = 30;

pub(crate) fn is_channel_tool(tool: &str) -> bool {
    matches!(
        tool,
        "code_read"
            | "code_list"
            | "code_search"
            | "code_symbols"
            | "code_diff"
            | "workspace_context"
            | "terminal_read"
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveState {
    Queued,
    Dispatched,
}

struct ActiveEntry {
    job: Job,
    state: ActiveState,
    created_at: i64,
    updated_at: i64,
    queued_ms: i64,
    dispatched_ms: Option<i64>,
}

struct CompletedEntry {
    job: Job,
    result: Value,
    digest: String,
    updated_at: i64,
    queued_ms: i64,
    dispatched_ms: Option<i64>,
    result_ms: i64,
}

#[derive(Default)]
struct Inner {
    active: VecDeque<ActiveEntry>,
    completed: VecDeque<CompletedEntry>,
}

impl Inner {
    fn evict(&mut self, now_s: i64) {
        let cutoff = now_s.saturating_sub(ENTRY_TTL_SECS);
        self.active.retain(|entry| {
            entry.updated_at > cutoff && entry.created_at > cutoff - ENTRY_TTL_SECS
        });
        while self
            .completed
            .front()
            .is_some_and(|entry| entry.updated_at <= cutoff)
        {
            self.completed.pop_front();
        }
        while self.completed.len() > MAX_COMPLETED_PER_DEVICE {
            self.completed.pop_front();
        }
    }
}

#[derive(Default)]
pub(crate) struct DeviceEphemeral {
    inner: Mutex<Inner>,
    live_poll_at: AtomicI64,
}

pub(crate) enum CommitOutcome {
    Accepted { duplicate: bool },
    Conflict,
    NotFound,
}

pub(crate) struct EphemeralSnapshot {
    pub job: Job,
    pub result: Option<Value>,
    pub state: &'static str,
    pub updated_at: i64,
    pub queued_ms: i64,
    pub dispatched_ms: Option<i64>,
    pub result_ms: Option<i64>,
}

impl EphemeralSnapshot {
    pub fn gateway_timing_json(&self) -> Value {
        let queue_ms = self
            .dispatched_ms
            .and_then(|d| d.checked_sub(self.queued_ms))
            .filter(|v| *v >= 0);
        let dispatch_to_result_ms = self
            .dispatched_ms
            .zip(self.result_ms)
            .and_then(|(a, b)| b.checked_sub(a))
            .filter(|v| *v >= 0);
        json!({
            "clock": "gateway_unix_ms_same_host",
            "queued_at_ms": self.queued_ms,
            "dispatched_at_ms": self.dispatched_ms,
            "result_at_ms": self.result_ms,
            "queue_ms": queue_ms,
            "dispatch_to_result_ms": dispatch_to_result_ms,
            "ephemeral_channel": true,
            "note": "gateway segments use one host clock; never subtract these timestamps from agent_monotonic timings"
        })
    }
}

impl DeviceEphemeral {
    pub fn note_live_poll(&self, now_s: i64) {
        self.live_poll_at.store(now_s, Ordering::Release);
    }

    pub fn is_live_poller_active(&self, now_s: i64) -> bool {
        let last = self.live_poll_at.load(Ordering::Acquire);
        last > 0 && (0..45).contains(&(now_s - last))
    }

    pub fn enqueue(&self, job: Job, now_s: i64, now_ms: i64) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("ephemeral channel poisoned"))?;
        inner.evict(now_s);
        ensure!(
            inner.active.len() < MAX_ACTIVE_PER_DEVICE,
            "ephemeral_channel_backpressure: concurrent read queue full on device"
        );
        inner.active.push_back(ActiveEntry {
            job,
            state: ActiveState::Queued,
            created_at: now_s,
            updated_at: now_s,
            queued_ms: now_ms,
            dispatched_ms: None,
        });
        Ok(())
    }

    pub fn has_claimable(
        &self,
        active_ops: &[String],
        scopes: &[String],
        lanes: &[Lane],
        now_s: i64,
    ) -> bool {
        if !scopes.iter().any(|s| s == "code:read") {
            return false;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        inner.evict(now_s);
        inner.active.iter().any(|entry| {
            let lane = Lane::for_tool(&entry.job.tool);
            lanes.contains(&lane)
                && !active_ops.iter().any(|id| id == &entry.job.id)
                && (entry.state == ActiveState::Queued
                    || entry.updated_at < now_s - DISPATCH_LEASE_SECS)
        })
    }

    pub fn claim(
        &self,
        active_ops: &[String],
        scopes: &[String],
        lanes: &[Lane],
        now_s: i64,
        now_ms: i64,
    ) -> Option<Job> {
        if !scopes.iter().any(|s| s == "code:read") {
            return None;
        }
        let mut inner = self.inner.lock().ok()?;
        inner.evict(now_s);
        for entry in &mut inner.active {
            let lane = Lane::for_tool(&entry.job.tool);
            if lanes.contains(&lane)
                && !active_ops.iter().any(|id| id == &entry.job.id)
                && (entry.state == ActiveState::Queued
                    || entry.updated_at < now_s - DISPATCH_LEASE_SECS)
            {
                entry.state = ActiveState::Dispatched;
                entry.updated_at = now_s;
                entry.dispatched_ms.get_or_insert(now_ms);
                return Some(entry.job.clone());
            }
        }
        None
    }

    pub fn renew(&self, active_ops: &[String], now_s: i64) {
        if active_ops.is_empty() {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for entry in &mut inner.active {
            if entry.state == ActiveState::Dispatched
                && active_ops.iter().any(|id| id == &entry.job.id)
            {
                entry.updated_at = now_s;
            }
        }
    }

    pub fn complete(
        &self,
        device: &str,
        operation_id: &str,
        result: &Value,
        now_s: i64,
        now_ms: i64,
    ) -> CommitOutcome {
        let Ok(mut inner) = self.inner.lock() else {
            return CommitOutcome::NotFound;
        };
        inner.evict(now_s);
        let digest = hash(result.to_string());
        if let Some(pos) = inner
            .active
            .iter()
            .position(|entry| entry.job.id == operation_id)
        {
            if inner.active[pos].job.device_id != device {
                return CommitOutcome::Conflict;
            }
            let entry = inner.active.remove(pos).expect("checked index");
            inner.completed.push_back(CompletedEntry {
                job: entry.job,
                result: result.clone(),
                digest,
                updated_at: now_s,
                queued_ms: entry.queued_ms,
                dispatched_ms: entry.dispatched_ms.or(Some(now_ms)),
                result_ms: now_ms,
            });
            while inner.completed.len() > MAX_COMPLETED_PER_DEVICE {
                inner.completed.pop_front();
            }
            return CommitOutcome::Accepted { duplicate: false };
        }
        if let Some(existing) = inner
            .completed
            .iter()
            .find(|entry| entry.job.id == operation_id)
        {
            if existing.job.device_id != device || existing.digest != digest {
                return CommitOutcome::Conflict;
            }
            return CommitOutcome::Accepted { duplicate: true };
        }
        CommitOutcome::NotFound
    }

    pub fn snapshot(&self, operation_id: &str, now_s: i64) -> Option<EphemeralSnapshot> {
        let mut inner = self.inner.lock().ok()?;
        inner.evict(now_s);
        if let Some(done) = inner
            .completed
            .iter()
            .find(|entry| entry.job.id == operation_id)
        {
            return Some(EphemeralSnapshot {
                job: done.job.clone(),
                result: Some(done.result.clone()),
                state: "done",
                updated_at: done.updated_at,
                queued_ms: done.queued_ms,
                dispatched_ms: done.dispatched_ms,
                result_ms: Some(done.result_ms),
            });
        }
        if let Some(active) = inner
            .active
            .iter()
            .find(|entry| entry.job.id == operation_id)
        {
            return Some(EphemeralSnapshot {
                job: active.job.clone(),
                result: None,
                state: match active.state {
                    ActiveState::Queued => "queued",
                    ActiveState::Dispatched => "dispatched",
                },
                updated_at: active.updated_at,
                queued_ms: active.queued_ms,
                dispatched_ms: active.dispatched_ms,
                result_ms: None,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_job(id: &str, tool: &str) -> Job {
        Job {
            ui_context: None,
            id: id.into(),
            device_id: "dev-1".into(),
            owner: "owner".into(),
            tool: tool.into(),
            arguments: json!({"workspace_id": "dev-1:default"}),
        }
    }

    #[test]
    fn ephemeral_ring_naturally_evicts_by_count_and_age() {
        let ch = DeviceEphemeral::default();
        let scopes = vec!["code:read".to_owned()];
        let lanes = vec![Lane::Read, Lane::Control];
        for idx in 0..(MAX_COMPLETED_PER_DEVICE + 10) {
            let id = format!("op-{idx}");
            ch.enqueue(sample_job(&id, "code_read"), 1000, 1_000_000)
                .unwrap();
            let claimed = ch.claim(&[], &scopes, &lanes, 1000, 1_000_005).unwrap();
            assert_eq!(claimed.id, id);
            assert!(matches!(
                ch.complete("dev-1", &id, &json!({"ok": idx}), 1000, 1_000_012),
                CommitOutcome::Accepted { duplicate: false }
            ));
        }
        // Oldest 10 entries were naturally evicted by the 256-slot ring cap.
        assert!(ch.snapshot("op-0", 1001).is_none());
        assert!(ch.snapshot("op-9", 1001).is_none());
        let latest = format!("op-{}", MAX_COMPLETED_PER_DEVICE + 9);
        let snap = ch.snapshot(&latest, 1001).unwrap();
        assert_eq!(snap.state, "done");
        assert!(matches!(
            ch.complete(
                "dev-1",
                &latest,
                &json!({"ok": MAX_COMPLETED_PER_DEVICE + 9}),
                1001,
                1_000_020
            ),
            CommitOutcome::Accepted { duplicate: true }
        ));
        assert!(matches!(
            ch.complete(
                "dev-1",
                &latest,
                &json!({"ok": "tampered"}),
                1001,
                1_000_020
            ),
            CommitOutcome::Conflict
        ));
        // Natural TTL expiration after ENTRY_TTL_SECS without background sweeps.
        assert!(ch.snapshot(&latest, 1000 + ENTRY_TTL_SECS + 1).is_none());
    }
}
