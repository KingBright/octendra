//! A real macOS guest uses its private virtio socket and the parent's own VZ
//! console. It never falls back to host input or depends on a DHCP address.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    ffi::{CStr, CString},
    io::{BufRead, BufReader, Read, Write},
    os::fd::{FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
    time::Duration,
};

const BACKEND: &str = "apple_vz_guest_private_devices";
const VERSION: u64 = 1;
const PORT: u32 = 18789;

#[repr(C, packed)]
struct VmAddress {
    length: u8,
    family: u8,
    reserved: u16,
    port: u32,
    cid: u32,
}

fn is_guest() -> bool {
    let mut bytes = [0u8; 128];
    let mut length = bytes.len();
    let result = unsafe {
        nix::libc::sysctlbyname(
            c"hw.model".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    result == 0
        && CStr::from_bytes_until_nul(&bytes).is_ok_and(|s| s.to_bytes().starts_with(b"VirtualMac"))
}

async fn request(value: Value) -> Result<Value> {
    tokio::task::spawn_blocking(move || {
        let fd = unsafe { nix::libc::socket(nix::libc::AF_VSOCK, nix::libc::SOCK_STREAM, 0) };
        ensure!(fd >= 0, "guest_control_socket_unavailable");
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let stream = UnixStream::from(owned);
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let address = VmAddress {
            length: std::mem::size_of::<VmAddress>() as u8,
            family: nix::libc::AF_VSOCK as u8,
            reserved: 0,
            port: PORT,
            cid: 2,
        };
        let result = unsafe {
            nix::libc::connect(
                fd,
                (&address as *const VmAddress).cast(),
                std::mem::size_of::<VmAddress>() as nix::libc::socklen_t,
            )
        };
        ensure!(
            result == 0,
            "guest_control_connect_failed: {}",
            std::io::Error::last_os_error()
        );
        let mut stream = stream;
        let mut payload = serde_json::to_vec(&value)?;
        ensure!(payload.len() < 64 * 1024, "guest_control_request_too_large");
        payload.push(b'\n');
        stream.write_all(&payload)?;
        let mut bytes = Vec::new();
        BufReader::new(stream.take(16 * 1024 * 1024 + 1)).read_until(b'\n', &mut bytes)?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024 && bytes.last() == Some(&b'\n'),
            "guest_control_reply_incomplete"
        );
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await?
}

struct Observed {
    capture: Capture,
    generation: Uuid,
    revision: u64,
}
async fn observe(workspace: &str, job: &Job) -> Result<Observed> {
    let value = request(json!({"workspace_id":workspace,"operation":"observe"})).await?;
    ensure!(
        value.get("error").is_none(),
        "{}: {}",
        value["error"].as_str().unwrap_or("guest_capture_failed"),
        value["message"].as_str().unwrap_or("")
    );
    ensure!(
        value["workspace_id"] == workspace,
        "ui_execution_context_mismatch"
    );
    let generation = Uuid::parse_str(
        value["generation"]
            .as_str()
            .context("guest_generation_missing")?,
    )?;
    let revision = value["revision"]
        .as_u64()
        .context("guest_revision_missing")?;
    let size: [u32; 2] = serde_json::from_value(value["size"].clone())?;
    ensure!(
        size[0] > 0 && size[0] <= 1280 && size[1] > 0 && size[1] <= 2048,
        "guest_frame_geometry_invalid"
    );
    let bytes = STANDARD.decode(value["data"].as_str().context("guest_frame_missing")?)?;
    let pixels = desktop::RawPixels {
        encoding: "rgba8_srgb".into(),
        size,
        color: [0; 4],
        plane_lengths: vec![bytes.len()],
        plane_sha256: vec![],
    };
    ensure!(
        value["encoding"] == pixels.encoding,
        "guest_pixel_format_invalid"
    );
    let image = image::Image::from_pixels(&pixels, &bytes)?;
    let mapping = desktop::FrameMapping {
        frame_size: size,
        content: desktop::ContentRect {
            left: 0,
            top: 0,
            width: size[0],
            height: size[1],
        },
        logical_size: size,
        desktop_position: [0, 0],
        rotation: desktop::Rotation::R0,
    };
    Ok(Observed {
        generation,
        revision,
        capture: Capture {
            image,
            frame: desktop::FrameRef {
                binding: desktop::Binding {
                    identity: desktop::Identity {
                        owner: job.owner.clone(),
                        device_id: Uuid::parse_str(&job.device_id)?,
                        desktop_id: format!("vm:{workspace}:{generation}"),
                        task_id: Uuid::parse_str(&job.id)?,
                    },
                    epoch: generation,
                },
                source_revision: revision,
                frame_sequence: revision,
                mapping,
            },
        },
    })
}

async fn remember(agent: &Agent, prefix: &str, observed: Observed) -> Result<Value> {
    let identity = observed.capture.frame.binding.identity.clone();
    let mut value = save(
        agent,
        prefix,
        &identity,
        observed.capture,
        BACKEND,
        &actions_for(BACKEND),
        VERSION,
    )
    .await?;
    value["capabilities"]["background"] = json!(true);
    value["source"] = json!({"kind":"apple_vz_guest", "generation":observed.generation,
        "revision":observed.revision,"input_backend":"vz_virtual_devices","clipboard":"guest_only"});
    Ok(value)
}

fn paste_actions(actions: Vec<desktop::Action>) -> Result<Vec<desktop::Action>> {
    unsafe extern "C" {
        fn octendra_macos_guest_copy(text: *const std::ffi::c_char) -> std::ffi::c_int;
    }
    let mut result = Vec::new();
    for action in actions {
        match action {
            desktop::Action::Unicode {
                text,
                key_down: true,
            } => {
                let text = CString::new(text).context("unsupported_text")?;
                let status = unsafe { octendra_macos_guest_copy(text.as_ptr()) };
                ensure!(
                    status != 2,
                    "ui_gui_session_required: use the Agent's gui/<uid> LaunchAgent"
                );
                ensure!(
                    status == 0,
                    "guest_clipboard_unavailable: native_status={status}"
                );
                result.extend([
                    desktop::Action::KeyDown { usage: 227 },
                    desktop::Action::KeyDown { usage: 25 },
                    desktop::Action::KeyUp { usage: 25 },
                    desktop::Action::KeyUp { usage: 227 },
                ]);
            }
            desktop::Action::Unicode {
                key_down: false, ..
            } => {}
            other => result.push(other),
        }
    }
    Ok(result)
}

pub(super) async fn perform(agent: &Agent, job: &Job, args: Arguments) -> Result<Value> {
    if !is_guest() {
        return Ok(
            json!({"error":"isolated_guest_required", "input_sent":false,"host_fallback":false}),
        );
    }
    let _guard = agent.ui_access.lock().await;
    let workspace = &job
        .ui_context
        .as_ref()
        .context("ui_workspace_gateway_required")?
        .id;
    let prefix = crate::hash(format!("{}:{}", job.owner, job.device_id));
    let observed = match observe(workspace, job).await {
        Ok(value) => value,
        Err(error) => {
            return Ok(
                json!({"error":"ui_workspace_unavailable", "reason":error.to_string(),
            "input_sent":false,"host_fallback":false}),
            );
        }
    };
    let surface = &observed.capture.frame.binding.identity.desktop_id;
    if args
        .view
        .as_deref()
        .is_some_and(|v| v != "active" && v != "background" && v != surface)
    {
        let mut value = failure("unsupported_view", &actions_for(BACKEND));
        value["available_views"] = json!([surface]);
        return Ok(value);
    }
    if job.tool == "ui_observe" {
        return remember(agent, &prefix, observed).await;
    }
    let key = format!(
        "{prefix}:{}",
        args.observation_id
            .as_deref()
            .context("observation_required")?
    );
    let saved = agent.store.get::<Saved>("ui_observation", &key).await?;
    let Some(saved) = saved.filter(|s| {
        s.surface == *surface
            && s.provider_generation == Some(agent.ui_generation)
            && s.capability_version == Some(VERSION)
    }) else {
        let mut value = failure("stale_observation", &actions_for(BACKEND));
        value["observation"] = remember(agent, &prefix, observed).await?;
        return Ok(value);
    };
    let action = args.action.as_ref().context("action_required")?;
    if !actions_for(BACKEND).contains(&action.name()) {
        return Ok(failure("unsupported_action", &actions_for(BACKEND)));
    }
    let native = match prepare(&saved, &observed.capture, action) {
        Ok(value) => value,
        Err(code) => {
            let mut value = failure(code, &actions_for(BACKEND));
            value["observation"] = remember(agent, &prefix, observed).await?;
            return Ok(value);
        }
    };
    if !consume(agent, &key, &saved, VERSION).await? {
        return Ok(failure("stale_observation", &actions_for(BACKEND)));
    }
    let native = match paste_actions(native) {
        Ok(value) => value,
        Err(error) => {
            let reason = error.to_string();
            let gui = reason.starts_with("ui_gui_session_required:");
            return Ok(
                json!({"error":if gui {"ui_gui_session_required"} else {"guest_clipboard_unavailable"},
                "reason":reason,"input_sent":false,"host_fallback":false,
                "next_action":if gui {"start_existing_agent_in_user_gui_session"} else {"ui_observe"}}),
            );
        }
    };
    let result = request(json!({"workspace_id":workspace,"operation":"input", "generation":observed.generation,
        "revision":observed.revision, "size":observed.capture.frame.mapping.frame_size, "actions":native})).await;
    let mut result = match result {
        Ok(value) => value,
        Err(_) => json!({"error":"outcome_unknown", "automatic_replay_safe":false,
            "original_operation_id":job.id,"next_action":"operation_get"}),
    };
    result["operation_id"] = json!(job.id);
    if args.observe_after != Some(false) && result["error"] != "outcome_unknown" {
        match observe(workspace, job).await {
            Ok(value) => result["observation"] = remember(agent, &prefix, value).await?,
            Err(_) => {
                result["observation_error"] =
                    json!({"error":"observe_failed","next_action":"ui_observe"})
            }
        }
    }
    Ok(result)
}
