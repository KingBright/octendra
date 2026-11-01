//! Device-neutral observe/act facade. Worker identities and short native leases
//! are internal to each operation; thinking time never keeps a worker alive.
mod image;
#[cfg(test)]
mod tests;
use crate::{agent::Agent, gateway::Job};
use anyhow::{Context, Result, ensure};
use octendra_desktop_protocol::{
    self as desktop,
    ui::{Action, Direction, Target},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Arguments {
    pub device: String,
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub observation_id: Option<String>,
    #[serde(default)]
    pub action: Option<Action>,
    #[serde(default)]
    pub observe_after: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)] // Gateway-owned envelope metadata retained for strict decoding.
    pub idempotency_key: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub response_mode: Option<String>,
}
pub(crate) fn validate(name: &str, args: &Value) -> Result<()> {
    let args: Arguments = serde_json::from_value(args.clone())
        .map_err(|_| anyhow::anyhow!("invalid_arguments: UI request"))?;
    ensure!(
        !args.device.is_empty() && args.device.len() <= 256,
        "invalid_arguments: device"
    );
    ensure!(
        args.view
            .as_ref()
            .is_none_or(|v| !v.is_empty() && v.len() <= 256),
        "invalid_arguments: view"
    );
    if name == "ui_act" {
        ensure!(
            args.observation_id
                .as_ref()
                .is_some_and(|id| Uuid::parse_str(id).is_ok())
                && args.action.as_ref().is_some_and(Action::valid)
                && args.view.is_none(),
            "invalid_arguments: observation_id/action"
        );
    } else {
        ensure!(
            args.action.is_none() && args.observation_id.is_none() && args.observe_after.is_none(),
            "invalid_arguments: observe is read-only"
        );
    }
    Ok(())
}

pub(crate) fn action_schema() -> Value {
    let target = json!({"type":"object","properties":{"ref":{"type":"string"},
        "x":{"type":"number","minimum":0},"y":{"type":"number","minimum":0}},
        "oneOf":[{"required":["ref"]},{"required":["x","y"]}],"additionalProperties":false,
        "description":"Use either an element ref from this observation, or x/y in its returned image."});
    let mut variants = Vec::new();
    let mut add = |name: &str, mut properties: Value, required: Vec<&str>| {
        properties["type"] = json!({"type":"string","const":name});
        let mut fields = vec!["type"];
        fields.extend(required);
        variants.push(json!({"type":"object","properties":properties,"required":fields,"additionalProperties":false}));
    };
    for name in ["click", "double_click", "secondary_click", "long_press"] {
        add(name, json!({"target":target}), vec!["target"]);
    }
    for name in ["type_text", "fill"] {
        add(
            name,
            json!({"target":target,"text":{"type":"string"}}),
            vec!["text"],
        );
    }
    add(
        "scroll",
        json!({"target":target,"direction":{"type":"string","enum":["up","down","left","right"],
        "description":"Content traversal direction; adapters map wheel/swipe direction."}}),
        vec!["direction"],
    );
    add(
        "drag",
        json!({"from":target,"to":target}),
        vec!["from", "to"],
    );
    add(
        "key",
        json!({"key":{"type":"string","description":"Logical key, e.g. Enter, Escape, ArrowDown, a."},
        "modifiers":{"type":"array","items":{"type":"string","enum":["shift","ctrl","alt","meta"]}}}),
        vec!["key"],
    );
    add("launch_app", json!({"app":{"type":"string"}}), vec!["app"]);
    add(
        "pinch",
        json!({"center":target,"scale":{"type":"number","exclusiveMinimum":0}}),
        vec!["center", "scale"],
    );
    for name in ["navigate_back", "go_home", "app_switcher"] {
        add(name, json!({}), vec![]);
    }
    json!({"type":"object","x-ui-action":true,"oneOf":variants})
}

/// Screenshots are MCP image blocks, never base64 repeated as model-visible text.
/// The durable result remains unchanged; this only formats the response clone.
pub(crate) fn mcp_images(value: &mut Value) -> Vec<rmcp::model::ContentBlock> {
    fn visit(value: &mut Value, content: &mut Vec<rmcp::model::ContentBlock>) {
        if let Some(image) = value.get_mut("image").and_then(Value::as_object_mut)
            && let Some(data) = image.remove("data")
        {
            let mime = image.get("mime_type").and_then(Value::as_str).unwrap_or("");
            if matches!(mime, "image/jpeg" | "image/png")
                && data.as_str().is_some_and(|s| s.len() <= 400 * 1024)
                && let Ok(block) =
                    serde_json::from_value(json!({"type":"image","data":data,"mimeType":mime}))
            {
                image.insert("content_index".into(), json!(content.len() + 1));
                content.push(block);
            } else {
                image.insert("error".into(), json!("invalid_image_payload"));
            }
        }
        for key in ["observation", "result"] {
            if let Some(child) = value.get_mut(key) {
                visit(child, content);
            }
        }
        if let Some(operations) = value.get_mut("operations").and_then(Value::as_array_mut) {
            for child in operations {
                visit(child, content);
            }
        }
    }
    let mut content = Vec::new();
    visit(value, &mut content);
    content
}

#[derive(Serialize, Deserialize)]
struct Saved {
    id: String,
    surface: String,
    mapping: desktop::FrameMapping,
    image_size: [u32; 2],
    tiles: Vec<String>,
}
struct Capture {
    frame: desktop::FrameRef,
    image: image::Image,
}
fn actions_for(backend: &str) -> Vec<&'static str> {
    if backend == "macos_pid_private_window_poc" {
        vec![
            "click",
            "secondary_click",
            "type_text",
            "fill",
            "scroll",
            "drag",
            "key",
        ]
    } else if backend == "weavelo_source_canvas_bounded_gesture" {
        vec!["click", "secondary_click", "scroll", "key"]
    } else if backend == "windows_wgc_uia_standard_control_click_only" {
        vec!["click"]
    } else {
        vec![]
    }
}
fn failure(code: &str, actions: &[&str]) -> Value {
    json!({"error":code,"input_sent":false,"supported_actions":actions,"next_action":"ui_observe"})
}

pub(crate) async fn perform(agent: &Agent, job: &Job) -> Result<Value> {
    validate(&job.tool, &job.arguments)?;
    let args: Arguments = serde_json::from_value(job.arguments.clone())?;
    ensure!(args.device == job.device_id, "ui_device_identity_mismatch");
    let _guard = agent.ui_access.lock().await;
    let capability = agent.desktop_workers.capabilities().await;
    let supported = actions_for(&capability.backend);
    let Some((version, approved)) = agent.desktop_workers.current_capability().await else {
        return Ok(
            json!({"error":"ui_unavailable","input_sent":false,"readiness":capability,
            "supported_actions":supported}),
        );
    };
    ensure!(
        approved.owner == job.owner && approved.device_id.to_string() == job.device_id,
        "ui_owner_device_mismatch"
    );
    if !capability.available {
        return Ok(
            json!({"error":"ui_unavailable","input_sent":false,"readiness":capability,
            "supported_actions":supported}),
        );
    }
    let key = crate::hash(format!("{}:{}", job.owner, job.device_id));
    let identity = desktop::Identity {
        owner: job.owner.clone(),
        device_id: approved.device_id,
        desktop_id: approved.desktop_id.clone(),
        task_id: Uuid::new_v4(),
    };
    let operation = Uuid::parse_str(&job.id)?;
    let saved = if job.tool == "ui_act" {
        let saved = agent.store.get::<Saved>("ui_observation", &key).await?;
        match saved {
            Some(saved)
                if Some(saved.id.as_str()) == args.observation_id.as_deref()
                    && saved.surface == approved.desktop_id =>
            {
                Some(saved)
            }
            _ => {
                let mut error = failure("stale_observation", &supported);
                let (readiness, capture) =
                    run(agent, operation, &identity, version, None, None).await?;
                if let Some(capture) = capture {
                    error["observation"] = save(
                        agent,
                        &key,
                        &identity,
                        capture,
                        &capability.backend,
                        &supported,
                    )
                    .await?;
                } else {
                    error["observation_error"] = readiness;
                }
                return Ok(error);
            }
        }
    } else {
        None
    };
    if args
        .view
        .as_deref()
        .is_some_and(|v| v != "active" && v != approved.desktop_id)
    {
        let mut error = failure("unsupported_view", &supported);
        error["available_views"] = json!([approved.desktop_id]);
        return Ok(error);
    }
    if let Some(action) = &args.action
        && !supported.contains(&action.name())
    {
        return Ok(failure("unsupported_action", &supported));
    }
    if capability.backend == "weavelo_source_canvas_bounded_gesture"
        && matches!(&args.action,Some(Action::Key { modifiers,.. }) if !modifiers.is_empty())
    {
        return Ok(failure("unsupported_modifiers", &supported));
    }
    if let Some(saved) = &saved {
        // Consume before any input. A new request cannot replay an uncertain
        // action by changing observe_after or choosing another retry key.
        let consumed = agent.store.take::<Saved>("ui_observation", &key).await?;
        if consumed.as_ref().is_none_or(|s| s.id != saved.id) {
            return Ok(failure("stale_observation", &supported));
        }
    }
    let (mut result, capture) = run(
        agent,
        operation,
        &identity,
        version,
        saved.as_ref(),
        args.action.as_ref(),
    )
    .await?;
    if let Some(capture) = capture {
        let observation = save(
            agent,
            &key,
            &identity,
            capture,
            &capability.backend,
            &supported,
        )
        .await?;
        if job.tool == "ui_observe" {
            return Ok(observation);
        }
        result["observation"] = observation;
        if result.get("error").is_some() {
            result["next_action"] = json!("ui_act");
        }
    } else if job.tool == "ui_act"
        && args.observe_after != Some(false)
        && result["error"] != "outcome_unknown"
    {
        let observed = run(agent, operation, &identity, version, None, None).await;
        match observed {
            Ok((_, Some(capture))) => match save(
                agent,
                &key,
                &identity,
                capture,
                &capability.backend,
                &supported,
            )
            .await
            {
                Ok(observation) => result["observation"] = observation,
                Err(_) => {
                    result["observation_error"] =
                        json!({"error":"observe_failed","next_action":"ui_observe"})
                }
            },
            Ok((error, _)) => result["observation_error"] = error,
            Err(_) => {
                result["observation_error"] =
                    json!({"error":"observe_failed","next_action":"ui_observe"})
            }
        }
    }
    result["operation_id"] = json!(job.id);
    Ok(result)
}

async fn run(
    agent: &Agent,
    operation: Uuid,
    identity: &desktop::Identity,
    version: u64,
    saved: Option<&Saved>,
    action: Option<&Action>,
) -> Result<(Value, Option<Capture>)> {
    let workers = &agent.desktop_workers;
    let open = workers
        .dispatch(
            operation,
            identity,
            version,
            desktop::Request::Open {
                identity: identity.clone(),
                lease_ms: desktop::MAX_LEASE_MS,
            },
        )
        .await?;
    if open.get("error").is_some() {
        return Ok((open, None));
    }
    let binding: desktop::Binding = serde_json::from_value(open["binding"].clone())?;
    let result = async {
        let observed_id = Uuid::new_v4();
        let value = workers.dispatch(observed_id, identity, version,
            desktop::Request::Observe { binding:binding.clone() }).await?;
        if value.get("error").is_some() { return Ok((value, None)); }
        let frame: desktop::FrameRef = serde_json::from_value(value["frame"].clone())?;
        let artifact = workers.artifact(observed_id).await?;
        let bytes = workers.read_frame(&artifact, identity, version).await;
        workers.retire_artifact(observed_id).await?;
        let image = image::Image::from_pixels(&artifact.pixels, &bytes?)?;
        let capture = Capture { frame, image };
        let Some(action) = action else { return Ok((json!({}), Some(capture))); };
        let saved = saved.context("observation_required")?;
        let native = match prepare(saved, &capture, action) {
            Ok(actions) => actions,
            Err(code) => return Ok((failure(code, &[]), Some(capture))),
        };
        if native.is_empty() {
            return Ok((json!({"state":"accepted","accepted_actions":0,"input_sent":false,"effect_confirmed":false}), Some(capture)));
        }
        let mut value = workers.dispatch(operation, identity, version, desktop::Request::Act {
            frame:capture.frame, sequence:1, actions:native,
        }).await?;
        value["input_sent"] = json!(if value.get("error").is_none() { Some(true) } else { None });
        value["effect_confirmed"] = json!(false);
        Ok((value, None))
    }.await;
    let closed = workers
        .dispatch(
            operation,
            identity,
            version,
            desktop::Request::Close { binding },
        )
        .await;
    if !closed.as_ref().is_ok_and(|v| v["state"] == "closed") {
        return Ok((
            json!({"error":"outcome_unknown","automatic_replay_safe":false,
            "original_operation_id":operation,"next_action":"operation_get"}),
            None,
        ));
    }
    result
}
async fn save(
    agent: &Agent,
    key: &str,
    identity: &desktop::Identity,
    capture: Capture,
    backend: &str,
    actions: &[&str],
) -> Result<Value> {
    let id = Uuid::new_v4().to_string();
    let saved = Saved {
        id: id.clone(),
        surface: identity.desktop_id.clone(),
        mapping: capture.frame.mapping.clone(),
        image_size: capture.image.size,
        tiles: capture.image.tiles(),
    };
    agent
        .store
        .put("ui_observation", key, &saved, i64::MAX)
        .await?;
    Ok(
        json!({"observation_id":id,"device":identity.device_id,"view":identity.desktop_id,
        "image":capture.image.value(),"elements":[],"capabilities":{"backend":backend,"actions":actions,
            "coordinates":"image_pixels","semantic_elements":false,"background":false},
        "readiness":{"available":true},"observed_at_ms":crate::now_ms()}),
    )
}

fn point(target: &Target, size: [u32; 2]) -> std::result::Result<[f64; 2], &'static str> {
    if target.element.is_some() {
        return Err("unsupported_target");
    }
    match (target.x, target.y) {
        (Some(x), Some(y))
            if x.is_finite()
                && y.is_finite()
                && x >= 0.0
                && y >= 0.0
                && x <= f64::from(size[0] - 1)
                && y <= f64::from(size[1] - 1) =>
        {
            Ok([x, y])
        }
        _ => Err("invalid_coordinates"),
    }
}
fn prepare(
    saved: &Saved,
    capture: &Capture,
    action: &Action,
) -> std::result::Result<Vec<desktop::Action>, &'static str> {
    if saved.mapping != capture.frame.mapping || saved.image_size != capture.image.size {
        return Err("stale_observation");
    }
    let mut points = Vec::new();
    let mut map = |target: &Target| -> std::result::Result<desktop::Action, &'static str> {
        let p = point(target, saved.image_size)?;
        points.push(p);
        let raw = capture.frame.mapping.frame_size;
        Ok(desktop::Action::PointerMove {
            x: p[0] * f64::from(raw[0] - 1)
                / f64::from(saved.image_size[0].saturating_sub(1).max(1)),
            y: p[1] * f64::from(raw[1] - 1)
                / f64::from(saved.image_size[1].saturating_sub(1).max(1)),
        })
    };
    use desktop::{Action as A, Button};
    let click = |button| [A::ButtonDown { button }, A::ButtonUp { button }];
    let mut actions = Vec::new();
    match action {
        Action::Click { target } | Action::SecondaryClick { target } => {
            actions.push(map(target)?);
            actions.extend(click(if matches!(action, Action::SecondaryClick { .. }) {
                Button::Right
            } else {
                Button::Left
            }));
        }
        Action::TypeText { text, target } | Action::Fill { text, target } => {
            if let Some(target) = target {
                actions.push(map(target)?);
                actions.extend(click(Button::Left));
            }
            if matches!(action, Action::Fill { .. }) {
                actions.extend([
                    A::KeyDown { usage: 0xe3 },
                    A::KeyDown { usage: 4 },
                    A::KeyUp { usage: 4 },
                    A::KeyUp { usage: 0xe3 },
                ]);
            }
            if text.is_empty() && matches!(action, Action::Fill { .. }) {
                actions.extend([A::KeyDown { usage: 42 }, A::KeyUp { usage: 42 }]);
            } else if !text.is_empty() {
                actions.extend([
                    A::Unicode {
                        text: text.clone(),
                        key_down: true,
                    },
                    A::Unicode {
                        text: String::new(),
                        key_down: false,
                    },
                ]);
            }
        }
        Action::Scroll { direction, target } => {
            actions.push(if let Some(target) = target {
                map(target)?
            } else {
                map(&Target {
                    element: None,
                    x: Some(f64::from(saved.image_size[0] - 1) / 2.0),
                    y: Some(f64::from(saved.image_size[1] - 1) / 2.0),
                })?
            });
            let (horizontal, vertical) = match direction {
                Direction::Down => (0, -100),
                Direction::Up => (0, 100),
                Direction::Right => (-100, 0),
                Direction::Left => (100, 0),
            };
            actions.push(A::Wheel {
                horizontal,
                vertical,
            });
        }
        Action::Drag { from, to } => {
            actions.extend([
                map(from)?,
                A::ButtonDown {
                    button: Button::Left,
                },
                map(to)?,
                A::ButtonUp {
                    button: Button::Left,
                },
            ]);
        }
        Action::Key { key, modifiers } => {
            let usage = logical_key(key).ok_or("unsupported_key")?;
            let mut modifiers: Vec<_> = modifiers
                .iter()
                .map(|m| match m.as_str() {
                    "shift" => 0xe1,
                    "ctrl" => 0xe0,
                    "alt" => 0xe2,
                    _ => 0xe3,
                })
                .collect();
            modifiers.sort_unstable();
            modifiers.dedup();
            for usage in &modifiers {
                actions.push(A::KeyDown { usage: *usage });
            }
            actions.extend([A::KeyDown { usage }, A::KeyUp { usage }]);
            for usage in modifiers.into_iter().rev() {
                actions.push(A::KeyUp { usage });
            }
        }
        _ => return Err("unsupported_action"),
    }
    let tiles = capture.image.tiles();
    let cols = saved.image_size[0].div_ceil(64) as usize;
    // Current-focus intents have no old pixel target. Cursor blinking must not
    // block them; the worker still checks the live window and fresh native frame.
    if !points.is_empty() {
        for [x, y] in points {
            let tile = y as usize / 64 * cols + x as usize / 64;
            if saved.tiles.get(tile) != tiles.get(tile) {
                return Err("stale_observation");
            }
        }
    }
    Ok(actions)
}
fn logical_key(key: &str) -> Option<u16> {
    Some(match key.to_ascii_lowercase().as_str() {
        "enter" => 40,
        "escape" => 41,
        "backspace" => 42,
        "tab" => 43,
        "space" => 44,
        "delete" => 76,
        "home" => 74,
        "end" => 77,
        "pageup" => 75,
        "pagedown" => 78,
        "arrowright" | "right" => 79,
        "arrowleft" | "left" => 80,
        "arrowdown" | "down" => 81,
        "arrowup" | "up" => 82,
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_lowercase() => {
            u16::from(k.as_bytes()[0] - b'a') + 4
        }
        k if k.len() == 1 && (b'1'..=b'9').contains(&k.as_bytes()[0]) => {
            u16::from(k.as_bytes()[0] - b'1') + 30
        }
        "0" => 39,
        _ => return None,
    })
}
