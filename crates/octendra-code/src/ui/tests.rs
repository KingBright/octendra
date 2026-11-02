use super::*;
use desktop::{ContentRect, FrameMapping, Rotation};
fn capture() -> Capture {
    Capture {
        frame: desktop::FrameRef {
            binding: desktop::Binding {
                identity: desktop::Identity {
                    owner: "test".into(),
                    task_id: Uuid::new_v4(),
                    device_id: Uuid::new_v4(),
                    desktop_id: "window".into(),
                },
                epoch: Uuid::new_v4(),
            },
            source_revision: 1,
            frame_sequence: 1,
            mapping: FrameMapping {
                frame_size: [256, 128],
                content: ContentRect {
                    left: 0,
                    top: 0,
                    width: 256,
                    height: 128,
                },
                logical_size: [128, 64],
                desktop_position: [100, 200],
                rotation: Rotation::R0,
            },
        },
        image: image::Image {
            size: [128, 64],
            rgb: vec![0; 128 * 64 * 3],
            jpeg: vec![],
        },
    }
}
fn saved(capture: &Capture) -> Saved {
    Saved {
        id: Uuid::new_v4().to_string(),
        surface: "window".into(),
        mapping: capture.frame.mapping.clone(),
        image_size: capture.image.size,
        tiles: capture.image.tiles(),
        resource_revision: 0,
        provider_generation: None,
        capability_version: None,
    }
}
fn action(value: Value) -> Action {
    serde_json::from_value(value).unwrap()
}

#[test]
fn image_pixels_map_once_and_stale_targets_never_produce_input() {
    let mut capture = capture();
    let saved = saved(&capture);
    let click = action(json!({"type":"click","target":{"x":127,"y":63}}));
    let native = prepare(&saved, &capture, &click).unwrap();
    assert_eq!(
        native[0],
        desktop::Action::PointerMove { x: 255.0, y: 127.0 }
    );
    assert_eq!(
        capture.frame.mapping.map(255.0, 127.0).unwrap(),
        desktop::SourcePoint { x: 127.0, y: 63.0 }
    );
    capture.image.rgb[(63 * 128 + 127) * 3] = 255;
    assert_eq!(
        prepare(&saved, &capture, &click).unwrap_err(),
        "stale_observation"
    );
    // An unrelated animation in the other tile does not prevent clicking this target.
    assert!(
        prepare(
            &saved,
            &capture,
            &action(json!({"type":"click","target":{"x":1,"y":1}}))
        )
        .is_ok()
    );
    capture.frame.mapping.desktop_position[0] += 1;
    assert_eq!(
        prepare(&saved, &capture, &click).unwrap_err(),
        "stale_observation"
    );
}
#[test]
fn fill_empty_clears_and_unicode_insert_never_selects_all() {
    let capture = capture();
    let saved = saved(&capture);
    let insert = prepare(
        &saved,
        &capture,
        &action(json!({"type":"type_text","text":"中文🙂"})),
    )
    .unwrap();
    assert_eq!(insert.len(), 2);
    assert_eq!(
        insert[0],
        desktop::Action::Unicode {
            text: "中文🙂".into(),
            key_down: true
        }
    );
    assert_eq!(
        insert[1],
        desktop::Action::Unicode {
            text: "".into(),
            key_down: false
        }
    );
    let clear = prepare(&saved, &capture, &action(json!({"type":"fill","text":""}))).unwrap();
    assert!(matches!(clear[4], desktop::Action::KeyDown { usage: 42 }));
}
#[test]
fn current_focus_intents_ignore_unrelated_pixel_changes() {
    let mut capture = capture();
    let saved = saved(&capture);
    capture.image.rgb[0] = 255;
    for value in [
        json!({"type":"type_text","text":"继续"}),
        json!({"type":"key","key":"Enter"}),
    ] {
        assert!(prepare(&saved, &capture, &action(value)).is_ok());
    }
}
#[test]
fn catalog_requires_observation_authority_and_never_exposes_worker_choreography() {
    for (tool, args) in [
        ("ui_observe", json!({"device":"phone"})),
        (
            "ui_act",
            json!({"device":"computer",
        "observation_id":Uuid::new_v4(),"action":{"type":"click","target":{"ref":"n1"}}}),
        ),
    ] {
        crate::tools::validate(tool, &args).unwrap();
        let schema = crate::tools::catalog()
            .into_iter()
            .find(|t| t.name == tool)
            .unwrap()
            .input_schema;
        assert!(schema["properties"].get("lease_ms").is_none());
        assert!(schema["properties"].get("task_id").is_none());
        assert!(!crate::tools::requires_desktop_observe(tool));
    }
    assert!(!crate::tools::scopes_allow(
        &["desktop:input".into()],
        "ui_act"
    ));
    assert!(crate::tools::scopes_allow(
        &["code:read".into(), "terminal:exec".into()],
        "ui_act"
    ));
    assert!(!crate::tools::scopes_allow(&["code:read".into()], "ui_act"));
    assert!(
        crate::tools::validate(
            "ui_observe",
            &json!({"device":"phone","action":{"type":"go_home"}})
        )
        .is_err()
    );
    assert!(crate::tools::validate("ui_act",&json!({"device":"phone","observation_id":Uuid::new_v4(),"action":{"type":"click","target":{"x":-1,"y":0}}})).is_err());
}
#[test]
fn mcp_images_are_native_blocks_including_operation_recovery() {
    let mut value = json!({"operations":[{"observation":{"image":{"data":"/9j/2Q==","mime_type":"image/jpeg","width":1,"height":1}}}]});
    let blocks = mcp_images(&mut value);
    assert_eq!(blocks.len(), 1);
    assert!(!value.to_string().contains("/9j/2Q=="));
    assert_eq!(
        value["operations"][0]["observation"]["image"]["content_index"],
        1
    );
}

#[tokio::test]
async fn observations_from_two_chats_remain_independent_and_cache_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut picture = capture();
    let identity = picture.frame.binding.identity.clone();
    let agent = Agent::new(crate::AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: identity.device_id.to_string(),
        device_token: crate::random(),
        state_dir: dir.path().join("state"),
        roots: vec![dir.path().to_owned()],
        allow_write: false,
        allow_exec: false,
        shell: "/bin/sh".into(),
    })
    .await
    .unwrap();
    let direct = crate::gateway::Job {
        ui_context: None,
        id: Uuid::new_v4().to_string(),
        device_id: identity.device_id.to_string(),
        owner: identity.owner.clone(),
        tool: "ui_observe".into(),
        arguments: json!({"device":identity.device_id}),
    };
    let refused = perform(&agent, &direct).await.unwrap();
    assert_eq!(refused["error"], "ui_workspace_gateway_required");
    assert_eq!(refused["input_sent"], false);
    let prefix = crate::hash(format!("{}:{}", identity.owner, identity.device_id));
    let first = save(&agent, &prefix, &identity, picture, "fixture", &[], 1)
        .await
        .unwrap();
    picture = capture();
    let second = save(&agent, &prefix, &identity, picture, "fixture", &[], 1)
        .await
        .unwrap();
    let key = |value: &Value| format!("{prefix}:{}", value["observation_id"].as_str().unwrap());
    let saved_first = agent
        .store
        .get::<Saved>("ui_observation", &key(&first))
        .await
        .unwrap()
        .unwrap();
    assert!(
        consume(&agent, &key(&first), &saved_first, 1)
            .await
            .unwrap()
    );
    assert!(
        agent
            .store
            .take::<Saved>("ui_observation", &key(&first))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        agent
            .store
            .get::<Saved>("ui_observation", &key(&second))
            .await
            .unwrap()
            .is_some()
    );
    let saved_second = agent
        .store
        .get::<Saved>("ui_observation", &key(&second))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !consume(&agent, &key(&second), &saved_second, 1)
            .await
            .unwrap()
    );
    let fresh = save(&agent, &prefix, &identity, capture(), "fixture", &[], 1)
        .await
        .unwrap();
    let saved_fresh = agent
        .store
        .get::<Saved>("ui_observation", &key(&fresh))
        .await
        .unwrap()
        .unwrap();
    assert!(
        consume(&agent, &key(&fresh), &saved_fresh, 1)
            .await
            .unwrap()
    );
    for _ in 0..128 {
        save(&agent, &prefix, &identity, capture(), "fixture", &[], 1)
            .await
            .unwrap();
    }
    assert!(
        agent
            .store
            .get::<Saved>("ui_observation", &key(&second))
            .await
            .unwrap()
            .is_none()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kv WHERE kind='ui_observation'")
        .fetch_one(&agent.store.pool)
        .await
        .unwrap();
    assert_eq!(count, 128);
}

#[tokio::test]
async fn durable_observations_require_the_current_agent_and_native_capability() {
    let dir = tempfile::tempdir().unwrap();
    let picture = capture();
    let identity = picture.frame.binding.identity.clone();
    let config = crate::AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: identity.device_id.to_string(),
        device_token: crate::random(),
        state_dir: dir.path().join("state"),
        roots: vec![dir.path().to_owned()],
        allow_write: false,
        allow_exec: false,
        shell: "/bin/sh".into(),
    };
    let agent = Agent::new(config.clone()).await.unwrap();
    let generation = agent.ui_generation;
    let prefix = crate::hash(format!("{}:{}", identity.owner, identity.device_id));
    let old = save(&agent, &prefix, &identity, picture, "fixture", &[], 9)
        .await
        .unwrap();
    let key = |value: &Value| format!("{prefix}:{}", value["observation_id"].as_str().unwrap());
    let old_key = key(&old);
    drop(agent);

    // Reopen the same durable state, with the same device and surface, as the
    // restarted Agent. Identical pixels must not authorize old input.
    let agent = Agent::new(config).await.unwrap();
    assert_ne!(agent.ui_generation, generation);
    let old = agent
        .store
        .get::<Saved>("ui_observation", &old_key)
        .await
        .unwrap()
        .unwrap();
    assert!(!consume(&agent, &old_key, &old, 9).await.unwrap());

    let fresh = save(&agent, &prefix, &identity, capture(), "fixture", &[], 10)
        .await
        .unwrap();
    let fresh_key = key(&fresh);
    let fresh = agent
        .store
        .get::<Saved>("ui_observation", &fresh_key)
        .await
        .unwrap()
        .unwrap();
    // Renewing or replacing the provider requires a new observation, without
    // consuming the original record or requiring a new task approval.
    assert!(!consume(&agent, &fresh_key, &fresh, 11).await.unwrap());
    let mut legacy = serde_json::to_value(&fresh).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("provider_generation");
    legacy.as_object_mut().unwrap().remove("capability_version");
    let legacy: Saved = serde_json::from_value(legacy).unwrap();
    assert!(!consume(&agent, &fresh_key, &legacy, 10).await.unwrap());
    let same_runtime = agent.clone();
    assert_eq!(same_runtime.ui_generation, agent.ui_generation);
    assert!(
        consume(&same_runtime, &fresh_key, &fresh, 10)
            .await
            .unwrap()
    );
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[tokio::test]
async fn isolated_guest_route_cannot_fall_back_to_a_native_host_provider() {
    let dir = tempfile::tempdir().unwrap();
    let device = Uuid::new_v4().to_string();
    let agent = Agent::new(crate::AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: device.clone(),
        device_token: crate::random(),
        state_dir: dir.path().join("state"),
        roots: vec![dir.path().to_owned()],
        allow_write: false,
        allow_exec: false,
        shell: "/bin/sh".into(),
    })
    .await
    .unwrap();
    let job = Job {
        id: Uuid::new_v4().to_string(),
        owner: "test".into(),
        device_id: device.clone(),
        tool: "ui_observe".into(),
        arguments: json!({"device":device}),
        ui_context: Some(
            serde_json::from_value(json!({"id":Uuid::new_v4(),"name":"isolated fixture",
            "host_device_id":Uuid::new_v4(),"execution_device_id":device,"isolation":"full_vm"}))
            .unwrap(),
        ),
    };
    let result = perform(&agent, &job).await.unwrap();
    assert_eq!(result["input_sent"], false);
    assert_eq!(result["host_fallback"], false);
    assert!(result.get("observation_id").is_none());
    assert!(agent.desktop_workers.current_capability().await.is_none());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[tokio::test]
#[ignore = "requires the owned running macOS VM, native parent and GUI LaunchAgent"]
async fn live_macos_vm_observe_click_unicode_and_save() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let workspace =
        std::env::var("OCTENDRA_LIVE_VM_WORKSPACE_ID").expect("explicit owned VM identity");
    Uuid::parse_str(&workspace).unwrap();
    let model = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.model"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&model.stdout).starts_with("VirtualMac"));
    let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(".octendra-acceptance/ui-live");
    std::fs::create_dir_all(&root).unwrap();
    let document = root.join("document.txt");
    if !document.exists() {
        std::fs::write(&document, "Owned Octendra UI acceptance\n").unwrap();
    }
    let device = Uuid::new_v4().to_string();
    let agent = Agent::new(crate::AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: device.clone(),
        device_token: crate::random(),
        state_dir: root.join("state"),
        roots: vec![root.clone()],
        allow_write: true,
        allow_exec: true,
        shell: "/bin/zsh".into(),
    })
    .await
    .unwrap();
    let context: crate::ui_workspace::ExecutionContext =
        serde_json::from_value(json!({"id":workspace,
        "name":"Owned native VM acceptance", "host_device_id":Uuid::new_v4(),
        "execution_device_id":device,"isolation":"full_vm"}))
        .unwrap();
    let job = |tool: &str, mut arguments: Value| {
        arguments["device"] = json!(device);
        Job {
            id: Uuid::new_v4().to_string(),
            device_id: device.clone(),
            owner: "native-vm-acceptance".into(),
            tool: tool.into(),
            arguments,
            ui_context: Some(context.clone()),
        }
    };
    let status = tokio::process::Command::new("/usr/bin/open")
        .args(["-a", "TextEdit"])
        .arg(&document)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let observed = agent.execute(&job("ui_observe", json!({}))).await.unwrap();
    assert!(
        observed.get("observation_id").is_some(),
        "{}",
        observed["error"]
    );
    std::fs::write(
        root.join("before.jpg"),
        STANDARD
            .decode(observed["image"]["data"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    println!(
        "native_vm_observation_ready {}x{}",
        observed["image"]["width"], observed["image"]["height"]
    );
    if std::env::var_os("OCTENDRA_LIVE_VM_OBSERVE_ONLY").is_some() {
        return;
    }
    let click = job(
        "ui_act",
        json!({"observation_id":observed["observation_id"],
        "action":{"type":"click","target":{"x":observed["image"]["width"].as_u64().unwrap()/2,
            "y":observed["image"]["height"].as_u64().unwrap()/2}}}),
    );
    let clicked = agent.execute(&click).await.unwrap();
    assert_eq!(clicked["state"], "accepted", "{}", clicked["error"]);
    // The same durable operation is observed, never sent to virtual hardware twice.
    let repeated = agent.execute(&click).await.unwrap();
    assert_eq!(repeated, clicked);
    let text = format!(
        "Octendra isolated Computer Use\n中文输入与保存已验证 ✅\n验收 {}\n",
        Uuid::new_v4()
    );
    let filled = agent
        .execute(&job(
            "ui_act",
            json!({"observation_id":clicked["observation"]["observation_id"],
        "action":{"type":"fill","text":text}}),
        ))
        .await
        .unwrap();
    if std::env::var_os("OCTENDRA_LIVE_VM_EXPECT_REMOTE_CLIPBOARD").is_some() {
        assert_eq!(filled["error"], "ui_gui_session_required");
        assert_eq!(filled["input_sent"], false);
        println!("remote_pasteboard_session_rejected_before_input");
        return;
    }
    assert_eq!(filled["state"], "accepted", "{}", filled["error"]);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let saved = agent
        .execute(&job(
            "ui_act",
            json!({"observation_id":filled["observation"]["observation_id"],
        "action":{"type":"key","key":"s","modifiers":["meta"]}}),
        ))
        .await
        .unwrap();
    assert_eq!(saved["state"], "accepted", "{}", saved["error"]);
    let mut content = String::new();
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        content = std::fs::read_to_string(&document).unwrap();
        if content == text {
            break;
        }
    }
    assert_eq!(
        content, text,
        "TextEdit must save actual Unicode text through native VM input"
    );
    let final_view = agent.execute(&job("ui_observe", json!({}))).await.unwrap();
    std::fs::write(
        root.join("after.jpg"),
        STANDARD
            .decode(final_view["image"]["data"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let receipt = json!({"read_click_unicode_save":true,"durable_retry_unchanged":true,
        "click_host_impact":clicked["host_impact"],"text_host_impact":filled["host_impact"],
        "save_host_impact":saved["host_impact"],"real_document":document,"source":final_view["source"]});
    std::fs::write(
        root.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    println!("{}", receipt);
}

#[cfg(unix)]
#[tokio::test]
async fn private_worker_is_closed_even_when_pixel_decoding_fails() {
    let dir = tempfile::tempdir().unwrap();
    let id = crate::computer_use::transport::tests::identity();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let mut agent = Agent::new(crate::AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: id.device_id.to_string(),
        device_token: crate::random(),
        state_dir: state.clone(),
        roots: vec![dir.path().to_owned()],
        allow_write: false,
        allow_exec: false,
        shell: "/bin/sh".into(),
    })
    .await
    .unwrap();
    agent.desktop_workers = crate::computer_use::transport::tests::fixture(
        &state,
        &dir.path().join("lock"),
        &id,
        "normal",
    );
    // Existing pipe fixture reports an intentionally unsupported YUV color space.
    assert!(
        run(&agent, Uuid::new_v4(), &id, 7, None, None)
            .await
            .is_err()
    );
    // A second Open proves the first worker and its native lock were released.
    let open = agent
        .desktop_workers
        .dispatch(
            Uuid::new_v4(),
            &id,
            7,
            desktop::Request::Open {
                identity: id.clone(),
                lease_ms: 1000,
            },
        )
        .await
        .unwrap();
    assert_eq!(open["state"], "opened");
    let binding = serde_json::from_value(open["binding"].clone()).unwrap();
    assert_eq!(
        agent
            .desktop_workers
            .dispatch(Uuid::new_v4(), &id, 7, desktop::Request::Close { binding })
            .await
            .unwrap()["state"],
        "closed"
    );
}
