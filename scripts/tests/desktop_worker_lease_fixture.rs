//! Actual Linux Worker orchestration against a recording Portal substitute.
//! No Portal, desktop, service, network or administrator access.
#![allow(dead_code)]
extern crate self as host;
extern crate self as protocol;
use remote_hosts_desktop_protocol::*;
use std::{
    sync::{Mutex, OnceLock},
    time::Duration,
};
use uuid::Uuid;
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    KeyDown(u32),
    KeyUp(u32),
    MouseDown(u8),
    MouseUp(u8),
    MouseMoveAbsolute { x: u16, y: u16 },
    MouseScroll { delta_x: i32, delta_y: i32 },
}
fn log() -> &'static Mutex<Vec<InputEvent>> {
    static LOG: OnceLock<Mutex<Vec<InputEvent>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(vec![]))
}
pub mod linux_frame {
    #[derive(Clone, Copy, PartialEq)]
    pub enum PixelFormat {
        Nv12,
        I420,
    }
    #[derive(Clone, Copy, PartialEq)]
    pub enum Transform {
        Identity,
    }
    #[derive(Clone, Copy)]
    pub struct Crop {
        pub x: u32,
        pub y: u32,
        pub width: u32,
        pub height: u32,
    }
    pub struct Color {
        pub range: u32,
        pub matrix: u32,
        pub transfer: u32,
        pub primaries: u32,
    }
    pub struct SourceFormat {
        pub crop: Option<Crop>,
        pub width: u32,
        pub height: u32,
        pub transform: Transform,
        pub pixel_format: PixelFormat,
        pub color: Color,
    }
    pub struct Stamp {
        pub format_revision: u64,
    }
    pub struct OwnedFrame {
        pub source_format: SourceFormat,
        pub stamp: Stamp,
        pub width: u32,
        pub height: u32,
        pub planes: Vec<Vec<u8>>,
    }
}
#[derive(PartialEq)]
pub enum PortalSourceKind {
    Monitor,
}
pub struct Selected {
    pub kind: PortalSourceKind,
    pub logical_size: Option<(i32, i32)>,
    pub logical_position: Option<(i32, i32)>,
}
pub struct Lease;
impl Lease {
    pub fn selected_source(&self) -> Selected {
        Selected {
            kind: PortalSourceKind::Monitor,
            logical_size: Some((100, 50)),
            logical_position: Some((1000, 200)),
        }
    }
    pub async fn close(&mut self) -> bool {
        true
    }
}
pub async fn request_registered_portal_control_capture(
    _: &str,
    _: &str,
    _: tokio::sync::watch::Receiver<bool>,
) -> Result<Lease, ()> {
    Ok(Lease)
}
static FRAME_VALID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
pub struct PreparedPortalCapture {
    keys: Mutex<std::collections::BTreeSet<u32>>,
    buttons: Mutex<std::collections::BTreeSet<u8>>,
}
pub async fn prepare_pipewire_capture_bounded(
    _: Lease,
    _: usize,
    _: usize,
) -> Result<PreparedPortalCapture, ()> {
    Ok(PreparedPortalCapture {
        keys: Mutex::new(Default::default()),
        buttons: Mutex::new(Default::default()),
    })
}
impl PreparedPortalCapture {
    pub fn input_available(&self) -> bool {
        true
    }
    pub fn is_revoked(&self) -> bool {
        false
    }
    pub fn revoke(&self) {}
    pub async fn close_owned(&self) -> Result<(), ()> {
        for key in std::mem::take(&mut *self.keys.lock().unwrap()) {
            log().lock().unwrap().push(InputEvent::KeyUp(key));
        }
        for button in std::mem::take(&mut *self.buttons.lock().unwrap()) {
            log().lock().unwrap().push(InputEvent::MouseUp(button));
        }
        Ok(())
    }
    pub async fn observe_owned_frame(
        &self,
        _: Duration,
        _: usize,
    ) -> Result<linux_frame::OwnedFrame, ()> {
        use linux_frame::*;
        Ok(OwnedFrame {
            source_format: SourceFormat {
                crop: None,
                width: 200,
                height: 100,
                transform: Transform::Identity,
                pixel_format: PixelFormat::I420,
                color: Color {
                    range: 0,
                    matrix: 0,
                    transfer: 0,
                    primaries: 0,
                },
            },
            stamp: Stamp { format_revision: 1 },
            width: 200,
            height: 100,
            planes: vec![vec![0]; 3],
        })
    }
    pub fn check_input_frame(&self, _: u64, _: [u32; 2]) -> Result<(), ()> {
        if FRAME_VALID.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err(())
        }
    }
    pub async fn inject_input(&self, event: InputEvent) -> Result<(), ()> {
        match &event {
            InputEvent::KeyDown(key) => {
                self.keys.lock().unwrap().insert(*key);
            }
            InputEvent::KeyUp(key) => {
                self.keys.lock().unwrap().remove(key);
            }
            InputEvent::MouseDown(button) => {
                self.buttons.lock().unwrap().insert(*button);
            }
            InputEvent::MouseUp(button) => {
                self.buttons.lock().unwrap().remove(button);
            }
            _ => {}
        }
        log().lock().unwrap().push(event);
        Ok(())
    }
}
mod config {
    use super::*;
    pub const APP_ID: &str = "recording-only";
    #[derive(Clone)]
    pub struct Approval {
        pub enabled: bool,
        pub identity: Identity,
        pub expires_at_ms: u64,
        pub logical_size: [u32; 2],
        pub logical_position: [i32; 2],
        pub max_frame_bytes: usize,
    }
    impl Approval {
        pub fn verify_desktop(&self) -> Result<(), ()> {
            Ok(())
        }
    }
    pub fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
}
mod lifecycle {
    use super::*;
    pub fn phase(_: Option<Uuid>, _: &str) {}
    pub async fn shutdown<T>(_: Option<Uuid>, future: impl std::future::Future<Output = T>) -> T {
        future.await
    }
}
#[path = "../../desktop-worker/src/native.rs"]
mod native;

#[cfg(test)]
mod recording_tests {
    use super::*;
    async fn opened() -> (native::Worker, Binding, FrameRef) {
        log().lock().unwrap().clear();
        FRAME_VALID.store(true, std::sync::atomic::Ordering::SeqCst);
        let id = Identity {
            owner: "fixture-owner".into(),
            task_id: Uuid::new_v4(),
            device_id: Uuid::new_v4(),
            desktop_id: "recording".into(),
        };
        let mut worker = native::Worker::new(Some(config::Approval {
            enabled: true,
            identity: id.clone(),
            expires_at_ms: config::now_ms() + 30_000,
            logical_size: [100, 50],
            logical_position: [1000, 200],
            max_frame_bytes: 100_000,
        }));
        let r = worker
            .handle(Request::Open {
                identity: id,
                lease_ms: 30_000,
            })
            .await
            .unwrap();
        let Response::Opened { binding, .. } = r.response else {
            panic!("open")
        };
        let r = worker
            .handle(Request::Observe {
                binding: binding.clone(),
            })
            .await
            .unwrap();
        let Response::Observed { frame } = r.response else {
            panic!("observe")
        };
        (worker, binding, frame)
    }
    #[tokio::test]
    async fn new_down_state_does_not_post_another_down_and_old_bindings_do_not_renew() {
        let (mut w, _, frame) = opened().await;
        for sequence in [1, 2] {
            w.handle(Request::Act {
                frame: frame.clone(),
                sequence,
                actions: vec![Action::KeyDown { usage: 4 }],
            })
            .await
            .unwrap();
        }
        assert_eq!(*log().lock().unwrap(), vec![InputEvent::KeyDown(0)]);
        let before = w.wait_budget();
        let mut old_epoch = frame.clone();
        old_epoch.binding.epoch = Uuid::new_v4();
        assert!(
            w.handle(Request::Act {
                frame: old_epoch,
                sequence: 3,
                actions: vec![Action::KeyDown { usage: 4 }]
            })
            .await
            .is_err()
        );
        let mut old_owner = frame.clone();
        old_owner.binding.identity.owner = "another".into();
        assert!(
            w.handle(Request::Act {
                frame: old_owner,
                sequence: 3,
                actions: vec![Action::KeyDown { usage: 4 }]
            })
            .await
            .is_err()
        );
        assert!(
            w.handle(Request::Act {
                frame,
                sequence: 2,
                actions: vec![Action::KeyDown { usage: 4 }]
            })
            .await
            .is_err()
        );
        assert!(w.wait_budget() <= before);
        assert_eq!(log().lock().unwrap().len(), 1);
        w.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn missing_release_expires_and_session_cannot_reopen() {
        let (mut w, binding, frame) = opened().await;
        w.handle(Request::Act {
            frame: frame.clone(),
            sequence: 1,
            actions: vec![
                Action::KeyDown { usage: 4 },
                Action::ButtonDown {
                    button: Button::Left,
                },
            ],
        })
        .await
        .unwrap();
        assert!(w.wait_budget() <= Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(1050)).await;
        assert!(w.wait_budget().is_zero());
        assert!(
            w.handle(Request::Observe {
                binding: binding.clone()
            })
            .await
            .is_err()
        );
        assert!(
            w.handle(Request::Open {
                identity: binding.identity,
                lease_ms: 1000
            })
            .await
            .is_err()
        );
        assert!(
            w.handle(Request::Act {
                frame,
                sequence: 2,
                actions: vec![Action::KeyDown { usage: 4 }]
            })
            .await
            .is_err()
        );
        assert_eq!(
            *log().lock().unwrap(),
            vec![
                InputEvent::KeyDown(0),
                InputEvent::MouseDown(0),
                InputEvent::KeyUp(0),
                InputEvent::MouseUp(0)
            ]
        );
    }
    #[tokio::test]
    async fn observe_and_wheel_are_not_hold_state_renewals() {
        let (mut w, binding, frame) = opened().await;
        w.handle(Request::Act {
            frame,
            sequence: 1,
            actions: vec![Action::KeyDown { usage: 4 }],
        })
        .await
        .unwrap();
        let before = w.wait_budget();
        let r = w.handle(Request::Observe { binding }).await.unwrap();
        let Response::Observed { frame } = r.response else {
            panic!("observe")
        };
        w.handle(Request::Act {
            frame,
            sequence: 2,
            actions: vec![Action::Wheel {
                horizontal: 0,
                vertical: 1,
            }],
        })
        .await
        .unwrap();
        assert!(w.wait_budget() <= before);
        assert_eq!(
            *log().lock().unwrap(),
            vec![
                InputEvent::KeyDown(0),
                InputEvent::MouseScroll {
                    delta_x: 0,
                    delta_y: 1
                }
            ]
        );
        w.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn fresh_drag_does_not_extend_a_different_held_key() {
        let (mut w, binding, frame) = opened().await;
        w.handle(Request::Act {
            frame,
            sequence: 1,
            actions: vec![
                Action::KeyDown { usage: 4 },
                Action::ButtonDown {
                    button: Button::Left,
                },
            ],
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(750)).await;
        let reply = w
            .handle(Request::Observe {
                binding: binding.clone(),
            })
            .await
            .unwrap();
        let Response::Observed { frame } = reply.response else {
            panic!("observe")
        };
        w.handle(Request::Act {
            frame,
            sequence: 2,
            actions: vec![Action::PointerMove { x: 10., y: 10. }],
        })
        .await
        .unwrap();
        assert!(w.wait_budget() <= Duration::from_millis(250));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(w.handle(Request::Observe { binding }).await.is_err());
        let events = log().lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, InputEvent::KeyUp(0)))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, InputEvent::MouseUp(0)))
                .count(),
            1
        );
        assert!(!events.iter().any(|e| matches!(e, InputEvent::KeyUp(99))));
    }
    #[tokio::test]
    async fn invalid_capture_cannot_renew_state_only_down() {
        let (mut w, _, frame) = opened().await;
        w.handle(Request::Act {
            frame: frame.clone(),
            sequence: 1,
            actions: vec![Action::KeyDown { usage: 4 }],
        })
        .await
        .unwrap();
        FRAME_VALID.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(
            w.handle(Request::Act {
                frame,
                sequence: 2,
                actions: vec![Action::KeyDown { usage: 4 }]
            })
            .await
            .is_err()
        );
        assert_eq!(
            *log().lock().unwrap(),
            vec![InputEvent::KeyDown(0), InputEvent::KeyUp(0)]
        );
    }
}
