use super::*;
use std::sync::{Arc, Mutex};

#[test]
fn windows_staging_texture_cannot_exceed_the_approved_pixel_budget() {
    assert!(bounded_texture(100, 100, 40000));
    assert!(!bounded_texture(100, 101, 40000));
    assert!(!bounded_texture(0, 100, 40000));
    assert!(!bounded_texture(u32::MAX, u32::MAX, usize::MAX));
    assert!(!bounded_texture(8192, 8192, usize::MAX));
}

#[test]
fn windows_only_explicit_standard_controls_admit_semantic_activation() {
    assert_eq!(activation(50000), Some(Activation::Invoke));
    assert_eq!(activation(50002), Some(Activation::Toggle));
    assert_eq!(activation(50007), Some(Activation::Select));
    assert_eq!(activation(50013), Some(Activation::Select));
    for id in [-1, 0, 50004, 50025, 50032, 50033, i32::MAX] {
        assert_eq!(activation(id), None);
    }
}

#[test]
fn windows_slow_startup_consumes_authorization_and_clock_reversal_never_extends_it() {
    assert_eq!(
        approval_budget(1100, 100),
        Some(Duration::from_millis(1000))
    );
    assert_eq!(
        approval_budget(1100, 1000),
        Some(Duration::from_millis(100))
    );
    assert_eq!(approval_budget(1100, 1100), None);
    assert_eq!(approval_budget(1100, 1101), None);
    assert_eq!(approval_budget(30100, 50), None);
    assert_eq!(approval_budget(u64::MAX, 0), None);
}
#[test]
fn windows_input_gate_rechecks_after_preparation_and_rejects_future_frames() {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(5);
    assert!(valid_input(true, start, deadline, start));
    assert!(!valid_input(
        true,
        start,
        deadline,
        start + Duration::from_secs(1)
    ));
    assert!(!valid_input(true, start, deadline, deadline));
    assert!(!valid_input(false, start, deadline, start));
    assert!(!valid_input(
        true,
        start + Duration::from_millis(1),
        deadline,
        start
    ));
}

#[test]
fn windows_authority_identity_and_acl_policy_are_platform_bound() {
    let a = WindowAuthority {
        owner_sid: "S-1-5-21-fixture".into(),
        session_id: 2,
        process_id: 3,
        process_created_100ns: 4,
        window_id: 5,
        scope_key: "fixture".into(),
        scope_value: 6,
    };
    assert_eq!(a.desktop_id(), "windows:2:3:4:5");
    assert!(!file_grant_allowed(false, true, 0x1));
    for mask in [
        0x2, 0x4, 0x10, 0x40, 0x100, 0x10000, 0x40000, 0x80000, 0x10000000, 0x40000000,
    ] {
        assert!(!file_grant_allowed(false, false, mask));
        assert!(file_grant_allowed(true, true, mask));
    }
    assert!(file_grant_allowed(
        false,
        false,
        0x20000000 | 0x20000 | 0x1 | 0x20
    ));
}

#[derive(Default)]
struct Calls {
    open: usize,
    capture: usize,
    input: usize,
    close: usize,
}
struct Recording {
    calls: Arc<Mutex<Calls>>,
    live: bool,
    fail_input: bool,
    stale: bool,
    bad_pixels: bool,
}
impl Backend for Recording {
    fn live(&self) -> bool {
        self.live
    }
    fn open(&mut self) -> Result<(), ErrorCode> {
        self.calls.lock().unwrap().open += 1;
        Ok(())
    }
    fn capture(&mut self, _: usize) -> Result<Pixels, ErrorCode> {
        self.calls.lock().unwrap().capture += 1;
        let bytes = vec![0; 16];
        Ok(Pixels {
            metadata: RawPixels {
                encoding: "rgba8".into(),
                size: [2, 2],
                color: [0; 4],
                plane_lengths: vec![16],
                plane_sha256: vec![if self.bad_pixels {
                    "0".repeat(64)
                } else {
                    crate::hash(&bytes)
                }],
            },
            bytes,
            captured: Instant::now()
                - if self.stale {
                    Duration::from_secs(2)
                } else {
                    Duration::ZERO
                },
        })
    }
    fn invoke(&mut self, _: SourcePoint, _: Instant, _: Instant) -> Result<(), ErrorCode> {
        self.calls.lock().unwrap().input += 1;
        if self.fail_input {
            Err(ErrorCode::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    fn close(&mut self) -> bool {
        self.calls.lock().unwrap().close += 1;
        true
    }
}
fn fixture() -> (Worker<Recording>, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let id = Identity {
        owner: "fixture".into(),
        task_id: Uuid::new_v4(),
        device_id: Uuid::new_v4(),
        desktop_id: "window".into(),
    };
    (
        Worker::new(
            Recording {
                calls: calls.clone(),
                live: true,
                fail_input: false,
                stale: false,
                bad_pixels: false,
            },
            id,
            Instant::now() + Duration::from_secs(10),
            [2, 2],
            [10, 20],
            1024,
        ),
        calls,
    )
}
fn open(w: &mut Worker<Recording>) -> Binding {
    match w
        .handle(Request::Open {
            identity: w.identity.clone(),
            lease_ms: 5000,
        })
        .0
        .unwrap()
    {
        Response::Opened { binding, .. } => binding,
        _ => panic!(),
    }
}
fn frame(w: &mut Worker<Recording>, binding: &Binding) -> FrameRef {
    let (r, p) = w.handle(Request::Observe {
        binding: binding.clone(),
    });
    assert_eq!(p.unwrap().bytes.len(), 16);
    match r.unwrap() {
        Response::Observed { frame } => frame,
        _ => panic!(),
    }
}
fn actions() -> Vec<Action> {
    vec![
        Action::PointerMove { x: 1., y: 1. },
        Action::ButtonDown {
            button: Button::Left,
        },
        Action::ButtonUp {
            button: Button::Left,
        },
    ]
}
#[test]
fn windows_single_window_pixels_and_standard_control_gesture_dispatch() {
    let (mut w, c) = fixture();
    let b = open(&mut w);
    let f = frame(&mut w, &b);
    assert!(matches!(
        w.handle(Request::Act {
            frame: f,
            sequence: 1,
            actions: actions()
        })
        .0
        .unwrap(),
        Response::Accepted {
            accepted_actions: 3,
            effect_confirmed: false,
            ..
        }
    ));
    assert_eq!(c.lock().unwrap().input, 1);
    assert!(matches!(
        w.handle(Request::Close { binding: b }).0,
        Ok(Response::Closed {})
    ));
    assert!(c.lock().unwrap().close > 0);
}
#[test]
fn windows_foreign_task_and_epoch_rejected_before_backend_effect() {
    let (mut w, c) = fixture();
    let mut foreign = w.identity.clone();
    foreign.task_id = Uuid::new_v4();
    assert_eq!(
        w.handle(Request::Open {
            identity: foreign,
            lease_ms: 5000
        })
        .0,
        Err(ErrorCode::IdentityMismatch)
    );
    assert_eq!(c.lock().unwrap().open, 0);
    let mut b = open(&mut w);
    b.epoch = Uuid::new_v4();
    assert_eq!(
        w.handle(Request::Observe { binding: b }).0,
        Err(ErrorCode::IdentityMismatch)
    );
    assert_eq!(c.lock().unwrap().capture, 0);
}
#[test]
fn windows_raw_key_drag_wheel_and_partial_click_never_reach_backend() {
    for unsupported in [
        vec![Action::KeyDown { usage: 4 }],
        vec![Action::Wheel {
            horizontal: 0,
            vertical: 1,
        }],
        vec![
            Action::PointerMove { x: 0., y: 0. },
            Action::ButtonDown {
                button: Button::Left,
            },
        ],
        vec![
            Action::PointerMove { x: 0., y: 0. },
            Action::ButtonDown {
                button: Button::Right,
            },
            Action::ButtonUp {
                button: Button::Right,
            },
        ],
    ] {
        let (mut w, c) = fixture();
        let b = open(&mut w);
        let f = frame(&mut w, &b);
        assert_eq!(
            w.handle(Request::Act {
                frame: f,
                sequence: 1,
                actions: unsupported
            })
            .0,
            Err(ErrorCode::InvalidActions)
        );
        assert_eq!(c.lock().unwrap().input, 0);
    }
}
#[test]
fn windows_stale_or_changed_frame_and_sequence_rejected_without_input() {
    for mode in 0..4 {
        let (mut w, c) = fixture();
        let b = open(&mut w);
        w.backend.stale = mode == 0;
        let mut f = frame(&mut w, &b);
        if mode == 1 {
            f.source_revision += 1;
        }
        let sequence = if mode == 2 { 2 } else { 1 };
        if mode == 3 {
            w.backend.live = false;
        }
        assert!(
            w.handle(Request::Act {
                frame: f,
                sequence,
                actions: actions()
            })
            .0
            .is_err()
        );
        assert_eq!(c.lock().unwrap().input, 0);
    }
}
#[test]
fn windows_partial_unknown_effect_poisoned_and_never_replayed() {
    let (mut w, c) = fixture();
    let b = open(&mut w);
    let f = frame(&mut w, &b);
    w.backend.fail_input = true;
    let request = Request::Act {
        frame: f,
        sequence: 1,
        actions: actions(),
    };
    assert_eq!(w.handle(request.clone()).0, Err(ErrorCode::OutcomeUnknown));
    assert_eq!(w.handle(request).0, Err(ErrorCode::OutcomeUnknown));
    assert_eq!(
        w.handle(Request::Open {
            identity: w.identity.clone(),
            lease_ms: 1000
        })
        .0,
        Err(ErrorCode::OutcomeUnknown)
    );
    assert_eq!(c.lock().unwrap().input, 1);
}
#[test]
fn windows_frame_cannot_be_reused_after_success_even_with_new_sequence() {
    let (mut w, c) = fixture();
    let b = open(&mut w);
    let f = frame(&mut w, &b);
    assert!(
        w.handle(Request::Act {
            frame: f.clone(),
            sequence: 1,
            actions: actions()
        })
        .0
        .is_ok()
    );
    assert_eq!(
        w.handle(Request::Act {
            frame: f,
            sequence: 2,
            actions: actions()
        })
        .0,
        Err(ErrorCode::StaleFrame)
    );
    assert_eq!(c.lock().unwrap().input, 1);
}
#[test]
fn windows_timeout_cancel_disconnect_close_without_reopening() {
    for mode in 0..3 {
        let (mut w, c) = fixture();
        let b = open(&mut w);
        if mode == 0 {
            w.epoch.as_mut().unwrap().deadline = Instant::now();
            assert_eq!(
                w.handle(Request::Observe { binding: b }).0,
                Err(ErrorCode::SessionClosed)
            );
        } else {
            assert!(w.shutdown());
        }
        assert!(c.lock().unwrap().close > 0);
        assert!(
            w.handle(Request::Open {
                identity: w.identity.clone(),
                lease_ms: 1000
            })
            .0
            .is_err()
        );
        assert_eq!(c.lock().unwrap().input, 0);
    }
}
#[test]
fn windows_capture_hash_and_pixel_budget_are_enforced() {
    for mode in 0..2 {
        let (mut w, c) = fixture();
        let b = open(&mut w);
        if mode == 0 {
            w.backend.bad_pixels = true;
        } else {
            w.maximum = 15;
        }
        assert!(w.handle(Request::Observe { binding: b }).0.is_err());
        assert_eq!(c.lock().unwrap().input, 0);
    }
}
