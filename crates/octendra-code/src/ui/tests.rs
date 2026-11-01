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
