//! Offline computer-use wire contract. No tool registration or OS backend.
//! Frame references carry metadata only; pixels need a bounded artifact transport.
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub mod ui;

pub const PROTOCOL: u32 = 1;
pub const MAX_ACTIONS: usize = 16;
pub const MAX_LEASE_MS: u32 = 30_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub protocol: u32,
    pub available: bool,
    pub backend: String,
    pub max_actions: usize,
    pub max_lease_ms: u32,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            protocol: PROTOCOL,
            available: false,
            backend: "unavailable".into(),
            max_actions: MAX_ACTIONS,
            max_lease_ms: MAX_LEASE_MS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub owner: String,
    pub task_id: Uuid,
    pub device_id: Uuid,
    /// Backend-owned desktop identity, never an RH polling session ID.
    pub desktop_id: String,
}
/// Revocable persistent capability; each execution supplies a separate task identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityIdentity {
    pub owner: String,
    pub device_id: Uuid,
    pub desktop_id: String,
}
impl CapabilityIdentity {
    pub fn allows(&self, identity: &Identity) -> bool {
        self.owner == identity.owner
            && self.device_id == identity.device_id
            && self.desktop_id == identity.desktop_id
    }
    pub fn from_execution(identity: &Identity) -> Self {
        Self {
            owner: identity.owner.clone(),
            device_id: identity.device_id,
            desktop_id: identity.desktop_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub identity: Identity,
    pub epoch: Uuid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotation {
    R0,
    R90,
    R180,
    R270,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentRect {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameMapping {
    pub frame_size: [u32; 2],
    /// Pixel centers run from (left, top) through (left + width - 1, top + height - 1).
    pub content: ContentRect,
    pub logical_size: [u32; 2],
    /// Backend layout origin: global for native desktop windows, [0,0] for a
    /// Weavelo source-local canvas. Never interpreted as a remote monitor origin.
    pub desktop_position: [i32; 2],
    pub rotation: Rotation,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Source-local logical pixel-grid coordinates in [0, logical_size - 1].
pub struct SourcePoint {
    pub x: f64,
    pub y: f64,
}
impl FrameMapping {
    pub fn map(&self, x: f64, y: f64) -> Result<SourcePoint, ErrorCode> {
        let c = &self.content;
        if self.frame_size.contains(&0)
            || self.logical_size.contains(&0)
            || c.width == 0
            || c.height == 0
            || u64::from(c.left) + u64::from(c.width) > u64::from(self.frame_size[0])
            || u64::from(c.top) + u64::from(c.height) > u64::from(self.frame_size[1])
            || !x.is_finite()
            || !y.is_finite()
            || x < f64::from(c.left)
            || y < f64::from(c.top)
            || x > f64::from(c.left) + f64::from(c.width - 1)
            || y > f64::from(c.top) + f64::from(c.height - 1)
        {
            return Err(ErrorCode::InvalidCoordinates);
        }
        let u = if c.width == 1 {
            0.5
        } else {
            (x - f64::from(c.left)) / f64::from(c.width - 1)
        };
        let v = if c.height == 1 {
            0.5
        } else {
            (y - f64::from(c.top)) / f64::from(c.height - 1)
        };
        let (u, v) = match self.rotation {
            Rotation::R0 => (u, v),
            Rotation::R90 => (v, 1.0 - u),
            Rotation::R180 => (1.0 - u, 1.0 - v),
            Rotation::R270 => (1.0 - v, u),
        };
        // Pixel endpoints stay inside the source for rotated edge coordinates.
        Ok(SourcePoint {
            x: u * f64::from(self.logical_size[0] - 1),
            y: v * f64::from(self.logical_size[1] - 1),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameRef {
    pub binding: Binding,
    pub source_revision: u64,
    pub frame_sequence: u64,
    pub mapping: FrameMapping,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Middle,
    Right,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Native Unicode keyboard event, independent of the active keyboard layout.
    Unicode {
        text: String,
        key_down: bool,
    },
    PointerMove {
        x: f64,
        y: f64,
    },
    /// USB HID keyboard-page usage; a native adapter must map it explicitly.
    KeyDown {
        usage: u16,
    },
    KeyUp {
        usage: u16,
    },
    ButtonDown {
        button: Button,
    },
    ButtonUp {
        button: Button,
    },
    Wheel {
        horizontal: i16,
        vertical: i16,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Capabilities {},
    Open {
        identity: Identity,
        lease_ms: u32,
    },
    Observe {
        binding: Binding,
    },
    Act {
        frame: FrameRef,
        sequence: u64,
        actions: Vec<Action>,
    },
    Close {
        binding: Binding,
    },
}
impl Request {
    pub fn identity(&self) -> Option<&Identity> {
        match self {
            Self::Capabilities {} => None,
            Self::Open { identity, .. } => Some(identity),
            Self::Observe { binding } | Self::Close { binding } => Some(&binding.identity),
            Self::Act { frame, .. } => Some(&frame.binding.identity),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unavailable,
    InvalidRequest,
    IdentityMismatch,
    DesktopBusy,
    SessionClosed,
    StaleFrame,
    InvalidSequence,
    InvalidCoordinates,
    InvalidActions,
    OutcomeUnknown,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Capabilities {
        capabilities: Capabilities,
    },
    Opened {
        binding: Binding,
        expires_at_ms: u64,
    },
    Observed {
        frame: FrameRef,
    },
    Accepted {
        sequence: u64,
        accepted_actions: usize,
        effect_confirmed: bool,
    },
    Closed {},
}

/// The only production endpoint in this phase. No environment-variable gate.
pub fn unavailable(request: &Request) -> Result<Response, ErrorCode> {
    if matches!(request, Request::Capabilities {}) {
        Ok(Response::Capabilities {
            capabilities: Capabilities::default(),
        })
    } else {
        Err(ErrorCode::Unavailable)
    }
}

/// Fixed private-pipe envelope. A request ID is the existing Agent operation UUID.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    pub request_id: Uuid,
    pub request: Request,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPixels {
    pub encoding: String,
    pub size: [u32; 2],
    /// Exact SPA range/matrix/transfer/primaries for YUV. Packed rgba8/rgba8_srgb
    /// uses [0;4] (not SPA YUV); its encoding explicitly declares color semantics.
    pub color: [u32; 4],
    pub plane_lengths: Vec<usize>,
    pub plane_sha256: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReply {
    pub request_id: Uuid,
    pub result: Result<Response, ErrorCode>,
    pub pixels: Option<RawPixels>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elements: Vec<ui::SemanticElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_state: Option<String>,
}
pub const MAX_WORKER_HEADER: usize = 64 * 1024;
pub const MAX_WORKER_PIXELS: usize = 16 * 1024 * 1024;

impl WorkerReply {
    /// Conservatively truncate `elements` so the serialized JSON header remains
    /// strictly smaller than `MAX_WORKER_HEADER`.
    pub fn bound_elements_for_header(&mut self) {
        fn json_str_bytes(s: &str) -> usize {
            2 + s
                .bytes()
                .map(|b| match b {
                    b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
                    0x00..=0x1f => 6,
                    _ => 1,
                })
                .sum::<usize>()
        }
        fn element_json_bytes(e: &ui::SemanticElement) -> usize {
            160 + json_str_bytes(&e.ref_id)
                + json_str_bytes(&e.role)
                + json_str_bytes(&e.name)
                + e.value.as_deref().map_or(0, json_str_bytes)
        }
        let takeover_bytes = self.takeover_state.as_deref().map_or(0, json_str_bytes) + 32;
        let budget = MAX_WORKER_HEADER.saturating_sub(4096 + takeover_bytes);
        let mut used = 2usize;
        let mut keep = 0usize;
        for el in &self.elements {
            let need = element_json_bytes(el) + usize::from(keep > 0);
            if used + need > budget {
                break;
            }
            used += need;
            keep += 1;
        }
        self.elements.truncate(keep);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameArtifact {
    pub operation_id: Uuid,
    pub binding: Binding,
    pub capability_version: u64,
    pub expires_at_ms: u64,
    pub bytes: usize,
    pub sha256: String,
    pub pixels: RawPixels,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elements: Vec<ui::SemanticElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_state: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn worker_reply_and_artifact_remain_backward_compatible_and_header_bounded() {
        let id = Uuid::new_v4();
        let identity = Identity {
            owner: "test".into(),
            task_id: id,
            device_id: id,
            desktop_id: "desktop".into(),
        };
        let binding = Binding {
            identity,
            epoch: id,
        };
        let old_reply = json!({
            "request_id": id,
            "result": { "Ok": { "state": "closed" } },
            "pixels": null
        });
        let decoded_reply: WorkerReply = serde_json::from_value(old_reply).unwrap();
        assert!(decoded_reply.elements.is_empty());
        assert!(decoded_reply.takeover_state.is_none());

        let old_artifact = json!({
            "operation_id": id,
            "binding": binding,
            "capability_version": 1,
            "expires_at_ms": 1234,
            "bytes": 4,
            "sha256": "0".repeat(64),
            "pixels": {
                "encoding": "rgba8_srgb",
                "size": [1, 1],
                "color": [0, 0, 0, 0],
                "plane_lengths": [4],
                "plane_sha256": []
            }
        });
        let decoded_artifact: FrameArtifact = serde_json::from_value(old_artifact).unwrap();
        assert!(decoded_artifact.elements.is_empty());
        assert!(decoded_artifact.takeover_state.is_none());

        let frame = FrameRef {
            binding,
            source_revision: 1,
            frame_sequence: 2,
            mapping: FrameMapping {
                frame_size: [1920, 1080],
                content: ContentRect {
                    left: 0,
                    top: 0,
                    width: 1920,
                    height: 1080,
                },
                logical_size: [1920, 1080],
                desktop_position: [0, 0],
                rotation: Rotation::R0,
            },
        };
        let mut huge_reply = WorkerReply {
            request_id: id,
            result: Ok(Response::Observed { frame }),
            pixels: Some(RawPixels {
                encoding: "rgba8_srgb".into(),
                size: [1920, 1080],
                color: [0; 4],
                plane_lengths: vec![1920 * 1080 * 4],
                plane_sha256: vec!["a".repeat(64)],
            }),
            elements: (0..400)
                .map(|i| ui::SemanticElement {
                    ref_id: format!("n{i}"),
                    role: "AXButton".into(),
                    name: format!("Control {i} {}", "x".repeat(120)),
                    value: Some("v".repeat(80)),
                    bounds: [10, 20, 100, 50],
                    interactive: true,
                    enabled: true,
                    focused: false,
                    checked: Some(false),
                    selected: Some(false),
                })
                .collect(),
            takeover_state: Some("paused_by_human".into()),
        };
        huge_reply.bound_elements_for_header();
        let encoded = serde_json::to_vec(&huge_reply).unwrap();
        assert!(
            encoded.len() < MAX_WORKER_HEADER,
            "header len {} >= {}",
            encoded.len(),
            MAX_WORKER_HEADER
        );
        assert!(!huge_reply.elements.is_empty());
    }
}
