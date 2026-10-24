//! Fixed private worker bridge to a selected, already-authorized Weavelo source.
//! Reuses desktop-launch authority, Agent journals and bounded worker frame bytes.
//! Bounded gestures use the original GUI receipt; HID is explicitly translated.
use anyhow::{Context, Result, ensure};
use octendra_desktop_protocol::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub use super::weavelo_route::*;
impl Route {
    pub fn desktop_id(&self) -> String {
        let source = match self.target.source {
            Source::MainDisplay => "main".into(),
            Source::Display(id) => format!("display:{id}"),
            Source::Window(id) => format!("window:{id}"),
        };
        format!(
            "weavelo:{}:{source}:{}",
            self.target.connection_id, self.target.peer_device_id
        )
    }
    pub fn validate(&self, owner: u32) -> Result<()> {
        ensure!(
            self.profile.is_absolute()
                && !self.generation.is_empty()
                && self.generation.len() <= 128
                && self
                    .generation
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && self.gui_executable_sha256.len() == 64
                && self
                    .gui_executable_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
                && self.target.connection_id > 0
                && !self.target.peer_device_id.is_empty()
                && self.target.peer_device_id.len() <= 128
                && self
                    .target
                    .peer_device_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
            "invalid Weavelo route"
        );
        private_directory(&self.profile, owner)
    }
}
fn private_directory(path: &Path, owner: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = path.symlink_metadata()?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink() && m.uid() == owner && m.mode() & 0o777 == 0o700,
        "private GUI directory required"
    );
    Ok(())
}
fn uid() -> u32 {
    unsafe { nix::libc::geteuid() }
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
async fn socket(route: &Route) -> Result<tokio::net::UnixStream> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    route.validate(uid())?;
    let mut selected = None;
    for directory in [route.profile.join("gui-state"), route.profile.clone()] {
        let path = directory.join("gui-receive.sock");
        if !path.try_exists()? {
            continue;
        }
        private_directory(&directory, uid())?;
        let m = path.symlink_metadata()?;
        ensure!(
            m.file_type().is_socket() && m.uid() == uid() && m.mode() & 0o777 == 0o600,
            "private GUI socket required"
        );
        ensure!(selected.is_none(), "ambiguous GUI adapter");
        selected = Some(path);
    }
    let s = tokio::net::UnixStream::connect(selected.context("GUI adapter unavailable")?).await?;
    ensure!(s.peer_cred()?.uid() == uid(), "GUI peer user mismatch");
    Ok(s)
}
async fn exchange(route: &Route, request: Value, maximum: usize) -> Result<(Value, Vec<u8>)> {
    tokio::time::timeout(Duration::from_secs(32), async {
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= 4096,
            "GUI request bound"
        );
        let mut s = socket(route).await?;
        s.write_u32(bytes.len() as u32).await?;
        s.write_all(&bytes).await?;
        let len = s.read_u32().await? as usize;
        ensure!(len > 0 && len <= MAX_WORKER_HEADER, "GUI response bound");
        let mut body = vec![0; len];
        s.read_exact(&mut body).await?;
        let body: Value = serde_json::from_slice(&body)?;
        ensure!(body["ok"] == true, "GUI request unavailable");
        let count = body
            .get("bytes")
            .map(|b| b.as_u64().context("pixel length"))
            .transpose()?
            .unwrap_or(0);
        ensure!(
            count <= maximum as u64 && count <= MAX_WORKER_PIXELS as u64,
            "GUI pixel budget"
        );
        let mut pixels = vec![0; count as usize];
        s.read_exact(&mut pixels).await?;
        Ok((body, pixels))
    })
    .await
    .context("GUI operation timed out")?
}
pub(super) async fn probe(route: &Route, input: bool) -> Result<()> {
    let (status, _) = exchange(route, json!("Status"), 0).await?;
    ensure!(
        status["generation"] == route.generation
            && status["executable_sha256"] == route.gui_executable_sha256
            && status["computer_use"]["protocol"] == 1
            && status["computer_use"]["observe"] == true
            && (!input || status["computer_use"]["input"] == true),
        "GUI generation/identity/observe contract changed"
    );
    Ok(())
}
pub(super) async fn validate_lease(route: &Route, binding: &Binding) -> Result<()> {
    probe(route, false).await?;
    exchange(route,envelope(route,uuid::Uuid::new_v4(),json!({"operation":"validate","actor":actor(&binding.identity),"lease_id":binding.epoch})),0).await?;
    Ok(())
}
fn envelope(route: &Route, id: uuid::Uuid, call: Value) -> Value {
    json!({"ComputerUse":{"generation":route.generation,"request_id":id.to_string(),"deadline_ms":now()+4500,"call":call}})
}
fn actor(id: &Identity) -> Value {
    json!({"owner":id.owner,"task_id":id.task_id,"executor_device_id":id.device_id})
}

#[cfg(target_os = "macos")]
fn load(path: &Path) -> Result<super::transport::WorkerApproval> {
    use std::os::unix::fs::MetadataExt;
    let owner = uid();
    ensure!(
        owner > 0 && owner == unsafe { nix::libc::getuid() },
        "ordinary user worker required"
    );
    let bytes = super::transport::owned_file(path, 0o600, 16384, owner)?;
    let a: super::transport::WorkerApproval = serde_json::from_slice(&bytes)?;
    let route = a
        .weavelo
        .as_ref()
        .context("explicit Weavelo route required")?;
    route.validate(owner)?;
    ensure!(
        a.enabled
            && (!a.allow_input || route.input_platform == Some(InputPlatform::Macos))
            && a.app_id == "fun.hackerlife.octendra.desktop-worker"
            && a.capability_version.is_some_and(|v| v > 0)
            && a.desktop_uid == owner
            && a.desktop_session == "weavelo"
            && a.logical_position == [0, 0]
            && !a.identity.owner.is_empty()
            && !a.identity.task_id.is_nil()
            && !a.identity.device_id.is_nil()
            && a.identity.desktop_id == route.desktop_id()
            && a.expires_at_ms > now()
            && a.expires_at_ms <= now() + 25_000 + u64::from(MAX_LEASE_MS)
            && a.logical_size.iter().all(|n| *n > 0 && *n <= 8192)
            && a.max_frame_bytes > 0
            && a.max_frame_bytes <= MAX_WORKER_PIXELS,
        "Weavelo approval bounds"
    );
    let exe = std::env::current_exe()?;
    let m = exe.symlink_metadata()?;
    ensure!(
        hash(&super::transport::owned_file(
            &exe,
            m.mode() & 0o777,
            256 * 1024 * 1024,
            owner
        )?) == a.worker_sha256,
        "worker changed"
    );
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, path: *mut std::ffi::c_void, size: u32) -> i32;
    }
    let mut parent = [0u8; 4096];
    ensure!(
        unsafe {
            proc_pidpath(
                nix::libc::getppid(),
                parent.as_mut_ptr().cast(),
                parent.len() as u32,
            )
        } > 0,
        "Agent parent unavailable"
    );
    let end = parent
        .iter()
        .position(|b| *b == 0)
        .context("Agent executable path")?;
    let path = Path::new(std::str::from_utf8(&parent[..end])?);
    let m = path.symlink_metadata()?;
    ensure!(
        m.mode() & 0o022 == 0
            && hash(&super::transport::owned_file(
                path,
                m.mode() & 0o777,
                256 * 1024 * 1024,
                owner
            )?) == a.agent_sha256,
        "approved Agent parent required"
    );
    Ok(a)
}
struct Session {
    binding: Binding,
    expires: Instant,
    expires_at_ms: u64,
    last_sequence: u64,
    last_frame: Option<FrameRef>,
    consumed: bool,
}
#[cfg(target_os = "macos")]
pub async fn run(path: &Path) -> Result<()> {
    let a = load(path)?;
    let route = a.weavelo.as_ref().context("Weavelo route")?;
    probe(route, a.allow_input).await?;
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let mut session: Option<Session> = None;
    loop {
        let len = match input.read_u32().await {
            Ok(n) => n as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        ensure!(len > 0 && len <= MAX_WORKER_HEADER, "worker frame bound");
        let mut bytes = vec![0; len];
        input.read_exact(&mut bytes).await?;
        let request: WorkerRequest = serde_json::from_slice(&bytes)?;
        let close = matches!(&request.request, Request::Close { .. });
        let is_act = matches!(&request.request, Request::Act { .. });
        let response = handle(
            &a,
            route,
            &mut session,
            request.request_id,
            &request.request,
        )
        .await;
        let (result, pixels, raw) = match response {
            Ok((r, p, b)) => (Ok(r), p, b),
            // Once an input RPC may have been sent, failures are never reported
            // as proof of no effect. Agent lifecycle/journal forbids automatic replay.
            Err(_) => (
                Err(if is_act {
                    ErrorCode::OutcomeUnknown
                } else {
                    ErrorCode::Unavailable
                }),
                None,
                Vec::new(),
            ),
        };
        let reply = WorkerReply {
            request_id: request.request_id,
            result,
            pixels,
        };
        let header = serde_json::to_vec(&reply)?;
        ensure!(header.len() <= MAX_WORKER_HEADER, "worker reply bound");
        output.write_u32(header.len() as u32).await?;
        output.write_all(&header).await?;
        output.write_all(&raw).await?;
        output.flush().await?;
        if close {
            break;
        }
    }
    Ok(())
}
#[cfg(target_os = "macos")]
async fn handle(
    a: &super::transport::WorkerApproval,
    route: &Route,
    session: &mut Option<Session>,
    operation: uuid::Uuid,
    request: &Request,
) -> Result<(Response, Option<RawPixels>, Vec<u8>)> {
    let closing = matches!(request, Request::Close { .. });
    ensure!(
        !operation.is_nil()
            && a.identity.desktop_id == route.desktop_id()
            && (closing || now() < a.expires_at_ms)
            && request.identity() == Some(&a.identity),
        "worker identity/expiry"
    );
    let actor = actor(&a.identity);
    match request {
        Request::Open { lease_ms, .. } => {
            ensure!(
                session.is_none() && *lease_ms > 0 && *lease_ms <= MAX_LEASE_MS,
                "observer busy/lease"
            );
            let binding = Binding {
                identity: a.identity.clone(),
                epoch: operation,
            };
            let started = Instant::now();
            let expires_at_ms = (now() + u64::from(*lease_ms)).min(a.expires_at_ms);
            let (body, _) = exchange(
                route,
                envelope(
                    route,
                    operation,
                    json!({"operation":"open","actor":actor,"target":route.target,
                "lease_id":binding.epoch,"lease_ms":lease_ms,"max_bytes":a.max_frame_bytes,"control":a.allow_input}),
                ),
                0,
            )
            .await?;
            ensure!(
                body["lease_id"] == binding.epoch.to_string() && body["lease_ms"] == *lease_ms,
                "observer admission conflict"
            );
            ensure!(now() < expires_at_ms, "open lease expired during admission");
            *session = Some(Session {
                binding: binding.clone(),
                expires: started + Duration::from_millis(u64::from(*lease_ms)),
                expires_at_ms,
                last_sequence: 0,
                last_frame: None,
                consumed: false,
            });
            Ok((
                Response::Opened {
                    binding,
                    expires_at_ms,
                },
                None,
                vec![],
            ))
        }
        Request::Observe { binding } | Request::Close { binding } => {
            let current = session.as_mut().context("observer closed")?;
            ensure!(
                current.binding == *binding
                    && (closing
                        || (Instant::now() < current.expires && now() < current.expires_at_ms)),
                "observer binding/expiry"
            );
            let close = matches!(request, Request::Close { .. });
            ensure!(
                close || !current.consumed,
                "gesture receipt consumed; close and reopen"
            );
            current.last_frame.take();
            let (body,bytes)=exchange(route,envelope(route,operation,json!({"operation":if close {"close"} else {"observe"},"actor":actor,"lease_id":binding.epoch})),a.max_frame_bytes).await?;
            if close {
                *session = None;
                return Ok((Response::Closed {}, None, vec![]));
            }
            ensure!(
                Instant::now() < current.expires && now() < current.expires_at_ms,
                "observer expired during readback"
            );
            let size: [u32; 2] = serde_json::from_value(body["size"].clone())?;
            ensure!(
                size == a.logical_size && body["lease_id"] == binding.epoch.to_string(),
                "source canvas changed"
            );
            let revision = body["source_revision"]
                .as_u64()
                .context("source revision")?;
            let sequence = body["frame_sequence"].as_u64().context("frame sequence")?;
            ensure!(
                revision > 0 && sequence > current.last_sequence,
                "source frame sequence"
            );
            current.last_sequence = sequence;
            let encoding = body["encoding"].as_str().context("RGBA encoding")?;
            rgba_layout(size, encoding, bytes.len(), a.max_frame_bytes)?;
            let pixels = RawPixels {
                encoding: encoding.into(),
                size,
                color: [0; 4],
                plane_lengths: vec![bytes.len()],
                plane_sha256: vec![hash(&bytes)],
            };
            let frame = FrameRef {
                binding: binding.clone(),
                source_revision: revision,
                frame_sequence: sequence,
                mapping: FrameMapping {
                    frame_size: size,
                    content: ContentRect {
                        left: 0,
                        top: 0,
                        width: size[0],
                        height: size[1],
                    },
                    logical_size: size,
                    desktop_position: [0, 0],
                    rotation: Rotation::R0,
                },
            };
            current.last_frame = Some(frame.clone());
            Ok((Response::Observed { frame }, Some(pixels), bytes))
        }
        Request::Act {
            frame,
            sequence,
            actions,
        } => {
            ensure!(
                a.allow_input && route.input_platform == Some(InputPlatform::Macos),
                "remote input not approved"
            );
            let current = session.as_mut().context("observer closed")?;
            ensure!(
                !current.consumed
                    && current.binding == frame.binding
                    && current.last_frame.as_ref() == Some(frame)
                    && *sequence == 1
                    && Instant::now() < current.expires
                    && now() < current.expires_at_ms,
                "gesture frame/binding/sequence/expiry"
            );
            let action = atomic_input(frame, actions)?;
            current.consumed = true;
            current.last_frame.take();
            let (body, _) = exchange(route, envelope(route, operation, json!({
                "operation":"act", "actor":actor, "lease_id":frame.binding.epoch,
                "source_revision":frame.source_revision, "frame_sequence":frame.frame_sequence, "action":action,
            })), 0).await?;
            ensure!(
                body["effect_confirmed"] == false,
                "input acknowledgement mismatch"
            );
            Ok((
                Response::Accepted {
                    sequence: *sequence,
                    accepted_actions: actions.len(),
                    effect_confirmed: false,
                },
                None,
                vec![],
            ))
        }
        _ => anyhow::bail!("unsupported Weavelo request"),
    }
}
/// One complete gesture per observed receipt/lease. No held keys, dragging,
/// modifiers or arbitrary batch can cross the owner-local bounded action API.
#[cfg(target_os = "macos")]
fn atomic_input(frame: &FrameRef, actions: &[Action]) -> Result<Value> {
    let point = |x, y| -> Result<(u16, u16)> {
        let p = frame
            .mapping
            .map(x, y)
            .map_err(|_| anyhow::anyhow!("source coordinates"))?;
        let axis = |value: f64, size: u32| -> u16 {
            if size == 1 {
                32768
            } else {
                (value / f64::from(size - 1) * 65535.).round() as u16
            }
        };
        Ok((
            axis(p.x, frame.mapping.logical_size[0]),
            axis(p.y, frame.mapping.logical_size[1]),
        ))
    };
    match actions {
        [
            Action::PointerMove { x, y },
            Action::ButtonDown { button },
            Action::ButtonUp { button: up },
        ] if button == up => {
            let (x, y) = point(*x, *y)?;
            let button = match button {
                Button::Left => 0,
                Button::Right => 1,
                Button::Middle => 2,
            };
            Ok(json!({"kind":"click","x":x,"y":y,"button":button}))
        }
        [Action::KeyDown { usage }, Action::KeyUp { usage: up }] if usage == up && *usage < 224 => {
            let key = super::macos_worker::mac_key(*usage).context("unsupported HID key")?;
            Ok(json!({"kind":"key_press","key_code":key}))
        }
        [
            Action::PointerMove { x, y },
            Action::Wheel {
                horizontal,
                vertical,
            },
        ] if i32::from(*horizontal).unsigned_abs() <= 1200
            && i32::from(*vertical).unsigned_abs() <= 1200 =>
        {
            let (x, y) = point(*x, *y)?;
            Ok(json!({"kind":"scroll","x":x,"y":y,"delta_x":horizontal,"delta_y":vertical}))
        }
        _ => anyhow::bail!("one complete bounded gesture required"),
    }
}
fn rgba_layout(size: [u32; 2], encoding: &str, length: usize, maximum: usize) -> Result<()> {
    ensure!(
        size.iter().all(|n| *n > 0 && *n <= 8192) && matches!(encoding, "rgba8" | "rgba8_srgb"),
        "RGBA geometry/encoding"
    );
    let expected = (size[0] as usize)
        .checked_mul(size[1] as usize)
        .and_then(|n| n.checked_mul(4))
        .context("RGBA overflow")?;
    ensure!(
        expected == length && length <= maximum.min(MAX_WORKER_PIXELS),
        "packed RGBA layout/budget"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn weavelo_rgba_layout_rejects_unknown_encoding_padding_and_budget_overflow() {
        assert!(rgba_layout([3, 1], "rgba8_srgb", 12, 12).is_ok());
        assert!(rgba_layout([3, 1], "rgba8", 12, 12).is_ok());
        assert!(rgba_layout([3, 1], "bgra8", 12, 12).is_err());
        assert!(rgba_layout([3, 1], "rgba8_srgb", 13, 13).is_err());
        assert!(rgba_layout([3, 1], "rgba8_srgb", 12, 11).is_err());
        assert!(rgba_layout([0, 1], "rgba8_srgb", 0, 16).is_err());
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn weavelo_observe_worker_uses_real_private_socket_and_exact_remote_target() {
        socket_roundtrip(false).await;
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn weavelo_control_worker_forwards_original_frame_once_with_explicit_mac_keycode() {
        socket_roundtrip(true).await;
    }
    #[cfg(target_os = "macos")]
    async fn socket_roundtrip(control: bool) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = tokio::net::UnixListener::bind(dir.path().join("gui-receive.sock")).unwrap();
        std::fs::set_permissions(
            dir.path().join("gui-receive.sock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let route = Route {
            input_platform: control.then_some(InputPlatform::Macos),
            profile: dir.path().to_owned(),
            generation: "1-2".into(),
            gui_executable_sha256: "0".repeat(64),
            target: Target {
                peer_device_id: "synthetic-peer".into(),
                source: Source::Window(42),
                connection_id: 7,
            },
        };
        let identity = Identity {
            owner: "fixture".into(),
            task_id: uuid::Uuid::new_v4(),
            device_id: uuid::Uuid::new_v4(),
            desktop_id: route.desktop_id(),
        };
        let executor = identity.device_id;
        let server = tokio::spawn(async move {
            let operations = if control {
                vec!["open", "observe", "act", "close"]
            } else {
                vec!["open", "observe", "close"]
            };
            for operation in operations {
                let (mut stream, _) = listener.accept().await.unwrap();
                let size = stream.read_u32().await.unwrap() as usize;
                assert!(size <= 4096);
                let mut raw = vec![0; size];
                stream.read_exact(&mut raw).await.unwrap();
                let value: Value = serde_json::from_slice(&raw).unwrap();
                let call = &value["ComputerUse"]["call"];
                assert_eq!(value["ComputerUse"]["generation"], "1-2");
                assert_eq!(call["operation"], operation);
                assert_eq!(call["actor"]["executor_device_id"], executor.to_string());
                let (body, data) = match operation {
                    "open" => {
                        assert_eq!(call["control"], control);
                        assert_eq!(call["target"]["peer_device_id"], "synthetic-peer");
                        assert_eq!(call["target"]["source"], json!({"Window":42}));
                        (
                            json!({"ok":true,"lease_id":call["lease_id"],"lease_ms":call["lease_ms"]}),
                            vec![],
                        )
                    }
                    "observe" => (
                        json!({"ok":true,"lease_id":call["lease_id"],"source_revision":7,"frame_sequence":1,"size":[2,2],"bytes":16,"encoding":"rgba8_srgb"}),
                        vec![17; 16],
                    ),
                    "act" => {
                        assert_eq!(call["source_revision"], 7);
                        assert_eq!(call["frame_sequence"], 1);
                        assert_eq!(call["action"], json!({"kind":"key_press","key_code":0}));
                        (json!({"ok":true,"effect_confirmed":false}), vec![])
                    }
                    _ => (json!({"ok":true}), vec![]),
                };
                let bytes = serde_json::to_vec(&body).unwrap();
                stream.write_u32(bytes.len() as u32).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
                stream.write_all(&data).await.unwrap();
            }
        });
        // Synthetic permission data exercises handle/IPC only; production run()
        // independently checks the real approved parent and owned authority file.
        let approval = super::super::transport::WorkerApproval {
            weavelo: Some(route.clone()),
            capability_version: Some(7),
            enabled: true,
            app_id: "fun.hackerlife.octendra.desktop-worker".into(),
            desktop_uid: uid(),
            desktop_session: "weavelo".into(),
            identity: identity.clone(),
            worker_sha256: "0".repeat(64),
            agent_sha256: "0".repeat(64),
            expires_at_ms: now() + 30_000,
            logical_size: [2, 2],
            logical_position: [0, 0],
            max_frame_bytes: 16,
            allow_input: control,
        };
        let mut session = None;
        let (opened, _, _) = handle(
            &approval,
            &route,
            &mut session,
            uuid::Uuid::new_v4(),
            &Request::Open {
                identity,
                lease_ms: 5000,
            },
        )
        .await
        .unwrap();
        let Response::Opened { binding, .. } = opened else {
            panic!("not opened")
        };
        let (observed, pixels, bytes) = handle(
            &approval,
            &route,
            &mut session,
            uuid::Uuid::new_v4(),
            &Request::Observe {
                binding: binding.clone(),
            },
        )
        .await
        .unwrap();
        let Response::Observed { frame } = observed else {
            panic!("not observed")
        };
        assert_eq!(pixels.unwrap().encoding, "rgba8_srgb");
        assert_eq!(bytes, vec![17; 16]);
        let actions = vec![Action::KeyDown { usage: 4 }, Action::KeyUp { usage: 4 }];
        let request = Request::Act {
            frame: frame.clone(),
            sequence: 1,
            actions: actions.clone(),
        };
        // Invalid target/frame/sequence fail before an IPC effect and don't consume.
        let mut changed = frame.clone();
        changed.binding.identity.device_id = uuid::Uuid::new_v4();
        assert!(
            handle(
                &approval,
                &route,
                &mut session,
                uuid::Uuid::new_v4(),
                &Request::Act {
                    frame: changed,
                    sequence: 1,
                    actions: actions.clone()
                }
            )
            .await
            .is_err()
        );
        let mut changed = frame.clone();
        changed.source_revision += 1;
        assert!(
            handle(
                &approval,
                &route,
                &mut session,
                uuid::Uuid::new_v4(),
                &Request::Act {
                    frame: changed,
                    sequence: 1,
                    actions: actions.clone()
                }
            )
            .await
            .is_err()
        );
        assert!(
            handle(
                &approval,
                &route,
                &mut session,
                uuid::Uuid::new_v4(),
                &Request::Act {
                    frame,
                    sequence: 2,
                    actions
                }
            )
            .await
            .is_err()
        );
        let result = handle(
            &approval,
            &route,
            &mut session,
            uuid::Uuid::new_v4(),
            &request,
        )
        .await;
        if control {
            assert!(matches!(
                result.unwrap().0,
                Response::Accepted {
                    accepted_actions: 2,
                    effect_confirmed: false,
                    ..
                }
            ));
            assert!(
                handle(
                    &approval,
                    &route,
                    &mut session,
                    uuid::Uuid::new_v4(),
                    &request
                )
                .await
                .is_err()
            );
            assert!(
                handle(
                    &approval,
                    &route,
                    &mut session,
                    uuid::Uuid::new_v4(),
                    &Request::Observe {
                        binding: binding.clone()
                    }
                )
                .await
                .is_err()
            );
        } else {
            assert!(result.is_err());
        }
        handle(
            &approval,
            &route,
            &mut session,
            uuid::Uuid::new_v4(),
            &Request::Close { binding },
        )
        .await
        .unwrap();
        server.await.unwrap();
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn weavelo_atomic_gesture_maps_hid_and_source_edges_without_held_input() {
        let frame = FrameRef {
            binding: Binding {
                identity: Identity {
                    owner: "fixture".into(),
                    task_id: uuid::Uuid::new_v4(),
                    device_id: uuid::Uuid::new_v4(),
                    desktop_id: "synthetic".into(),
                },
                epoch: uuid::Uuid::new_v4(),
            },
            source_revision: 7,
            frame_sequence: 3,
            mapping: FrameMapping {
                frame_size: [101, 51],
                logical_size: [101, 51],
                desktop_position: [0, 0],
                rotation: Rotation::R0,
                content: ContentRect {
                    left: 0,
                    top: 0,
                    width: 101,
                    height: 51,
                },
            },
        };
        let click = |x, y| {
            vec![
                Action::PointerMove { x, y },
                Action::ButtonDown {
                    button: Button::Right,
                },
                Action::ButtonUp {
                    button: Button::Right,
                },
            ]
        };
        assert_eq!(
            atomic_input(&frame, &click(100., 50.)).unwrap(),
            json!({"kind":"click","x":65535,"y":65535,"button":1})
        );
        assert_eq!(atomic_input(&frame, &click(0., 0.)).unwrap()["x"], 0);
        for (x, y) in [
            (101., 0.),
            (-1., 0.),
            (0., 51.),
            (f64::NAN, 0.),
            (0., f64::INFINITY),
        ] {
            assert!(atomic_input(&frame, &click(x, y)).is_err());
        }
        assert_eq!(
            atomic_input(
                &frame,
                &[Action::KeyDown { usage: 40 }, Action::KeyUp { usage: 40 }]
            )
            .unwrap(),
            json!({"kind":"key_press","key_code":36})
        );
        for usage in [0, 50, 224, 225, 231, u16::MAX] {
            assert!(
                atomic_input(
                    &frame,
                    &[Action::KeyDown { usage }, Action::KeyUp { usage }]
                )
                .is_err()
            );
        }
        assert!(atomic_input(&frame, &[Action::KeyDown { usage: 4 }]).is_err());
        assert!(
            atomic_input(
                &frame,
                &[Action::KeyDown { usage: 4 }, Action::KeyUp { usage: 5 }]
            )
            .is_err()
        );
        assert!(
            atomic_input(
                &frame,
                &[
                    Action::PointerMove { x: 0., y: 0. },
                    Action::Wheel {
                        horizontal: 0,
                        vertical: 1201
                    }
                ]
            )
            .is_err()
        );
        assert_eq!(
            atomic_input(
                &frame,
                &[
                    Action::PointerMove { x: 50., y: 25. },
                    Action::Wheel {
                        horizontal: -1200,
                        vertical: 1200
                    }
                ]
            )
            .unwrap(),
            json!({"kind":"scroll","x":32768,"y":32768,"delta_x":-1200,"delta_y":1200})
        );
        let mut single = frame.clone();
        single.mapping.frame_size = [1, 1];
        single.mapping.logical_size = [1, 1];
        single.mapping.content.width = 1;
        single.mapping.content.height = 1;
        assert_eq!(atomic_input(&single, &click(0., 0.)).unwrap()["x"], 32768);
    }
    #[test]
    fn weavelo_input_platform_is_an_explicit_remote_grant_not_an_executor_inference() {
        let base = json!({"profile":"/synthetic/private","generation":"1-2","gui_executable_sha256":"c".repeat(64),
            "target":{"peer_device_id":"remote-peer","source":{"Window":42},"connection_id":7}});
        let route: Route = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(route.input_platform, None);
        let mut mac = base.clone();
        mac["input_platform"] = json!("macos");
        assert_eq!(
            serde_json::from_value::<Route>(mac).unwrap().input_platform,
            Some(InputPlatform::Macos)
        );
        for value in [json!("linux"), json!("executor"), json!(true)] {
            let mut invalid = base.clone();
            invalid["input_platform"] = value;
            assert!(serde_json::from_value::<Route>(invalid).is_err());
        }
    }
    #[test]
    fn remote_source_is_explicit_and_never_replaces_the_executor_device() {
        let route = Route {
            input_platform: None,
            profile: PathBuf::from("/synthetic/private/profile"),
            generation: "1-2".into(),
            gui_executable_sha256: "0".repeat(64),
            target: Target {
                peer_device_id: "remote-peer".into(),
                source: Source::Window(42),
                connection_id: 7,
            },
        };
        let id = Identity {
            owner: "fixture".into(),
            task_id: uuid::Uuid::new_v4(),
            device_id: uuid::Uuid::new_v4(),
            desktop_id: route.desktop_id(),
        };
        let call = actor(&id);
        assert_eq!(call["executor_device_id"], id.device_id.to_string());
        assert_eq!(route.desktop_id(), "weavelo:7:window:42:remote-peer");
        let packet = envelope(
            &route,
            uuid::Uuid::new_v4(),
            json!({"operation":"observe","actor":call,"lease_id":uuid::Uuid::new_v4()}),
        );
        assert_eq!(packet["ComputerUse"]["generation"], "1-2");
        assert!(
            serde_json::from_value::<Route>(
                json!({"profile":"/x","target":{"peer_device_id":"x"}})
            )
            .is_err()
        );
    }
}
