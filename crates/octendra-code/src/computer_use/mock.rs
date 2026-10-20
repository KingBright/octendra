//! Deterministic memory executor; compiled only for offline unit tests.
use super::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Input {
    Pointer(SourcePoint),
    KeyDown(u16),
    KeyUp(u16),
    ButtonDown(Button),
    ButtonUp(Button),
    Wheel(i16, i16),
}
#[derive(Default)]
pub(super) struct Backend {
    pub events: Vec<(Uuid, Input)>,
    pub fail_at: Option<usize>,
}
impl Backend {
    fn apply(&mut self, epoch: Uuid, input: Input) -> Result<(), ErrorCode> {
        self.events.push((epoch, input));
        if self.fail_at == Some(self.events.len()) {
            self.fail_at = None;
            Err(ErrorCode::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
}
struct Session {
    binding: Binding,
    expires_at_ms: u64,
    source_revision: u64,
    latest: Option<FrameRef>,
    frame_sequence: u64,
    next_action: u64,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<Button>,
}
struct State {
    backend: Arc<Mutex<Backend>>,
    sessions: BTreeMap<String, Session>,
    source_revision: u64,
    mapping: FrameMapping,
    now_ms: u64,
}
impl State {
    fn release(&mut self, session: Session) {
        let mut backend = self.backend.lock().unwrap();
        // Only holds tracked by this epoch are released. Never release all desktop keys.
        for key in session.keys {
            let _ = backend.apply(session.binding.epoch, Input::KeyUp(key));
        }
        for button in session.buttons {
            let _ = backend.apply(session.binding.epoch, Input::ButtonUp(button));
        }
    }
    fn close(&mut self, desktop: &str) {
        if let Some(session) = self.sessions.remove(desktop) {
            self.release(session);
        }
    }
    fn expire(&mut self) {
        let expired: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.expires_at_ms <= self.now_ms)
            .map(|(d, _)| d.clone())
            .collect();
        for desktop in expired {
            self.close(&desktop);
        }
    }
    fn checked(&self, binding: &Binding) -> Result<&Session, ErrorCode> {
        let session = self
            .sessions
            .get(&binding.identity.desktop_id)
            .ok_or(ErrorCode::SessionClosed)?;
        if session.binding != *binding {
            return Err(ErrorCode::IdentityMismatch);
        }
        Ok(session)
    }
}
impl Drop for State {
    fn drop(&mut self) {
        for (_, session) in std::mem::take(&mut self.sessions) {
            self.release(session);
        }
    }
}
#[derive(Clone)]
pub(super) struct Executor(Arc<Mutex<State>>);
impl Executor {
    pub fn new(mapping: FrameMapping) -> (Self, Arc<Mutex<Backend>>) {
        let backend = Arc::new(Mutex::new(Backend::default()));
        let state = State {
            backend: backend.clone(),
            sessions: BTreeMap::new(),
            source_revision: 1,
            mapping,
            now_ms: 0,
        };
        (Self(Arc::new(Mutex::new(state))), backend)
    }
    pub fn advance(&self, ms: u64) {
        let mut state = self.0.lock().unwrap();
        state.now_ms = state.now_ms.checked_add(ms).unwrap();
        state.expire();
    }
    pub fn disconnect(&self, identity: &Identity) {
        let mut state = self.0.lock().unwrap();
        if state
            .sessions
            .get(&identity.desktop_id)
            .is_some_and(|session| session.binding.identity == *identity)
        {
            state.close(&identity.desktop_id);
        }
    }
    pub fn change_source(&self, mapping: FrameMapping) {
        let mut state = self.0.lock().unwrap();
        for desktop in state.sessions.keys().cloned().collect::<Vec<_>>() {
            state.close(&desktop);
        }
        state.source_revision = state.source_revision.checked_add(1).unwrap();
        state.mapping = mapping;
    }
    pub fn run(&self, request: Request) -> Result<Response, ErrorCode> {
        let mut state = self.0.lock().unwrap();
        state.expire();
        match request {
            Request::Capabilities {} => {
                let capabilities = Capabilities {
                    available: true,
                    backend: "mock".into(),
                    ..Capabilities::default()
                };
                Ok(Response::Capabilities { capabilities })
            }
            Request::Open { identity, lease_ms } => {
                if identity.owner.is_empty()
                    || identity.owner.len() > 128
                    || identity.desktop_id.is_empty()
                    || identity.desktop_id.len() > 128
                    || identity.task_id.is_nil()
                    || identity.device_id.is_nil()
                    || !(100..=MAX_LEASE_MS).contains(&lease_ms)
                {
                    return Err(ErrorCode::InvalidRequest);
                }
                if state.sessions.contains_key(&identity.desktop_id) {
                    return Err(ErrorCode::DesktopBusy);
                }
                let binding = Binding {
                    identity,
                    epoch: Uuid::new_v4(),
                };
                let expires_at_ms = state
                    .now_ms
                    .checked_add(u64::from(lease_ms))
                    .ok_or(ErrorCode::InvalidRequest)?;
                let session = Session {
                    binding: binding.clone(),
                    expires_at_ms,
                    source_revision: state.source_revision,
                    latest: None,
                    frame_sequence: 0,
                    next_action: 1,
                    keys: BTreeSet::new(),
                    buttons: BTreeSet::new(),
                };
                state
                    .sessions
                    .insert(binding.identity.desktop_id.clone(), session);
                Ok(Response::Opened {
                    binding,
                    expires_at_ms,
                })
            }
            Request::Observe { binding } => {
                state.checked(&binding)?;
                let mapping = state.mapping.clone();
                let session = state
                    .sessions
                    .get_mut(&binding.identity.desktop_id)
                    .unwrap();
                session.frame_sequence += 1;
                let frame = FrameRef {
                    binding,
                    source_revision: session.source_revision,
                    frame_sequence: session.frame_sequence,
                    mapping,
                };
                session.latest = Some(frame.clone());
                Ok(Response::Observed { frame })
            }
            Request::Close { binding } => {
                state.checked(&binding)?;
                state.close(&binding.identity.desktop_id);
                Ok(Response::Closed {})
            }
            Request::Act {
                frame,
                sequence,
                actions,
            } => {
                let desktop = &frame.binding.identity.desktop_id;
                let session = state.checked(&frame.binding)?;
                if session.latest.as_ref() != Some(&frame)
                    || frame.source_revision != state.source_revision
                {
                    return Err(ErrorCode::StaleFrame);
                }
                if sequence != session.next_action || sequence == u64::MAX {
                    return Err(ErrorCode::InvalidSequence);
                }
                let inputs =
                    validate_actions(&frame.mapping, &actions, &session.keys, &session.buttons)?;
                let backend = state.backend.clone();
                let session = state.sessions.get_mut(desktop).unwrap();
                for input in inputs {
                    // A failed press may already have reached the backend: track first.
                    match input {
                        Input::KeyDown(key) => {
                            session.keys.insert(key);
                        }
                        Input::ButtonDown(button) => {
                            session.buttons.insert(button);
                        }
                        _ => {}
                    }
                    if backend
                        .lock()
                        .unwrap()
                        .apply(session.binding.epoch, input.clone())
                        .is_err()
                    {
                        state.close(desktop);
                        return Err(ErrorCode::OutcomeUnknown);
                    }
                    match input {
                        Input::KeyUp(key) => {
                            session.keys.remove(&key);
                        }
                        Input::ButtonUp(button) => {
                            session.buttons.remove(&button);
                        }
                        _ => {}
                    }
                }
                session.next_action += 1;
                // Require a new observation between action batches.
                session.latest = None;
                Ok(Response::Accepted {
                    sequence,
                    accepted_actions: actions.len(),
                    effect_confirmed: false,
                })
            }
        }
    }
    /// Reuses the exact production operation journal, not a second deduplication map.
    pub async fn execute(
        &self,
        agent: &crate::agent::Agent,
        job: &crate::gateway::Job,
    ) -> anyhow::Result<Value> {
        self.execute_with_delivery(agent, job, None).await
    }
    pub async fn execute_with_delivery(
        &self,
        agent: &crate::agent::Agent,
        job: &crate::gateway::Job,
        delivery: Option<&crate::delivery::Delivery>,
    ) -> anyhow::Result<Value> {
        agent
            .execute_journaled(job, delivery, |_| async {
                anyhow::ensure!(job.tool == "computer_use_mock", "mock tool required");
                let request: Request = serde_json::from_value(job.arguments["request"].clone())?;
                if let Some(identity) = request.identity() {
                    anyhow::ensure!(
                        identity.owner == job.owner
                            && identity.device_id.to_string() == job.device_id
                            && Some(identity.task_id.to_string().as_str())
                                == job.arguments["task_id"].as_str(),
                        "desktop identity mismatch"
                    );
                }
                Ok(match self.run(request) {
                    Ok(response) => serde_json::to_value(response)?,
                    Err(error) => json!({
                        "error": error,
                        "automatic_replay_safe": false,
                        "next_action": if error == ErrorCode::OutcomeUnknown {
                            "operation_get_original"
                        } else { "inspect_rejection" }
                    }),
                })
            })
            .await
    }
}
fn validate_actions(
    mapping: &FrameMapping,
    actions: &[Action],
    held_keys: &BTreeSet<u16>,
    held_buttons: &BTreeSet<Button>,
) -> Result<Vec<Input>, ErrorCode> {
    if actions.is_empty() || actions.len() > MAX_ACTIONS {
        return Err(ErrorCode::InvalidActions);
    }
    let mut keys = held_keys.clone();
    let mut buttons = held_buttons.clone();
    actions
        .iter()
        .map(|action| {
            Ok(match *action {
                Action::PointerMove { x, y } => Input::Pointer(mapping.map(x, y)?),
                Action::KeyDown { usage } if (4..=0xe7).contains(&usage) && keys.insert(usage) => {
                    Input::KeyDown(usage)
                }
                Action::KeyUp { usage } if keys.remove(&usage) => Input::KeyUp(usage),
                Action::ButtonDown { button } if buttons.insert(button) => {
                    Input::ButtonDown(button)
                }
                Action::ButtonUp { button } if buttons.remove(&button) => Input::ButtonUp(button),
                Action::Wheel {
                    horizontal,
                    vertical,
                } if horizontal.unsigned_abs() <= 120 && vertical.unsigned_abs() <= 120 => {
                    Input::Wheel(horizontal, vertical)
                }
                _ => return Err(ErrorCode::InvalidActions),
            })
        })
        .collect()
}
