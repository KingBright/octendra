//! Monotonic leases for input held by one Worker session.
//! Call only after checking the existing frame, owner, epoch and input sequence.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

pub const HOLD_FRESHNESS: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Control {
    Key(u16),
    Button(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldError {
    InvalidSequence,
    Expired,
}

#[derive(Debug, Default)]
pub struct HoldGuard {
    renewed: BTreeMap<Control, Instant>,
    message_controls: BTreeMap<Control, Instant>,
    sequence: u64,
    stopped: bool,
}
impl HoldGuard {
    /// Earliest deadline; unrelated messages never extend another control.
    pub fn expires_at(&self) -> Option<Instant> {
        self.renewed
            .values()
            .map(|at| at.checked_add(HOLD_FRESHNESS).unwrap_or(*at))
            .min()
    }
    pub fn budget(&self, deadline: Instant, now: Instant) -> Duration {
        if self.stopped {
            return Duration::ZERO;
        }
        let mut remaining = deadline
            .saturating_duration_since(now)
            .min(Duration::from_secs(5));
        for at in self.renewed.values() {
            remaining = remaining.min(
                now.checked_duration_since(*at)
                    .and_then(|age| HOLD_FRESHNESS.checked_sub(age))
                    .unwrap_or(Duration::ZERO),
            );
        }
        remaining
    }
    /// A validated new state message consumes its sequence before any OS effect.
    /// Renewal is bookkeeping, never another key-down/button-down event.
    pub fn renew(
        &mut self,
        sequence: u64,
        deadline: Instant,
        now: Instant,
        controls: &[Control],
    ) -> Result<(), HoldError> {
        if self.budget(deadline, now).is_zero() {
            self.stopped = true;
            return Err(HoldError::Expired);
        }
        if self.sequence.checked_add(1) != Some(sequence) {
            return Err(HoldError::InvalidSequence);
        }
        self.sequence = sequence;
        self.message_controls.clear();
        for control in controls {
            self.renewed.insert(*control, now);
            self.message_controls.insert(*control, now);
        }
        Ok(())
    }
    /// Re-press in the same batch uses its original validated message deadline.
    pub fn posted(&mut self, control: Control) {
        if !self.stopped
            && let Some(at) = self.message_controls.get(&control)
        {
            self.renewed.entry(control).or_insert(*at);
        }
    }
    /// Forget only after posting this session's owned release.
    pub fn released(&mut self, control: Control) {
        self.renewed.remove(&control);
    }
    pub fn stop(&mut self) {
        self.stopped = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    struct Recording {
        guard: HoldGuard,
        owned: BTreeSet<Control>,
        events: Vec<(Control, bool)>,
        deadline: Instant,
    }
    impl Recording {
        fn down(&mut self, sequence: u64, now: Instant, control: Control) -> Result<(), HoldError> {
            self.guard.renew(sequence, self.deadline, now, &[control])?;
            if self.owned.insert(control) {
                self.events.push((control, true));
            }
            Ok(())
        }
        fn stop_due(&mut self, now: Instant) {
            if self.guard.budget(self.deadline, now).is_zero() {
                self.guard.stop();
                for control in std::mem::take(&mut self.owned) {
                    self.events.push((control, false));
                }
            }
        }
    }
    fn recording(now: Instant) -> Recording {
        Recording {
            guard: HoldGuard::default(),
            owned: BTreeSet::new(),
            events: vec![],
            deadline: now + Duration::from_secs(30),
        }
    }
    #[test]
    fn missing_key_up_and_mouse_up_stop_only_owned_controls() {
        let t = Instant::now();
        let mut r = recording(t);
        r.down(1, t, Control::Key(4)).unwrap();
        r.down(2, t, Control::Button(0)).unwrap();
        r.stop_due(t + Duration::from_millis(999));
        assert_eq!(r.events.len(), 2);
        r.stop_due(t + HOLD_FRESHNESS);
        assert_eq!(
            r.events,
            vec![
                (Control::Key(4), true),
                (Control::Button(0), true),
                (Control::Key(4), false),
                (Control::Button(0), false)
            ]
        );
        assert!(!r.events.iter().any(|(c, _)| *c == Control::Key(5)));
    }
    #[test]
    fn fresh_down_state_renews_without_replaying_down_or_one_shot() {
        let t = Instant::now();
        let mut r = recording(t);
        r.down(1, t, Control::Key(4)).unwrap();
        r.down(2, t + Duration::from_millis(750), Control::Key(4))
            .unwrap();
        assert_eq!(r.events, vec![(Control::Key(4), true)]);
        r.stop_due(t + HOLD_FRESHNESS);
        assert_eq!(r.events.len(), 1);
        r.stop_due(t + Duration::from_millis(1750));
        assert_eq!(r.events.len(), 2);
    }
    #[test]
    fn duplicate_old_or_skipped_sequence_never_renews() {
        let t = Instant::now();
        let mut r = recording(t);
        r.down(1, t, Control::Key(4)).unwrap();
        for sequence in [0, 1, 3] {
            assert_eq!(
                r.down(sequence, t + Duration::from_millis(900), Control::Key(4)),
                Err(HoldError::InvalidSequence)
            );
        }
        r.stop_due(t + HOLD_FRESHNESS);
        assert_eq!(r.events.len(), 2);
    }
    #[test]
    fn unrelated_messages_and_mouse_state_do_not_renew_held_key() {
        let t = Instant::now();
        let mut r = recording(t);
        r.down(1, t, Control::Key(4)).unwrap();
        r.guard
            .renew(2, r.deadline, t + Duration::from_millis(700), &[])
            .unwrap();
        r.down(3, t + Duration::from_millis(900), Control::Button(0))
            .unwrap();
        r.stop_due(t + HOLD_FRESHNESS);
        assert!(r.owned.is_empty());
    }
    #[test]
    fn expiry_is_terminal_even_after_releases_and_new_sequence() {
        let t = Instant::now();
        let mut r = recording(t);
        r.down(1, t, Control::Key(4)).unwrap();
        r.stop_due(t + HOLD_FRESHNESS);
        r.guard.released(Control::Key(4));
        assert_eq!(
            r.down(2, t + Duration::from_millis(1001), Control::Key(4)),
            Err(HoldError::Expired)
        );
        assert_eq!(r.events.len(), 2);
    }
    #[test]
    fn release_then_repress_in_one_batch_keeps_original_deadline() {
        let t = Instant::now();
        let mut g = HoldGuard::default();
        let d = t + Duration::from_secs(30);
        g.renew(1, d, t, &[Control::Key(4)]).unwrap();
        g.released(Control::Key(4));
        g.posted(Control::Key(4));
        assert_eq!(
            g.budget(d, t + Duration::from_millis(999)),
            Duration::from_millis(1)
        );
        assert_eq!(g.budget(d, t + HOLD_FRESHNESS), Duration::ZERO);
        g.posted(Control::Key(5));
        assert_eq!(g.expires_at(), Some(t + HOLD_FRESHNESS));
    }
    #[test]
    fn session_deadline_and_future_clock_fail_closed() {
        let t = Instant::now();
        let mut guard = HoldGuard::default();
        guard
            .renew(1, t + Duration::from_millis(100), t, &[Control::Key(4)])
            .unwrap();
        assert_eq!(
            guard.budget(t + Duration::from_millis(100), t),
            Duration::from_millis(100)
        );
        assert_eq!(
            guard.budget(
                t + Duration::from_millis(100),
                t + Duration::from_millis(100)
            ),
            Duration::ZERO
        );
        assert_eq!(
            guard.budget(t + Duration::from_secs(30), t - Duration::from_nanos(1)),
            Duration::ZERO
        );
    }
}
