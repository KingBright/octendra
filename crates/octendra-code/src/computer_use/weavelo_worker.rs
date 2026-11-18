//! Fixed private worker bridge to a selected, already-authorized Weavelo source.
//! Reuses desktop-launch authority, Agent journals and bounded worker frame bytes.
//! Bounded gestures use the original GUI receipt; HID is explicitly translated.
use anyhow::{Context, Result, bail, ensure};
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
        if body["ok"] != true {
            if let Some(code) = body["error"]
                .as_str()
                .or_else(|| body["code"].as_str())
                .filter(|code| {
                    !code.is_empty()
                        && code.len() <= 96
                        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
            {
                bail!("{code}");
            }
            bail!("GUI request unavailable");
        }
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
    let current = refresh_route(route, input).await?;
    ensure!(
        current.generation == route.generation,
        "octendra_authority_gui_generation_changed"
    );
    Ok(())
}
pub(super) async fn refresh_route(route: &Route, input: bool) -> Result<Route> {
    let (status, _) = exchange(route, json!("Status"), 0).await?;
    ensure!(
        status["executable_sha256"] == route.gui_executable_sha256,
        "octendra_authority_gui_executable_changed"
    );
    ensure!(
        status["computer_use"]["protocol"] == 1,
        "octendra_authority_gui_protocol_changed"
    );
    ensure!(
        status["computer_use"]["observe"] == true,
        "octendra_authority_observe_unavailable"
    );
    ensure!(
        !input || status["computer_use"]["input"] == true,
        "octendra_authority_input_unavailable"
    );
    let mut current = route.clone();
    current.generation = status["generation"]
        .as_str()
        .context("octendra_authority_gui_generation_invalid")?
        .into();
    current.validate(uid())?;
    Ok(current)
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
            && (!a.allow_input || route.input_platform.is_some())
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
    paused_by_human: bool,
}
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct WorkerOutcome {
    result: std::result::Result<Response, ErrorCode>,
    pixels: Option<RawPixels>,
    raw: Vec<u8>,
    takeover_state: Option<String>,
}
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct PausedByHuman;
#[cfg(target_os = "macos")]
impl std::fmt::Display for PausedByHuman {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("paused_by_human")
    }
}
#[cfg(target_os = "macos")]
impl std::error::Error for PausedByHuman {}

#[cfg(target_os = "macos")]
fn value_is_paused_by_human(value: &Value) -> bool {
    value["computer_use_mode"].as_str() == Some("paused_by_human")
        || value["takeover_state"].as_str() == Some("paused_by_human")
        || value["mode"].as_str() == Some("paused_by_human")
}
#[cfg(target_os = "macos")]
fn session_list_paused(sessions: &Value, route: &Route) -> bool {
    let Some(items) = sessions.as_array() else {
        return false;
    };
    items.iter().any(|s| {
        s["computer_use_mode"].as_str() == Some("paused_by_human")
            && (s["connection_id"].as_u64() == Some(u64::from(route.target.connection_id))
                || s["device_id"].as_str() == Some(route.target.peer_device_id.as_str()))
    })
}
#[cfg(target_os = "macos")]
fn status_reports_paused_by_human(status: &Value, route: &Route) -> bool {
    if value_is_paused_by_human(status)
        || value_is_paused_by_human(&status["computer_use"])
        || value_is_paused_by_human(&status["sessions"])
        || session_list_paused(&status["sessions"]["sessions"], route)
        || session_list_paused(&status["sessions"], route)
    {
        return true;
    }
    if let Some(viewers) = status["viewers"].as_array() {
        for viewer in viewers {
            if value_is_paused_by_human(&viewer["sessions"])
                || session_list_paused(&viewer["sessions"]["sessions"], route)
            {
                return true;
            }
        }
    }
    false
}
#[cfg(target_os = "macos")]
fn is_paused_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<PausedByHuman>().is_some() || err.to_string() == "paused_by_human"
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
        let outcome = match handle_outcome(
            &a,
            route,
            &mut session,
            request.request_id,
            &request.request,
        )
        .await
        {
            Ok(outcome) => outcome,
            // Once an input RPC may have been sent, failures are never reported
            // as proof of no effect. Agent lifecycle/journal forbids automatic replay.
            Err(err) if is_paused_error(&err) => WorkerOutcome {
                result: Err(ErrorCode::Unavailable),
                pixels: None,
                raw: Vec::new(),
                takeover_state: Some("paused_by_human".into()),
            },
            Err(_) => WorkerOutcome {
                result: Err(if is_act {
                    ErrorCode::OutcomeUnknown
                } else {
                    ErrorCode::Unavailable
                }),
                pixels: None,
                raw: Vec::new(),
                takeover_state: None,
            },
        };
        let mut reply = WorkerReply {
            request_id: request.request_id,
            result: outcome.result,
            pixels: outcome.pixels,
            elements: Vec::new(),
            takeover_state: outcome.takeover_state,
        };
        reply.bound_elements_for_header();
        let header = serde_json::to_vec(&reply)?;
        ensure!(header.len() <= MAX_WORKER_HEADER, "worker reply bound");
        output.write_u32(header.len() as u32).await?;
        output.write_all(&header).await?;
        output.write_all(&outcome.raw).await?;
        output.flush().await?;
        if close {
            break;
        }
    }
    Ok(())
}
#[cfg(target_os = "macos")]
#[cfg_attr(not(test), allow(dead_code))]
async fn handle(
    a: &super::transport::WorkerApproval,
    route: &Route,
    session: &mut Option<Session>,
    operation: uuid::Uuid,
    request: &Request,
) -> Result<(Response, Option<RawPixels>, Vec<u8>)> {
    let outcome = handle_outcome(a, route, session, operation, request).await?;
    let response = outcome
        .result
        .map_err(|e| anyhow::anyhow!("worker error: {e:?}"))?;
    Ok((response, outcome.pixels, outcome.raw))
}
#[cfg(target_os = "macos")]
async fn handle_outcome(
    a: &super::transport::WorkerApproval,
    route: &Route,
    session: &mut Option<Session>,
    operation: uuid::Uuid,
    request: &Request,
) -> Result<WorkerOutcome> {
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
                paused_by_human: false,
            });
            Ok(WorkerOutcome {
                result: Ok(Response::Opened {
                    binding,
                    expires_at_ms,
                }),
                pixels: None,
                raw: vec![],
                takeover_state: None,
            })
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
                return Ok(WorkerOutcome {
                    result: Ok(Response::Closed {}),
                    pixels: None,
                    raw: vec![],
                    takeover_state: None,
                });
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
            let mut paused = value_is_paused_by_human(&body);
            if !paused {
                match exchange(route, json!("Status"), 0).await {
                    Ok((status, _)) => {
                        paused = status_reports_paused_by_human(&status, route);
                    }
                    Err(err) if err.to_string() == "paused_by_human" => paused = true,
                    Err(_) => {}
                }
            }
            current.paused_by_human = paused;
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
            Ok(WorkerOutcome {
                result: Ok(Response::Observed { frame }),
                pixels: Some(pixels),
                raw: bytes,
                takeover_state: paused.then(|| "paused_by_human".to_string()),
            })
        }
        Request::Act {
            frame,
            sequence,
            actions,
        } => {
            ensure!(
                a.allow_input && route.input_platform.is_some(),
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
            let call_payload = weavelo_call(frame, actions, actor, frame.binding.epoch)?;
            if current.paused_by_human {
                current.consumed = true;
                current.last_frame.take();
                return Err(PausedByHuman.into());
            }
            match exchange(route, json!("Status"), 0).await {
                Ok((status, _)) if status_reports_paused_by_human(&status, route) => {
                    current.paused_by_human = true;
                    current.consumed = true;
                    current.last_frame.take();
                    return Err(PausedByHuman.into());
                }
                Err(err) if err.to_string() == "paused_by_human" => {
                    current.paused_by_human = true;
                    current.consumed = true;
                    current.last_frame.take();
                    return Err(PausedByHuman.into());
                }
                _ => {}
            }
            current.consumed = true;
            current.last_frame.take();
            let (body, _) = exchange(route, envelope(route, operation, call_payload), 0).await?;
            ensure!(
                body["effect_confirmed"] == false,
                "input acknowledgement mismatch"
            );
            Ok(WorkerOutcome {
                result: Ok(Response::Accepted {
                    sequence: *sequence,
                    accepted_actions: actions.len(),
                    effect_confirmed: false,
                }),
                pixels: None,
                raw: vec![],
                takeover_state: None,
            })
        }
        _ => anyhow::bail!("unsupported Weavelo request"),
    }
}
#[cfg(target_os = "macos")]
fn weavelo_button(button: Button) -> u8 {
    match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
    }
}
#[cfg(target_os = "macos")]
fn weavelo_modifier_bit(usage: u16) -> Option<u8> {
    match usage {
        225 | 229 => Some(1 << 0), // SHIFT
        224 | 228 => Some(1 << 1), // CONTROL
        226 | 230 => Some(1 << 2), // ALT
        227 | 231 => Some(1 << 3), // META
        _ => None,
    }
}
#[cfg(target_os = "macos")]
fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if cur.len() + ch.len_utf8() > max_bytes && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}
/// Parses bounded Octendra UI desktop actions into one or more complete Weavelo
/// owner-local actions (no held keys/buttons left open across the lease).
#[cfg(target_os = "macos")]
fn parse_weavelo_steps(frame: &FrameRef, actions: &[Action]) -> Result<Vec<Value>> {
    ensure!(
        !actions.is_empty() && actions.len() <= MAX_ACTIONS,
        "one complete bounded gesture required"
    );
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
    let mut steps = Vec::new();
    let mut i = 0;
    while i < actions.len() {
        match &actions[i..] {
            [
                Action::PointerMove { x, y },
                Action::ButtonDown { button: b1 },
                Action::ButtonUp { button: b2 },
                Action::ButtonDown { button: b3 },
                Action::ButtonUp { button: b4 },
                Action::ButtonDown { button: b5 },
                Action::ButtonUp { button: b6 },
                ..,
            ] if b1 == b2 && b2 == b3 && b3 == b4 && b4 == b5 && b5 == b6 => {
                let (x, y) = point(*x, *y)?;
                steps.push(json!({
                    "kind": "triple_click",
                    "x": x,
                    "y": y,
                    "button": weavelo_button(*b1),
                }));
                i += 7;
            }
            [
                Action::PointerMove { x, y },
                Action::ButtonDown { button: b1 },
                Action::ButtonUp { button: b2 },
                Action::ButtonDown { button: b3 },
                Action::ButtonUp { button: b4 },
                ..,
            ] if b1 == b2 && b2 == b3 && b3 == b4 => {
                let (x, y) = point(*x, *y)?;
                steps.push(json!({
                    "kind": "double_click",
                    "x": x,
                    "y": y,
                    "button": weavelo_button(*b1),
                }));
                i += 5;
            }
            [
                Action::PointerMove { x: fx, y: fy },
                Action::ButtonDown {
                    button: Button::Left,
                },
                Action::PointerMove { x: tx, y: ty },
                Action::ButtonUp {
                    button: Button::Left,
                },
                ..,
            ] => {
                let (from_x, from_y) = point(*fx, *fy)?;
                let (to_x, to_y) = point(*tx, *ty)?;
                steps.push(
                    json!({"kind":"drag","from_x":from_x,"from_y":from_y,"to_x":to_x,"to_y":to_y}),
                );
                i += 4;
            }
            [
                Action::PointerMove { x, y },
                Action::ButtonDown { button },
                Action::ButtonUp { button: up },
                ..,
            ] if button == up => {
                let (x, y) = point(*x, *y)?;
                steps.push(json!({"kind":"click","x":x,"y":y,"button":weavelo_button(*button)}));
                i += 3;
            }
            [
                Action::PointerMove { x, y },
                Action::Wheel {
                    horizontal,
                    vertical,
                },
                ..,
            ] if i32::from(*horizontal).unsigned_abs() <= 1200
                && i32::from(*vertical).unsigned_abs() <= 1200 =>
            {
                let (x, y) = point(*x, *y)?;
                steps.push(
                    json!({"kind":"scroll","x":x,"y":y,"delta_x":horizontal,"delta_y":vertical}),
                );
                i += 2;
            }
            [
                Action::Unicode {
                    text,
                    key_down: true,
                },
                Action::Unicode {
                    text: release,
                    key_down: false,
                },
                ..,
            ] if !text.is_empty() && text.len() <= 8192 && release.is_empty() => {
                for chunk in utf8_chunks(text, 1024) {
                    steps.push(json!({"kind":"commit_text","text":chunk}));
                }
                i += 2;
            }
            [Action::KeyDown { usage }, Action::KeyUp { usage: up }, ..]
                if usage == up && *usage < 224 =>
            {
                let key = super::macos_worker::mac_key(*usage).context("unsupported HID key")?;
                steps.push(json!({"kind":"key_press","key_code":key}));
                i += 2;
            }
            rest => {
                // Balanced modifier chord: KeyDown(mod1..modK), KeyDown(key), KeyUp(key), KeyUp(modK..mod1).
                let mut mods = Vec::new();
                let mut mask = 0u8;
                let mut k = 0;
                while k < rest.len() {
                    if let Action::KeyDown { usage } = rest[k]
                        && let Some(bit) = weavelo_modifier_bit(usage)
                    {
                        ensure!(!mods.contains(&usage), "duplicate modifier");
                        mods.push(usage);
                        mask |= bit;
                        k += 1;
                    } else {
                        break;
                    }
                }
                ensure!(
                    !mods.is_empty() && rest.len() >= k * 2 + 2,
                    "one complete bounded gesture required"
                );
                let (Action::KeyDown { usage }, Action::KeyUp { usage: up }) =
                    (&rest[k], &rest[k + 1])
                else {
                    anyhow::bail!("one complete bounded gesture required");
                };
                ensure!(
                    usage == up && *usage < 224,
                    "one complete bounded gesture required"
                );
                for (offset, expected) in mods.iter().rev().enumerate() {
                    ensure!(
                        rest[k + 2 + offset] == Action::KeyUp { usage: *expected },
                        "unbalanced modifier chord"
                    );
                }
                let key = super::macos_worker::mac_key(*usage).context("unsupported HID key")?;
                steps.push(json!({"kind":"key_combo","key_code":key,"modifiers":mask}));
                i += k * 2 + 2;
            }
        }
    }
    ensure!(
        !steps.is_empty() && steps.len() <= 32,
        "one complete bounded gesture required"
    );
    Ok(steps)
}
#[cfg(target_os = "macos")]
fn weavelo_call(
    frame: &FrameRef,
    actions: &[Action],
    actor: Value,
    lease_id: uuid::Uuid,
) -> Result<Value> {
    let mut steps = parse_weavelo_steps(frame, actions)?;
    if steps.len() == 1 {
        Ok(json!({
            "operation": "act",
            "actor": actor,
            "lease_id": lease_id,
            "source_revision": frame.source_revision,
            "frame_sequence": frame.frame_sequence,
            "action": steps.pop().unwrap(),
        }))
    } else {
        Ok(json!({
            "operation": "batch_act",
            "actor": actor,
            "lease_id": lease_id,
            "source_revision": frame.source_revision,
            "frame_sequence": frame.frame_sequence,
            "keep_lease": false,
            "actions": steps,
        }))
    }
}
#[cfg(all(target_os = "macos", test))]
fn atomic_input(frame: &FrameRef, actions: &[Action]) -> Result<Value> {
    let mut steps = parse_weavelo_steps(frame, actions)?;
    ensure!(steps.len() == 1, "one complete bounded gesture required");
    Ok(steps.pop().unwrap())
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
    #[tokio::test]
    async fn reopened_gui_uses_current_generation_but_old_lease_cannot_validate() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("gui-receive.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let route = Route {
            input_platform: None,
            profile: dir.path().to_owned(),
            generation: "1-2".into(),
            gui_executable_sha256: "0".repeat(64),
            target: Target {
                peer_device_id: "approved-peer".into(),
                source: Source::MainDisplay,
                connection_id: 1,
            },
        };
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let len = stream.read_u32().await.unwrap() as usize;
                let mut request = vec![0; len];
                stream.read_exact(&mut request).await.unwrap();
                let body = serde_json::to_vec(&json!({"ok":true,"generation":"3-4","executable_sha256":"0".repeat(64),"computer_use":{"protocol":1,"observe":true}})).unwrap();
                stream.write_u32(body.len() as u32).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
        });
        let refreshed = refresh_route(&route, false).await.unwrap();
        assert_eq!(refreshed.generation, "3-4");
        assert_eq!(refreshed.target, route.target);
        assert_eq!(
            probe(&route, false).await.unwrap_err().to_string(),
            "octendra_authority_gui_generation_changed"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn private_gui_failures_preserve_codes_but_never_raw_messages() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("gui-receive.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let route = Route {
            input_platform: None,
            profile: dir.path().to_owned(),
            generation: "1-2".into(),
            gui_executable_sha256: "0".repeat(64),
            target: Target {
                peer_device_id: "fixture-peer".into(),
                source: Source::MainDisplay,
                connection_id: 1,
            },
        };
        let server = tokio::spawn(async move {
            for error in [
                "octendra_task_authority_expired",
                "permission error https://private/?token=secret",
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let count = stream.read_u32().await.unwrap() as usize;
                let mut request = vec![0; count];
                stream.read_exact(&mut request).await.unwrap();
                let body = serde_json::to_vec(&json!({"ok":false,"error":error})).unwrap();
                stream.write_u32(body.len() as u32).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
        });
        assert_eq!(
            exchange(&route, json!("Status"), 0)
                .await
                .unwrap_err()
                .to_string(),
            "octendra_task_authority_expired"
        );
        assert_eq!(
            exchange(&route, json!("Status"), 0)
                .await
                .unwrap_err()
                .to_string(),
            "GUI request unavailable"
        );
        server.await.unwrap();
    }

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
            let mut act_calls = 0usize;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let size = stream.read_u32().await.unwrap() as usize;
                assert!(size <= 4096);
                let mut raw = vec![0; size];
                stream.read_exact(&mut raw).await.unwrap();
                let value: Value = serde_json::from_slice(&raw).unwrap();
                if value == json!("Status") {
                    let body = json!({
                        "ok": true,
                        "generation": "1-2",
                        "executable_sha256": "0".repeat(64),
                        "computer_use": {"protocol": 1, "observe": true, "input": control},
                        "sessions": {"computer_use_mode": if control { "controlling" } else { "observing" }, "sessions": []}
                    });
                    let bytes = serde_json::to_vec(&body).unwrap();
                    stream.write_u32(bytes.len() as u32).await.unwrap();
                    stream.write_all(&bytes).await.unwrap();
                    continue;
                }
                let call = &value["ComputerUse"]["call"];
                assert_eq!(value["ComputerUse"]["generation"], "1-2");
                let operation = call["operation"].as_str().unwrap();
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
                        assert!(control);
                        act_calls += 1;
                        assert_eq!(call["source_revision"], 7);
                        assert_eq!(call["frame_sequence"], 1);
                        assert_eq!(call["action"], json!({"kind":"key_press","key_code":0}));
                        (json!({"ok":true,"effect_confirmed":false}), vec![])
                    }
                    "close" => (json!({"ok":true}), vec![]),
                    other => panic!("unexpected operation {other}"),
                };
                let bytes = serde_json::to_vec(&body).unwrap();
                stream.write_u32(bytes.len() as u32).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
                stream.write_all(&data).await.unwrap();
                if operation == "close" {
                    break;
                }
            }
            assert_eq!(act_calls, usize::from(control));
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
        // DoubleClick, Drag, KeyCombo, CommitText, and compound Fill map cleanly.
        assert_eq!(
            atomic_input(
                &frame,
                &[
                    Action::PointerMove { x: 50., y: 25. },
                    Action::ButtonDown {
                        button: Button::Left
                    },
                    Action::ButtonUp {
                        button: Button::Left
                    },
                    Action::ButtonDown {
                        button: Button::Left
                    },
                    Action::ButtonUp {
                        button: Button::Left
                    },
                ]
            )
            .unwrap(),
            json!({"kind":"double_click","x":32768,"y":32768,"button":0})
        );
        assert_eq!(
            atomic_input(
                &frame,
                &[
                    Action::PointerMove { x: 0., y: 0. },
                    Action::ButtonDown {
                        button: Button::Left
                    },
                    Action::PointerMove { x: 100., y: 50. },
                    Action::ButtonUp {
                        button: Button::Left
                    },
                ]
            )
            .unwrap(),
            json!({"kind":"drag","from_x":0,"from_y":0,"to_x":65535,"to_y":65535})
        );
        assert_eq!(
            atomic_input(
                &frame,
                &[
                    Action::KeyDown { usage: 0xe3 },
                    Action::KeyDown { usage: 6 },
                    Action::KeyUp { usage: 6 },
                    Action::KeyUp { usage: 0xe3 },
                ]
            )
            .unwrap(),
            json!({"kind":"key_combo","key_code":8,"modifiers":8})
        );
        let fill_call = weavelo_call(
            &frame,
            &[
                Action::PointerMove { x: 50., y: 25. },
                Action::ButtonDown {
                    button: Button::Left,
                },
                Action::ButtonUp {
                    button: Button::Left,
                },
                Action::KeyDown { usage: 0xe3 },
                Action::KeyDown { usage: 4 },
                Action::KeyUp { usage: 4 },
                Action::KeyUp { usage: 0xe3 },
                Action::Unicode {
                    text: "你好 Weavelo".into(),
                    key_down: true,
                },
                Action::Unicode {
                    text: String::new(),
                    key_down: false,
                },
            ],
            json!({"owner":"fixture"}),
            uuid::Uuid::nil(),
        )
        .unwrap();
        assert_eq!(fill_call["operation"], "batch_act");
        assert_eq!(fill_call["keep_lease"], false);
        assert_eq!(fill_call["actions"].as_array().unwrap().len(), 3);

        // TripleClick, Right DoubleClick, Ctrl+A KeyCombo, and UTF-8 <=1024B chunked CommitText.
        assert_eq!(
            atomic_input(
                &frame,
                &[
                    Action::PointerMove { x: 50., y: 25. },
                    Action::ButtonDown {
                        button: Button::Left
                    },
                    Action::ButtonUp {
                        button: Button::Left
                    },
                    Action::ButtonDown {
                        button: Button::Left
                    },
                    Action::ButtonUp {
                        button: Button::Left
                    },
                    Action::ButtonDown {
                        button: Button::Left
                    },
                    Action::ButtonUp {
                        button: Button::Left
                    },
                ]
            )
            .unwrap(),
            json!({"kind":"triple_click","x":32768,"y":32768,"button":0})
        );
        assert_eq!(
            atomic_input(
                &frame,
                &[
                    Action::PointerMove { x: 50., y: 25. },
                    Action::ButtonDown {
                        button: Button::Right
                    },
                    Action::ButtonUp {
                        button: Button::Right
                    },
                    Action::ButtonDown {
                        button: Button::Right
                    },
                    Action::ButtonUp {
                        button: Button::Right
                    },
                ]
            )
            .unwrap(),
            json!({"kind":"double_click","x":32768,"y":32768,"button":1})
        );
        assert_eq!(
            atomic_input(
                &frame,
                &[
                    Action::KeyDown { usage: 0xe0 },
                    Action::KeyDown { usage: 4 },
                    Action::KeyUp { usage: 4 },
                    Action::KeyUp { usage: 0xe0 },
                ]
            )
            .unwrap(),
            json!({"kind":"key_combo","key_code":0,"modifiers":2})
        );
        let long_text = "你好🙂Weavelo".repeat(250);
        let chunk_call = weavelo_call(
            &frame,
            &[
                Action::Unicode {
                    text: long_text.clone(),
                    key_down: true,
                },
                Action::Unicode {
                    text: String::new(),
                    key_down: false,
                },
            ],
            json!({"owner":"fixture"}),
            uuid::Uuid::nil(),
        )
        .unwrap();
        assert_eq!(chunk_call["operation"], "batch_act");
        let chunk_actions = chunk_call["actions"].as_array().unwrap();
        assert!(chunk_actions.len() >= 3);
        let mut rebuilt = String::new();
        for step in chunk_actions {
            assert_eq!(step["kind"], "commit_text");
            let piece = step["text"].as_str().unwrap();
            assert!(!piece.is_empty() && piece.len() <= 1024);
            rebuilt.push_str(piece);
        }
        assert_eq!(rebuilt, long_text);
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn weavelo_paused_by_human_sets_takeover_state_on_observe_and_blocks_act_without_ipc() {
        use std::os::unix::fs::PermissionsExt;
        for detect_stage in ["observe_status", "pre_act_status", "ipc_paused_code"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let listener =
                tokio::net::UnixListener::bind(dir.path().join("gui-receive.sock")).unwrap();
            std::fs::set_permissions(
                dir.path().join("gui-receive.sock"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let route = Route {
                input_platform: Some(InputPlatform::Windows),
                profile: dir.path().to_owned(),
                generation: "1-2".into(),
                gui_executable_sha256: "0".repeat(64),
                target: Target {
                    peer_device_id: "win-peer".into(),
                    source: Source::Window(99),
                    connection_id: 12,
                },
            };
            let identity = Identity {
                owner: "fixture".into(),
                task_id: uuid::Uuid::new_v4(),
                device_id: uuid::Uuid::new_v4(),
                desktop_id: route.desktop_id(),
            };
            let stage = detect_stage.to_string();
            let server = tokio::spawn(async move {
                let mut status_calls = 0usize;
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let size = stream.read_u32().await.unwrap() as usize;
                    let mut raw = vec![0; size];
                    stream.read_exact(&mut raw).await.unwrap();
                    let value: Value = serde_json::from_slice(&raw).unwrap();
                    if value == json!("Status") {
                        status_calls += 1;
                        let mode = match stage.as_str() {
                            "observe_status" => json!({"computer_use_mode": "paused_by_human"}),
                            "pre_act_status" if status_calls >= 2 => {
                                json!({"sessions": [{"device_id": "win-peer", "connection_id": 12, "computer_use_mode": "paused_by_human"}]})
                            }
                            _ => json!({"computer_use_mode": "controlling"}),
                        };
                        let body = json!({
                            "ok": true,
                            "generation": "1-2",
                            "executable_sha256": "0".repeat(64),
                            "computer_use": {"protocol": 1, "observe": true, "input": true},
                            "sessions": mode
                        });
                        let bytes = serde_json::to_vec(&body).unwrap();
                        stream.write_u32(bytes.len() as u32).await.unwrap();
                        stream.write_all(&bytes).await.unwrap();
                        continue;
                    }
                    let call = &value["ComputerUse"]["call"];
                    let operation = call["operation"].as_str().unwrap();
                    if stage != "ipc_paused_code" {
                        assert_ne!(operation, "act", "act must never be sent after paused_by_human");
                        assert_ne!(
                            operation, "batch_act",
                            "batch_act must never be sent after paused_by_human"
                        );
                    }
                    let (body, data) = match operation {
                        "open" => (
                            json!({"ok":true,"lease_id":call["lease_id"],"lease_ms":call["lease_ms"]}),
                            vec![],
                        ),
                        "observe" => (
                            json!({"ok":true,"lease_id":call["lease_id"],"source_revision":7,"frame_sequence":1,"size":[2,2],"bytes":16,"encoding":"rgba8_srgb"}),
                            vec![17; 16],
                        ),
                        "act" => (
                            json!({"ok":false,"code":"paused_by_human"}),
                            vec![],
                        ),
                        "close" => (json!({"ok":true}), vec![]),
                        other => panic!("unexpected operation {other}"),
                    };
                    let bytes = serde_json::to_vec(&body).unwrap();
                    stream.write_u32(bytes.len() as u32).await.unwrap();
                    stream.write_all(&bytes).await.unwrap();
                    stream.write_all(&data).await.unwrap();
                    if operation == "close" {
                        break;
                    }
                }
            });
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
                allow_input: true,
            };
            let mut session = None;
            let open_out = handle_outcome(
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
            let Response::Opened { binding, .. } = open_out.result.unwrap() else {
                panic!("not opened");
            };
            let obs_out = handle_outcome(
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
            if detect_stage == "observe_status" {
                assert_eq!(obs_out.takeover_state.as_deref(), Some("paused_by_human"));
            } else {
                assert_eq!(obs_out.takeover_state, None);
            }
            let Response::Observed { frame } = obs_out.result.unwrap() else {
                panic!("not observed");
            };
            let act_err = handle_outcome(
                &approval,
                &route,
                &mut session,
                uuid::Uuid::new_v4(),
                &Request::Act {
                    frame,
                    sequence: 1,
                    actions: vec![Action::KeyDown { usage: 4 }, Action::KeyUp { usage: 4 }],
                },
            )
            .await
            .unwrap_err();
            assert!(is_paused_error(&act_err));
            handle_outcome(
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
    }
    #[test]
    fn weavelo_input_platform_is_an_explicit_remote_grant_not_an_executor_inference() {
        let base = json!({"profile":"/synthetic/private","generation":"1-2","gui_executable_sha256":"c".repeat(64),
            "target":{"peer_device_id":"remote-peer","source":{"Window":42},"connection_id":7}});
        let route: Route = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(route.input_platform, None);
        for (raw, expected) in [
            ("macos", InputPlatform::Macos),
            ("windows", InputPlatform::Windows),
            ("linux", InputPlatform::Linux),
            ("android", InputPlatform::Android),
        ] {
            let mut valid = base.clone();
            valid["input_platform"] = json!(raw);
            assert_eq!(
                serde_json::from_value::<Route>(valid)
                    .unwrap()
                    .input_platform,
                Some(expected)
            );
        }
        for value in [json!("executor"), json!("auto"), json!("unknown"), json!(true)] {
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
