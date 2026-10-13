//! Synchronous prepare/check/post boundary shared by native input and recording tests.
//! The backend must submit immediately, with no task, await or new queue after the gate.
use std::time::{Duration, Instant};

pub(crate) fn held_modifier_groups(keys: &std::collections::BTreeSet<u16>) -> [bool; 4] {
    [[225, 229], [224, 228], [226, 230], [227, 231]]
        .map(|usages| usages.iter().any(|usage| keys.contains(usage)))
}

pub(crate) const INPUT_FRESHNESS: Duration = Duration::from_secs(1);
pub(crate) fn freshness_budget(at: Option<Instant>, now: Instant) -> Duration {
    at.and_then(|at| now.checked_duration_since(at))
        .and_then(|age| INPUT_FRESHNESS.checked_sub(age))
        .unwrap_or(Duration::ZERO)
}
pub(crate) struct InputValidity {
    pub live: bool,
    pub captured: Option<Instant>,
    pub deadline: Instant,
    // Read after any potentially blocking target/permission probes.
    pub now: Instant,
}
impl InputValidity {
    fn allows(&self) -> bool {
        self.live
            && self.now < self.deadline
            && !freshness_budget(self.captured, self.now).is_zero()
    }
}
pub(crate) trait InputBackend {
    type Action;
    type Event;
    type Error;
    fn validity(&self) -> InputValidity;
    fn prepare(&mut self, action: &Self::Action) -> Result<Self::Event, Self::Error>;
    fn post(&mut self, event: Self::Event);
    fn invalidated() -> Self::Error;
}
pub(crate) fn dispatch_input_batch<B: InputBackend>(
    backend: &mut B,
    actions: &[B::Action],
) -> Result<(), B::Error> {
    for action in actions {
        if !backend.validity().allows() {
            return Err(B::invalidated());
        }
        let event = backend.prepare(action)?;
        // Event construction/configuration can block even without an await.
        if !backend.validity().allows() {
            return Err(B::invalidated());
        }
        backend.post(event);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc::{self, Receiver, Sender},
        },
        thread,
    };
    #[test]
    fn modifier_flags_follow_only_owned_held_usages() {
        use std::collections::BTreeSet;
        assert_eq!(held_modifier_groups(&BTreeSet::new()), [false; 4]);
        assert_eq!(held_modifier_groups(&BTreeSet::from([4])), [false; 4]);
        for (usage, group) in [
            (225, 0),
            (229, 0),
            (224, 1),
            (228, 1),
            (226, 2),
            (230, 2),
            (227, 3),
            (231, 3),
        ] {
            let mut expected = [false; 4];
            expected[group] = true;
            assert_eq!(held_modifier_groups(&BTreeSet::from([usage])), expected);
        }
        let mut held = BTreeSet::from([225, 229]);
        held.remove(&225);
        assert_eq!(held_modifier_groups(&held), [true, false, false, false]);
        held.remove(&229);
        assert_eq!(held_modifier_groups(&held), [false; 4]);
    }
    struct State {
        live: AtomicBool,
        millis: AtomicU64,
    }
    struct Recording {
        state: Arc<State>,
        base: Instant,
        deadline_ms: u64,
        validations: Cell<usize>,
        prepared: usize,
        posted: Vec<u8>,
        pause_at: Option<usize>,
        ready: Sender<usize>,
        resume: Receiver<()>,
    }
    impl InputBackend for Recording {
        type Action = u8;
        type Event = u8;
        type Error = &'static str;
        fn validity(&self) -> InputValidity {
            self.validations.set(self.validations.get() + 1);
            InputValidity {
                live: self.state.live.load(Ordering::SeqCst),
                captured: Some(self.base),
                deadline: self.base + Duration::from_millis(self.deadline_ms),
                now: self.base + Duration::from_millis(self.state.millis.load(Ordering::SeqCst)),
            }
        }
        fn prepare(&mut self, action: &u8) -> Result<u8, Self::Error> {
            self.prepared += 1;
            if self.pause_at == Some(self.prepared) {
                self.ready
                    .send(self.prepared)
                    .map_err(|_| "controller_disconnected")?;
                self.resume
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| "pause_timeout")?;
            }
            Ok(*action)
        }
        fn post(&mut self, event: u8) {
            self.posted.push(event);
        }
        fn invalidated() -> Self::Error {
            "invalidated"
        }
    }
    fn paused(
        pause_at: usize,
        deadline_ms: u64,
        invalidate: impl FnOnce(&State),
    ) -> (Recording, Result<(), &'static str>) {
        let state = Arc::new(State {
            live: AtomicBool::new(true),
            millis: AtomicU64::new(0),
        });
        let (ready, notification) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let backend = Recording {
            state: state.clone(),
            base: Instant::now(),
            deadline_ms,
            validations: Cell::new(0),
            prepared: 0,
            posted: Vec::new(),
            pause_at: Some(pause_at),
            ready,
            resume: resumed,
        };
        thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let mut backend = backend;
                let result = dispatch_input_batch(&mut backend, &[1, 2, 3]);
                (backend, result)
            });
            assert_eq!(
                notification.recv_timeout(Duration::from_secs(2)).unwrap(),
                pause_at
            );
            invalidate(&state);
            resume.send(()).unwrap();
            worker.join().unwrap()
        })
    }
    #[test]
    fn input_gate_rechecks_frame_after_preparation() {
        let (b, result) = paused(1, 10_000, |s| s.millis.store(1000, Ordering::SeqCst));
        assert_eq!(result, Err("invalidated"));
        assert_eq!((b.prepared, b.validations.get()), (1, 2));
        assert!(b.posted.is_empty());
    }
    #[test]
    fn input_gate_rechecks_session_after_preparation() {
        let (b, result) = paused(1, 10_000, |s| s.live.store(false, Ordering::SeqCst));
        assert_eq!(result, Err("invalidated"));
        assert_eq!((b.prepared, b.validations.get()), (1, 2));
        assert!(b.posted.is_empty());
    }
    #[test]
    fn input_gate_rechecks_deadline_after_preparation() {
        let (b, result) = paused(1, 500, |s| s.millis.store(500, Ordering::SeqCst));
        assert_eq!(result, Err("invalidated"));
        assert_eq!(b.prepared, 1);
        assert!(b.posted.is_empty());
    }
    #[test]
    fn input_gate_stops_remaining_batch_after_later_invalidation() {
        let (b, result) = paused(2, 10_000, |s| s.live.store(false, Ordering::SeqCst));
        assert_eq!(result, Err("invalidated"));
        assert_eq!((b.prepared, b.validations.get()), (2, 4));
        assert_eq!(b.posted, vec![1]);
    }
    #[test]
    fn input_gate_valid_batch_submits_all() {
        let state = Arc::new(State {
            live: AtomicBool::new(true),
            millis: AtomicU64::new(0),
        });
        let (ready, _) = mpsc::channel();
        let (_, resume) = mpsc::channel();
        let mut b = Recording {
            state,
            base: Instant::now(),
            deadline_ms: 10_000,
            validations: Cell::new(0),
            prepared: 0,
            posted: Vec::new(),
            pause_at: None,
            ready,
            resume,
        };
        assert_eq!(dispatch_input_batch(&mut b, &[1, 2, 3]), Ok(()));
        assert_eq!((b.prepared, b.validations.get()), (3, 6));
        assert_eq!(b.posted, vec![1, 2, 3]);
    }
}
