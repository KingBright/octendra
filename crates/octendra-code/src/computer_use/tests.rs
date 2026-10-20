use super::{
    mock::{Executor, Input},
    *,
};
use serde_json::json;

#[tokio::test]
async fn typed_gateway_ack_to_four_k_existing_chunk_artifact_and_close() {
    use crate::{
        DeviceRegistration, GatewayConfig, auth::Principal, delivery::Delivery, gateway::Gateway,
    };
    use std::sync::Arc;
    let id = identity("offline");
    let (_root, state, mut agent) = agent_fixture(id.device_id).await;
    agent.desktop_workers = super::transport::tests::fixture(
        state.path(),
        &state.path().join("desktop.lock"),
        &id,
        "four_k",
    )
    .with_store(agent.store.clone());
    let gateway_state = tempfile::tempdir().unwrap();
    let mut gateway = Gateway::new(GatewayConfig {
        public_url: "https://offline.invalid".into(),
        bind: "127.0.0.1:0".into(),
        state_dir: gateway_state.path().join("gateway"),
        forge_browser: None,
        owner: id.owner.clone(),
        password_hash: "test-unused".into(),
        devices: vec![DeviceRegistration {
            id: id.device_id.to_string(),
            name: "offline".into(),
            token_hash: crate::hash(&agent.config.device_token),
            scopes: vec!["desktop:input".into(), "desktop:observe".into()],
        }],
        redirect_uris: vec![],
        allowed_origins: vec![],
    })
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    Arc::make_mut(&mut gateway.config).public_url =
        format!("https://{}", listener.local_addr().unwrap());
    let router = gateway.router().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Arc::make_mut(&mut agent.config).gateway_url = endpoint.clone();
    let delivery = Delivery::new(agent.store.clone(), agent.config.clone())
        .await
        .unwrap();
    let sending = delivery.clone();
    let sender = tokio::spawn(async move {
        let _ = sending.run(&reqwest::Client::new()).await;
    });
    let seed = |job: crate::gateway::Job| {
        let store = gateway.store.clone();
        async move {
            sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,NULL,'dispatched',?)")
                .bind(&job.id).bind(&job.device_id).bind(&job.id).bind(crate::hash(serde_json::to_vec(&job).unwrap()))
                .bind(serde_json::to_string(&job).unwrap()).bind(crate::now()).execute(&store.pool).await.unwrap();
        }
    };
    let open = desktop_job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 30000,
        },
    );
    let mut public = open.arguments.clone();
    public["device_id"] = json!(id.device_id);
    public["idempotency_key"] = json!("offline-open");
    let terminal = Principal {
        owner: id.owner.clone(),
        scopes: vec!["terminal:exec".into()],
    };
    assert!(
        gateway
            .dispatch(&terminal, "desktop_worker", public.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("device_offline")
    );
    assert!(crate::tools::validate("desktop_worker", &public).is_ok());
    public["task_id"] = json!(Uuid::new_v4());
    assert!(crate::tools::validate("desktop_worker", &public).is_err());
    let opened = agent.execute(&open).await.unwrap();
    let binding: Binding = serde_json::from_value(opened["binding"].clone()).unwrap();
    let observe = desktop_job(
        &id,
        Request::Observe {
            binding: binding.clone(),
        },
    );
    seed(observe.clone()).await;
    let observed = agent
        .execute_with_delivery(&observe, Some(&delivery))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let saved: serde_json::Value = agent
                .store
                .get("local_operation", &observe.id)
                .await
                .unwrap()
                .unwrap();
            if saved["gateway_accepted"] == true {
                assert!(saved.get("result").is_none());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        observed["frame_artifact"]["bytes"], 12_441_600,
        "{observed}"
    );
    // The authorized export below retrieves the full indexed frame after ACK.
    // Avoid an extra full-frame hash/read within the same short execution lease.
    let export = crate::gateway::Job {
        id: Uuid::new_v4().to_string(),
        owner: id.owner.clone(),
        device_id: id.device_id.to_string(),
        tool: "desktop_frame".into(),
        arguments: json!({"task_id":id.task_id,"device_id":id.device_id,"capability_version":7,
            "observed_operation_id":observe.id,"idempotency_key":"offline-frame"}),
    };
    seed(export.clone()).await;
    let mut exported = agent.execute(&export).await.unwrap();
    eprintln!("four_k_export lease_ms=30000 timing={}", exported["timing"]);
    assert_eq!(exported["state"], "completed", "{exported}");
    assert_eq!(exported["size"], 12_441_600);
    assert_eq!(exported["checkpoint_bytes"], 4 * 1024 * 1024);
    crate::transfers::desktop_artifact(&gateway, &export)
        .await
        .expect("completed export must still have a live desktop artifact");
    crate::transfers::decorate(&gateway, &export, &mut exported)
        .await
        .unwrap();
    assert_eq!(exported["download_available"], true, "{exported}");
    let url = reqwest::Url::parse(exported["download_url"].as_str().unwrap()).unwrap();
    let download = format!("{endpoint}{}", url.path());
    let response = reqwest::get(&download).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.unwrap();
    assert_eq!(bytes.len(), 12_441_600);
    let mut foreign = export.clone();
    foreign.owner = "another-owner".into();
    assert!(
        crate::transfers::desktop_artifact(&gateway, &foreign)
            .await
            .is_err()
    );
    let mut wrong_task = export.clone();
    wrong_task.arguments["task_id"] = json!(Uuid::new_v4());
    assert!(
        crate::transfers::desktop_artifact(&gateway, &wrong_task)
            .await
            .is_err()
    );
    let mut expired = observed.clone();
    expired["frame_artifact"]["expires_at_ms"] = json!(0);
    sqlx::query("UPDATE jobs SET result=? WHERE id=?")
        .bind(expired.to_string())
        .bind(&observe.id)
        .execute(&gateway.store.pool)
        .await
        .unwrap();
    assert_eq!(
        reqwest::get(&download).await.unwrap().status(),
        reqwest::StatusCode::GONE
    );
    sqlx::query("UPDATE jobs SET result=? WHERE id=?")
        .bind(observed.to_string())
        .bind(&observe.id)
        .execute(&gateway.store.pool)
        .await
        .unwrap();
    let close = desktop_job(&id, Request::Close { binding });
    seed(close.clone()).await;
    assert_eq!(
        agent
            .execute_with_delivery(&close, Some(&delivery))
            .await
            .unwrap()["state"],
        "closed"
    );
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while gateway
            .store
            .get::<serde_json::Value>("file_blob", &export.id)
            .await
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!crate::transfers::blob_path(&gateway, &export.id).exists());
    assert!(agent.read_desktop_frame(&observe).await.is_err());
    // A removed link is 404; retained link metadata with retired bytes is 410.
    // Both deny the closed session's artifact; byte removal is checked above.
    assert!(matches!(
        reqwest::get(&download).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    ));
    assert!(
        !agent
            .config
            .state_dir
            .join("transfers-v2")
            .join(format!("{}.data", export.id))
            .exists()
    );
    assert_eq!(crate::hash(&bytes), exported["sha256"]);
    sender.abort();
    server.abort();
}

#[tokio::test]
async fn desktop_export_one_429_or_disconnect_delivers_terminal_and_retires_frame() {
    use crate::{DeviceRegistration, GatewayConfig, delivery::Delivery, gateway::Gateway};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for fault in ["429", "disconnect"] {
        let id = identity("offline");
        let (_root, state, mut agent) = agent_fixture(id.device_id).await;
        agent.desktop_workers = super::transport::tests::fixture(
            state.path(),
            &state.path().join("desktop.lock"),
            &id,
            "normal",
        )
        .with_store(agent.store.clone());
        let gateway_state = tempfile::tempdir().unwrap();
        let mut gateway = Gateway::new(GatewayConfig {
            public_url: "https://offline.invalid".into(),
            bind: "127.0.0.1:0".into(),
            state_dir: gateway_state.path().join("gateway"),
            forge_browser: None,
            owner: id.owner.clone(),
            password_hash: "test-unused".into(),
            devices: vec![DeviceRegistration {
                id: id.device_id.to_string(),
                name: "offline".into(),
                token_hash: crate::hash(&agent.config.device_token),
                scopes: vec!["desktop:input".into(), "desktop:observe".into()],
            }],
            redirect_uris: vec![],
            allowed_origins: vec![],
        })
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        Arc::make_mut(&mut gateway.config).public_url =
            format!("https://{}", listener.local_addr().unwrap());
        let export_id = Uuid::new_v4().to_string();
        let intercept = format!("/device/transfers/{export_id}");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = gateway.router().unwrap().layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let counter = counter.clone();
                let intercept = intercept.clone();
                async move {
                    if request.uri().path() == intercept
                        && request.method() == reqwest::Method::POST
                    {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if fault == "429" {
                            return axum::response::Response::builder()
                                .status(429)
                                .body(axum::body::Body::empty())
                                .unwrap();
                        }
                        let broken = futures_util::stream::once(async {
                            Err::<Vec<u8>, std::io::Error>(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "injected disconnect",
                            ))
                        });
                        return axum::response::Response::builder()
                            .status(200)
                            .body(axum::body::Body::from_stream(broken))
                            .unwrap();
                    }
                    next.run(request).await
                }
            },
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Arc::make_mut(&mut agent.config).gateway_url = endpoint;
        let delivery = Delivery::new(agent.store.clone(), agent.config.clone())
            .await
            .unwrap();
        let sending = delivery.clone();
        let sender = tokio::spawn(async move {
            let _ = sending.run(&reqwest::Client::new()).await;
        });
        let seed = |job: crate::gateway::Job| {
            let store = gateway.store.clone();
            async move {
                sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,NULL,'dispatched',?)")
                .bind(&job.id).bind(&job.device_id).bind(&job.id).bind(crate::hash(serde_json::to_vec(&job).unwrap()))
                .bind(serde_json::to_string(&job).unwrap()).bind(crate::now()).execute(&store.pool).await.unwrap();
            }
        };
        let opened = agent
            .execute(&desktop_job(
                &id,
                Request::Open {
                    identity: id.clone(),
                    lease_ms: 30000,
                },
            ))
            .await
            .unwrap();
        let binding: Binding = serde_json::from_value(opened["binding"].clone()).unwrap();
        let observe = desktop_job(
            &id,
            Request::Observe {
                binding: binding.clone(),
            },
        );
        seed(observe.clone()).await;
        let observed = agent
            .execute_with_delivery(&observe, Some(&delivery))
            .await
            .unwrap();
        assert_eq!(observed["frame_artifact"]["bytes"], 6, "{observed}");
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let saved: serde_json::Value = agent
                    .store
                    .get("local_operation", &observe.id)
                    .await
                    .unwrap()
                    .unwrap();
                if saved["gateway_accepted"] == true {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let export = crate::gateway::Job {
            id: export_id,
            owner: id.owner.clone(),
            device_id: id.device_id.to_string(),
            tool: "desktop_frame".into(),
            arguments: json!({"task_id":id.task_id,"device_id":id.device_id,"capability_version":7,
                "observed_operation_id":observe.id,"idempotency_key":"offline-failed-frame"}),
        };
        seed(export.clone()).await;
        let failed = agent
            .execute_with_delivery(&export, Some(&delivery))
            .await
            .unwrap();
        assert_eq!(failed["state"], "failed", "{failed}");
        assert_eq!(
            failed["error_code"], "desktop_export_interrupted",
            "{failed}"
        );
        assert_eq!(failed["resumable"], false);
        assert_eq!(failed["outcome"], "unknown");
        assert_eq!(failed["cleanup_complete"], true);
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let saved: serde_json::Value = agent
                    .store
                    .get("local_operation", &export.id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(saved["resumable"], false);
                if saved["gateway_accepted"] == true {
                    assert!(saved.get("result").is_none());
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (saved_state, saved_result): (String, String) =
            sqlx::query_as("SELECT state,result FROM jobs WHERE id=?")
                .bind(&export.id)
                .fetch_one(&gateway.store.pool)
                .await
                .unwrap();
        assert_eq!(saved_state, "done");
        let saved_result: serde_json::Value = serde_json::from_str(&saved_result).unwrap();
        assert_eq!(saved_result["error_code"], "desktop_export_interrupted");
        assert_eq!(saved_result["resumable"], false);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(agent.read_desktop_frame(&observe).await.is_err());
        assert!(
            crate::transfers::desktop_artifact(&gateway, &export)
                .await
                .is_err()
        );
        assert!(
            !agent
                .config
                .state_dir
                .join("transfers-v2")
                .join(format!("{}.data", export.id))
                .exists()
        );
        let repeated = agent
            .execute_with_delivery(&export, Some(&delivery))
            .await
            .unwrap();
        assert_eq!(repeated["state"], "already_completed");
        let mut other = export.clone();
        other.id = Uuid::new_v4().to_string();
        let denied = agent.execute(&other).await.unwrap();
        assert!(denied.get("error").is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            agent
                .execute(&desktop_job(&id, Request::Close { binding }))
                .await
                .unwrap()["state"],
            "closed"
        );
        sender.abort();
        server.abort();
    }
}

fn mapping() -> FrameMapping {
    FrameMapping {
        frame_size: [120, 80],
        content: ContentRect {
            left: 10,
            top: 10,
            width: 100,
            height: 60,
        },
        logical_size: [201, 101],
        desktop_position: [-1920, 500],
        rotation: Rotation::R0,
    }
}
fn identity(desktop: &str) -> Identity {
    Identity {
        owner: "owner".into(),
        task_id: Uuid::new_v4(),
        device_id: Uuid::new_v4(),
        desktop_id: desktop.into(),
    }
}
fn open(executor: &Executor, identity: &Identity) -> Binding {
    match executor
        .run(Request::Open {
            identity: identity.clone(),
            lease_ms: 1000,
        })
        .unwrap()
    {
        Response::Opened { binding, .. } => binding,
        _ => panic!("open response"),
    }
}
fn observe(executor: &Executor, binding: &Binding) -> FrameRef {
    match executor
        .run(Request::Observe {
            binding: binding.clone(),
        })
        .unwrap()
    {
        Response::Observed { frame } => frame,
        _ => panic!("observe response"),
    }
}
fn act(
    executor: &Executor,
    frame: &FrameRef,
    sequence: u64,
    actions: Vec<Action>,
) -> Result<Response, ErrorCode> {
    executor.run(Request::Act {
        frame: frame.clone(),
        sequence,
        actions,
    })
}
fn holds() -> Vec<Action> {
    vec![
        Action::KeyDown { usage: 4 },
        Action::ButtonDown {
            button: Button::Left,
        },
    ]
}

#[test]
fn production_default_and_catalog_remain_unavailable() {
    assert!(!Capabilities::default().available);
    assert!(matches!(
        unavailable(&Request::Capabilities {}),
        Ok(Response::Capabilities { .. })
    ));
    assert_eq!(
        unavailable(&Request::Open {
            identity: identity("d"),
            lease_ms: 1000
        }),
        Err(ErrorCode::Unavailable)
    );
    assert_eq!(crate::SCOPES, crate::SUPPORTED_SCOPES);
    for tool in [
        "computer_use_mock",
        "computer_use_open",
        "computer_use_act",
        "computer_use_observe",
    ] {
        assert!(crate::tools::scope(tool).is_none());
    }
}
#[test]
fn strict_wire_contract_rejects_unknown_and_wrong_types() {
    assert!(
        serde_json::from_value::<Request>(json!({"operation":"capabilities","capture":true}))
            .is_err()
    );
    assert!(serde_json::from_value::<Response>(json!({"state":"closed","reopen":true})).is_err());
    assert!(serde_json::from_value::<Action>(json!({"kind":"key_down","usage":"4"})).is_err());
    let mut value = serde_json::to_value(mapping()).unwrap();
    value["fallback_display"] = json!(true);
    assert!(serde_json::from_value::<FrameMapping>(value).is_err());
    let (executor, _) = Executor::new(mapping());
    let id = identity("d");
    let binding = open(&executor, &id);
    let frame = observe(&executor, &binding);
    for request in [
        Request::Capabilities {},
        Request::Open {
            identity: id,
            lease_ms: 1000,
        },
        Request::Observe {
            binding: binding.clone(),
        },
        Request::Act {
            frame,
            sequence: 1,
            actions: holds(),
        },
        Request::Close { binding },
    ] {
        assert_eq!(
            request,
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap()
        );
    }
}
#[test]
fn coordinates_respect_padding_rotation_and_source_local_position() {
    let mut m = mapping();
    assert_eq!(m.map(59.5, 39.5), Ok(SourcePoint { x: 100.0, y: 50.0 }));
    assert_eq!(m.map(109.0, 69.0), Ok(SourcePoint { x: 200.0, y: 100.0 }));
    for (rotation, expected) in [
        (Rotation::R0, SourcePoint { x: 0.0, y: 0.0 }),
        (Rotation::R90, SourcePoint { x: 0.0, y: 100.0 }),
        (Rotation::R180, SourcePoint { x: 200.0, y: 100.0 }),
        (Rotation::R270, SourcePoint { x: 200.0, y: 0.0 }),
    ] {
        m.rotation = rotation;
        assert_eq!(m.map(10.0, 10.0), Ok(expected));
    }
    for point in [
        (9.0, 10.0),
        (110.0, 10.0),
        (10.0, 70.0),
        (f64::NAN, 10.0),
        (10.0, f64::INFINITY),
    ] {
        assert_eq!(m.map(point.0, point.1), Err(ErrorCode::InvalidCoordinates));
    }
    m.content.width = u32::MAX;
    assert_eq!(m.map(10.0, 10.0), Err(ErrorCode::InvalidCoordinates));
    m = mapping();
    m.logical_size[0] = 0;
    assert_eq!(m.map(10.0, 10.0), Err(ErrorCode::InvalidCoordinates));
}
#[test]
fn coordinate_endpoints_preserve_one_to_one_pixels_and_single_pixel_center() {
    let mut m = FrameMapping {
        frame_size: [100, 60],
        content: ContentRect {
            left: 0,
            top: 0,
            width: 100,
            height: 60,
        },
        logical_size: [100, 60],
        desktop_position: [-1920, 500],
        rotation: Rotation::R0,
    };
    for (x, y) in [(0.0, 0.0), (33.25, 12.5), (99.0, 59.0)] {
        let point = m.map(x, y).unwrap();
        assert!((point.x - x).abs() < 1e-10);
        assert!((point.y - y).abs() < 1e-10);
    }
    assert_eq!(m.map(99.5, 59.0), Err(ErrorCode::InvalidCoordinates));
    m.frame_size = [1, 1];
    m.content.width = 1;
    m.content.height = 1;
    assert_eq!(m.map(0.0, 0.0), Ok(SourcePoint { x: 49.5, y: 29.5 }));
}
#[test]
fn every_identity_field_and_epoch_is_bound() {
    let (executor, _) = Executor::new(mapping());
    let binding = open(&executor, &identity("d"));
    let mut variants = Vec::new();
    let mut bad = binding.clone();
    bad.epoch = Uuid::new_v4();
    variants.push(bad);
    let mut bad = binding.clone();
    bad.identity.owner = "another".into();
    variants.push(bad);
    let mut bad = binding.clone();
    bad.identity.task_id = Uuid::new_v4();
    variants.push(bad);
    let mut bad = binding.clone();
    bad.identity.device_id = Uuid::new_v4();
    variants.push(bad);
    let mut bad = binding.clone();
    bad.identity.desktop_id = "another".into();
    variants.push(bad);
    for bad in variants {
        assert!(
            executor
                .run(Request::Observe {
                    binding: bad.clone()
                })
                .is_err()
        );
        assert!(executor.run(Request::Close { binding: bad }).is_err());
    }
    assert!(executor.run(Request::Observe { binding }).is_ok());
}
#[test]
fn desktop_lease_is_exclusive_across_clones_and_concurrent_open() {
    let (executor, _) = Executor::new(mapping());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let executor = executor.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                executor.run(Request::Open {
                    identity: identity("d"),
                    lease_ms: 1000,
                })
            })
        })
        .collect();
    barrier.wait();
    let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(outcomes.iter().filter(|v| v.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|v| **v == Err(ErrorCode::DesktopBusy))
            .count(),
        1
    );
    assert!(
        executor
            .run(Request::Open {
                identity: identity("other-desktop"),
                lease_ms: 1000
            })
            .is_ok()
    );
}
#[test]
fn stale_frame_revision_and_forged_geometry_never_inject() {
    let (executor, backend) = Executor::new(mapping());
    let binding = open(&executor, &identity("d"));
    let first = observe(&executor, &binding);
    let current = observe(&executor, &binding);
    assert_eq!(
        act(&executor, &first, 1, holds()),
        Err(ErrorCode::StaleFrame)
    );
    let mut forged = current.clone();
    forged.source_revision += 1;
    assert_eq!(
        act(&executor, &forged, 1, holds()),
        Err(ErrorCode::StaleFrame)
    );
    let mut forged = current.clone();
    forged.mapping.desktop_position[0] += 1;
    assert_eq!(
        act(&executor, &forged, 1, holds()),
        Err(ErrorCode::StaleFrame)
    );
    executor.change_source(mapping());
    assert_eq!(
        act(&executor, &current, 1, holds()),
        Err(ErrorCode::SessionClosed)
    );
    let replacement = open(&executor, &binding.identity);
    assert_ne!(replacement.epoch, binding.epoch);
    assert_eq!(observe(&executor, &replacement).source_revision, 2);
    assert!(backend.lock().unwrap().events.is_empty());
}
#[test]
fn short_batches_validate_whole_order_before_side_effects() {
    let (executor, backend) = Executor::new(mapping());
    let binding = open(&executor, &identity("d"));
    let frame = observe(&executor, &binding);
    assert_eq!(
        act(&executor, &frame, 2, holds()),
        Err(ErrorCode::InvalidSequence)
    );
    for actions in [
        vec![],
        vec![Action::Wheel {
            horizontal: i16::MIN,
            vertical: 0,
        }],
        vec![Action::KeyDown { usage: 4 }; MAX_ACTIONS + 1],
        vec![Action::KeyDown { usage: 4 }, Action::KeyUp { usage: 5 }],
        vec![
            Action::ButtonDown {
                button: Button::Left,
            },
            Action::PointerMove { x: 0.0, y: 0.0 },
        ],
    ] {
        assert!(act(&executor, &frame, 1, actions).is_err());
        assert!(backend.lock().unwrap().events.is_empty());
    }
    let actions = vec![
        Action::PointerMove { x: 59.5, y: 39.5 },
        Action::KeyDown { usage: 4 },
        Action::KeyUp { usage: 4 },
        Action::ButtonDown {
            button: Button::Left,
        },
        Action::ButtonUp {
            button: Button::Left,
        },
    ];
    assert_eq!(
        act(&executor, &frame, 1, actions),
        Ok(Response::Accepted {
            sequence: 1,
            accepted_actions: 5,
            effect_confirmed: false,
        })
    );
    let inputs: Vec<_> = backend
        .lock()
        .unwrap()
        .events
        .iter()
        .map(|(_, e)| e.clone())
        .collect();
    assert_eq!(
        inputs,
        vec![
            Input::Pointer(SourcePoint { x: 100.0, y: 50.0 }),
            Input::KeyDown(4),
            Input::KeyUp(4),
            Input::ButtonDown(Button::Left),
            Input::ButtonUp(Button::Left),
        ]
    );
    assert_eq!(
        act(&executor, &frame, 1, holds()),
        Err(ErrorCode::StaleFrame)
    );
    let frame = observe(&executor, &binding);
    assert_eq!(
        act(&executor, &frame, 1, holds()),
        Err(ErrorCode::InvalidSequence)
    );
    assert!(act(&executor, &frame, 2, holds()).is_ok());
}
#[test]
fn close_timeout_disconnect_source_change_and_drop_release_owned_holds() {
    for cause in 0..5 {
        let (executor, backend) = Executor::new(mapping());
        let id = identity("d");
        let binding = open(&executor, &id);
        let frame = observe(&executor, &binding);
        act(&executor, &frame, 1, holds()).unwrap();
        match cause {
            0 => {
                executor
                    .run(Request::Close {
                        binding: binding.clone(),
                    })
                    .unwrap();
            }
            1 => executor.advance(1000),
            2 => executor.disconnect(&id),
            3 => executor.change_source(mapping()),
            _ => {
                drop(executor);
            }
        }
        let events = &backend.lock().unwrap().events;
        assert_eq!(
            events,
            &vec![
                (binding.epoch, Input::KeyDown(4)),
                (binding.epoch, Input::ButtonDown(Button::Left)),
                (binding.epoch, Input::KeyUp(4)),
                (binding.epoch, Input::ButtonUp(Button::Left)),
            ]
        );
    }
}
#[test]
fn disconnect_cannot_release_another_identity_or_desktop() {
    let (executor, backend) = Executor::new(mapping());
    let a = identity("a");
    let b = identity("b");
    let ba = open(&executor, &a);
    let bb = open(&executor, &b);
    act(&executor, &observe(&executor, &ba), 1, holds()).unwrap();
    act(&executor, &observe(&executor, &bb), 1, holds()).unwrap();
    let mut stranger = a.clone();
    stranger.task_id = Uuid::new_v4();
    executor.disconnect(&stranger);
    assert_eq!(backend.lock().unwrap().events.len(), 4);
    executor.disconnect(&a);
    assert_eq!(backend.lock().unwrap().events.len(), 6);
    assert!(
        executor
            .run(Request::Observe {
                binding: bb.clone()
            })
            .is_ok()
    );
    assert!(
        backend
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|(_, e)| matches!(e, Input::KeyUp(_) | Input::ButtonUp(_)))
            .all(|(epoch, _)| *epoch == ba.epoch)
    );
}
#[test]
fn uncertain_press_releases_conservative_hold_and_closes_epoch() {
    let (executor, backend) = Executor::new(mapping());
    let binding = open(&executor, &identity("d"));
    let frame = observe(&executor, &binding);
    backend.lock().unwrap().fail_at = Some(1);
    assert_eq!(
        act(&executor, &frame, 1, holds()),
        Err(ErrorCode::OutcomeUnknown)
    );
    assert_eq!(
        act(&executor, &frame, 1, holds()),
        Err(ErrorCode::SessionClosed)
    );
    assert_eq!(
        backend.lock().unwrap().events,
        vec![
            (binding.epoch, Input::KeyDown(4)),
            (binding.epoch, Input::KeyUp(4)),
        ]
    );
}
#[test]
fn lease_bounds_reject_invalid_open_and_expired_epoch() {
    let (executor, _) = Executor::new(mapping());
    let id = identity("d");
    for lease_ms in [0, 99, MAX_LEASE_MS + 1] {
        assert_eq!(
            executor.run(Request::Open {
                identity: id.clone(),
                lease_ms
            }),
            Err(ErrorCode::InvalidRequest)
        );
    }
    let binding = open(&executor, &id);
    executor.advance(1000);
    assert_eq!(
        executor.run(Request::Observe { binding }),
        Err(ErrorCode::SessionClosed)
    );
    assert!(
        executor
            .run(Request::Open {
                identity: id,
                lease_ms: 1000
            })
            .is_ok()
    );
}
async fn agent_fixture(
    device: Uuid,
) -> (tempfile::TempDir, tempfile::TempDir, crate::agent::Agent) {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let agent = crate::agent::Agent::new(crate::AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: device.to_string(),
        device_token: crate::random(),
        state_dir: state.path().into(),
        roots: vec![root.path().into()],
        allow_write: false,
        allow_exec: false,
        shell: "/bin/sh".into(),
    })
    .await
    .unwrap();
    (root, state, agent)
}
fn job(id: &Identity, request: Request) -> crate::gateway::Job {
    crate::gateway::Job {
        id: Uuid::new_v4().to_string(),
        device_id: id.device_id.to_string(),
        owner: id.owner.clone(),
        tool: "computer_use_mock".into(),
        arguments: json!({"task_id":id.task_id,"request":request,"idempotency_key":"fixture"}),
    }
}
#[tokio::test]
async fn original_agent_journal_returns_saved_result_without_input_replay() {
    let (executor, backend) = Executor::new(mapping());
    let id = identity("d");
    let (_root, _state, agent) = agent_fixture(id.device_id).await;
    let open_job = job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 1000,
        },
    );
    let opened = executor.execute(&agent, &open_job).await.unwrap();
    assert_eq!(opened, executor.execute(&agent, &open_job).await.unwrap());
    let binding: Binding = serde_json::from_value(opened["binding"].clone()).unwrap();
    let frame = observe(&executor, &binding);
    let act_job = job(
        &id,
        Request::Act {
            frame,
            sequence: 1,
            actions: holds(),
        },
    );
    let accepted = executor.execute(&agent, &act_job).await.unwrap();
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(accepted, executor.execute(&agent, &act_job).await.unwrap());
    assert_eq!(backend.lock().unwrap().events.len(), 2);
    let mut conflict = act_job;
    conflict.arguments["idempotency_key"] = json!("changed");
    assert!(executor.execute(&agent, &conflict).await.is_err());
}
#[tokio::test]
async fn unknown_result_and_interrupted_journal_never_repeat() {
    let (executor, backend) = Executor::new(mapping());
    let id = identity("d");
    let (_root, _state, agent) = agent_fixture(id.device_id).await;
    let binding = open(&executor, &id);
    let act_job = job(
        &id,
        Request::Act {
            frame: observe(&executor, &binding),
            sequence: 1,
            actions: holds(),
        },
    );
    backend.lock().unwrap().fail_at = Some(1);
    let unknown = executor.execute(&agent, &act_job).await.unwrap();
    assert_eq!(unknown["error"], "outcome_unknown");
    assert_eq!(unknown, executor.execute(&agent, &act_job).await.unwrap());
    assert_eq!(backend.lock().unwrap().events.len(), 2);
    let interrupted = job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 1000,
        },
    );
    agent
        .store
        .put(
            "local_operation",
            &interrupted.id,
            &json!({
                "fingerprint":crate::hash(serde_json::to_vec(&interrupted).unwrap()),
                "state":"running","result":null,"resumable":false,
            }),
            i64::MAX,
        )
        .await
        .unwrap();
    let config = (*agent.config).clone();
    drop(agent);
    let restarted = crate::agent::Agent::new(config).await.unwrap();
    assert_eq!(
        executor.execute(&restarted, &interrupted).await.unwrap()["error"],
        "outcome_unknown"
    );
    assert_eq!(backend.lock().unwrap().events.len(), 2);
}
#[tokio::test]
async fn mock_receipt_uses_existing_durable_delivery_outbox() {
    let (executor, _) = Executor::new(mapping());
    let id = identity("d");
    let (_root, _state, agent) = agent_fixture(id.device_id).await;
    let delivery = crate::delivery::Delivery::new(agent.store.clone(), agent.config.clone())
        .await
        .unwrap();
    let original = job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 1000,
        },
    );
    let result = executor
        .execute_with_delivery(&agent, &original, Some(&delivery))
        .await
        .unwrap();
    assert_eq!(
        result,
        executor
            .execute_with_delivery(&agent, &original, Some(&delivery))
            .await
            .unwrap()
    );
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM receipt_outbox")
        .fetch_one(&agent.store.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let saved: serde_json::Value = agent
        .store
        .get("local_operation", &original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["result"], result);
}
#[tokio::test]
async fn job_identity_mismatch_and_production_dispatch_cannot_reach_mock() {
    let (executor, backend) = Executor::new(mapping());
    let id = identity("d");
    let (_root, _state, agent) = agent_fixture(id.device_id).await;
    let mut wrong = job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 1000,
        },
    );
    wrong.owner = "stranger".into();
    assert!(
        executor
            .execute(&agent, &wrong)
            .await
            .unwrap()
            .get("error")
            .is_some()
    );
    let legitimate = job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 1000,
        },
    );
    assert!(
        agent
            .execute(&legitimate)
            .await
            .unwrap()
            .get("error")
            .is_some()
    );
    assert!(backend.lock().unwrap().events.is_empty());
    assert!(
        executor
            .run(Request::Open {
                identity: id,
                lease_ms: 1000
            })
            .is_ok()
    );
}

fn desktop_job(id: &Identity, request: Request) -> crate::gateway::Job {
    let mut job = job(id, request);
    job.tool = "desktop_worker".into();
    job.arguments
        .as_object_mut()
        .unwrap()
        .remove("idempotency_key");
    job.arguments["capability_version"] = json!(7);
    job.arguments["device_id"] = json!(id.device_id);
    job.arguments["idempotency_key"] = json!(job.id);
    job
}
#[tokio::test]
async fn private_transport_through_agent_journal_delivers_real_bytes_and_never_replays() {
    let id = identity("offline");
    let (_root, state, mut agent) = agent_fixture(id.device_id).await;
    agent.desktop_workers = super::transport::tests::fixture(
        state.path(),
        &state.path().join("desktop.lock"),
        &id,
        "normal",
    );
    let open = desktop_job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 30000,
        },
    );
    let opened = agent.execute(&open).await.unwrap();
    assert_eq!(opened, agent.execute(&open).await.unwrap());
    let binding: Binding = serde_json::from_value(opened["binding"].clone()).unwrap();
    let observation = desktop_job(
        &id,
        Request::Observe {
            binding: binding.clone(),
        },
    );
    let result = agent.execute(&observation).await.unwrap();
    assert_eq!(result, agent.execute(&observation).await.unwrap());
    let pixels = agent.read_desktop_frame(&observation).await.unwrap();
    assert_eq!(pixels, b"abcdef");
    assert_eq!(result["frame_artifact"]["sha256"], crate::hash(&pixels));
    let frame: FrameRef = serde_json::from_value(result["frame"].clone()).unwrap();
    let action = desktop_job(
        &id,
        Request::Act {
            frame,
            sequence: 1,
            actions: vec![Action::KeyDown { usage: 4 }],
        },
    );
    let accepted = agent.execute(&action).await.unwrap();
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(accepted, agent.execute(&action).await.unwrap());
    assert!(!agent.config.allow_exec);
    let mut forged = observation.clone();
    forged.owner = "stranger".into();
    assert!(agent.read_desktop_frame(&forged).await.is_err());
    assert!(agent.revoke_desktop(&binding).await);
    assert!(agent.read_desktop_frame(&observation).await.is_err());
    assert_eq!(result, agent.execute(&observation).await.unwrap());
}
#[tokio::test]
async fn private_transport_unknown_journal_and_default_gate_do_not_replay() {
    let id = identity("offline");
    let (_root, state, mut agent) = agent_fixture(id.device_id).await;
    let disabled = desktop_job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 1000,
        },
    );
    assert_eq!(
        agent.execute(&disabled).await.unwrap()["error"],
        "unavailable"
    );
    assert_eq!(crate::SCOPES, crate::SUPPORTED_SCOPES);
    assert_eq!(crate::tools::scope("desktop_worker"), Some("desktop:input"));
    agent.desktop_workers = super::transport::tests::fixture(
        state.path(),
        &state.path().join("desktop.lock"),
        &id,
        "act_eof",
    );
    let open = desktop_job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 30000,
        },
    );
    let opened = agent.execute(&open).await.unwrap();
    let binding: Binding = serde_json::from_value(opened["binding"].clone()).unwrap();
    let observe = desktop_job(&id, Request::Observe { binding });
    let observed = agent.execute(&observe).await.unwrap();
    let frame: FrameRef = serde_json::from_value(observed["frame"].clone()).unwrap();
    let action = desktop_job(
        &id,
        Request::Act {
            frame,
            sequence: 1,
            actions: vec![Action::KeyDown { usage: 4 }],
        },
    );
    let unknown = agent.execute(&action).await.unwrap();
    assert_eq!(unknown["error"], "outcome_unknown");
    assert_eq!(unknown, agent.execute(&action).await.unwrap());
    let saved: serde_json::Value = agent
        .store
        .get("local_operation", &action.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["result"], unknown);
    let reopen = desktop_job(
        &id,
        Request::Open {
            identity: id.clone(),
            lease_ms: 1000,
        },
    );
    assert_eq!(
        agent.execute(&reopen).await.unwrap()["error"],
        "outcome_unknown"
    );
}
