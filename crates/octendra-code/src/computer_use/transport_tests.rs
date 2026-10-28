use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn local_observe_only_rejects_input_before_spawning_a_worker() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let workers = fixture(
        &dir.path().join("state"),
        &dir.path().join("lock"),
        &id,
        "normal",
    );
    workers.state.lock().await.approval.as_mut().unwrap().input = false;
    let frame = FrameRef {
        binding: Binding {
            identity: id.clone(),
            epoch: Uuid::new_v4(),
        },
        source_revision: 1,
        frame_sequence: 1,
        mapping: FrameMapping {
            frame_size: [2, 2],
            content: ContentRect {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
            },
            logical_size: [2, 2],
            desktop_position: [0, 0],
            rotation: Rotation::R0,
        },
    };
    let result = workers
        .dispatch(
            Uuid::new_v4(),
            &id,
            7,
            Request::Act {
                frame,
                sequence: 1,
                actions: vec![Action::PointerMove { x: 0., y: 0. }],
            },
        )
        .await
        .unwrap();
    assert_eq!(result["reason"], "linux_portal_window_observe_only");
    let state = workers.state.lock().await;
    assert!(state.pipe.is_none());
    assert!(state.binding.is_none());
    assert_eq!(state.sequence, 0);
}

#[tokio::test]
async fn linux_window_capability_advertises_no_actions_and_never_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let (workers, _id, _) = capability_fixture(dir.path()).await;
    {
        let mut state = workers.state.lock().await;
        let approval = state.approval.as_mut().unwrap();
        approval.input = false;
        approval.logical_size = [0, 0];
        approval.logical_position = [0, 0];
    }
    let caps = workers
        .capabilities_with_probe("linux_portal_window_observe_only", |_| true)
        .await;
    assert!(caps.available);
    assert_eq!(caps.backend, "linux_portal_window_observe_only");
    assert_eq!(caps.max_actions, 0);
    assert!(workers.state.lock().await.pipe.is_none());
}

pub(crate) fn observer_fixture(path: &Path, lock: &Path, id: &Identity) -> NativeWorkers {
    let workers = fixture(path, lock, id, "normal");
    {
        let mut state = workers.state.try_lock().unwrap();
        let approval = state.approval.as_mut().unwrap();
        approval.input = false;
        approval.logical_size = [0, 0];
        approval.logical_position = [0, 0];
    }
    workers
}

#[tokio::test]
async fn observer_never_uses_an_input_capable_or_desktop_geometry_launch() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let workers = fixture(
        &dir.path().join("state"),
        &dir.path().join("lock"),
        &id,
        "normal",
    );
    let result = workers
        .dispatch_observer(
            Uuid::new_v4(),
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 1000,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["reason"], "linux_readonly_window_launch_required");
    assert!(workers.state.lock().await.pipe.is_none());
    {
        workers.state.lock().await.approval.as_mut().unwrap().input = false;
    }
    let result = workers
        .dispatch_observer(
            Uuid::new_v4(),
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 1000,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["reason"], "linux_readonly_window_launch_required");
    assert!(workers.state.lock().await.pipe.is_none());
}

#[tokio::test]
async fn observer_open_observe_close_keeps_the_existing_frame_and_epoch_guards() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let workers = observer_fixture(&dir.path().join("state"), &dir.path().join("lock"), &id);
    let open = workers
        .dispatch_observer(
            Uuid::new_v4(),
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 30000,
            },
        )
        .await
        .unwrap();
    let binding: Binding = serde_json::from_value(open["binding"].clone()).unwrap();
    let operation = Uuid::new_v4();
    let observed = workers
        .dispatch_observer(
            operation,
            &id,
            7,
            Request::Observe {
                binding: binding.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(observed["state"], "observed");
    let frame: FrameRef = serde_json::from_value(observed["frame"].clone()).unwrap();
    let before = workers.state.lock().await.sequence;
    let result = workers
        .dispatch_observer(
            Uuid::new_v4(),
            &id,
            7,
            Request::Act {
                frame,
                sequence: 1,
                actions: vec![Action::PointerMove { x: 0., y: 0. }],
            },
        )
        .await
        .unwrap();
    assert_eq!(result["reason"], "observer_never_accepts_act");
    assert_eq!(workers.state.lock().await.sequence, before);
    let artifact = workers.artifact(operation).await.unwrap();
    assert_eq!(
        workers.read_frame(&artifact, &id, 7).await.unwrap(),
        b"abcdef"
    );
    workers
        .dispatch_observer(Uuid::new_v4(), &id, 7, Request::Close { binding })
        .await
        .unwrap();
    assert!(workers.read_frame(&artifact, &id, 7).await.is_err());
}

#[test]
fn weavelo_packed_rgba_declares_color_by_encoding_without_fabricating_spa_yuv() {
    let mut pixels = RawPixels {
        encoding: "rgba8_srgb".into(),
        size: [3, 1],
        color: [0; 4],
        plane_lengths: vec![12],
        plane_sha256: vec![digest(&[17; 12])],
    };
    assert!(validate_pixels(&pixels).is_ok());
    pixels.color = [1; 4];
    assert!(validate_pixels(&pixels).is_err());
    pixels.color = [0; 4];
    pixels.plane_lengths = vec![13];
    assert!(validate_pixels(&pixels).is_err());
    pixels.size = [8192, 8192];
    pixels.plane_lengths = vec![8192 * 8192 * 4];
    assert!(validate_pixels(&pixels).is_err());
}
#[tokio::test]
async fn weavelo_observe_authority_never_spawns_or_dispatches_input() {
    let dir = tempfile::tempdir().unwrap();
    let route = super::super::weavelo_worker::Route {
        input_platform: None,
        profile: dir.path().to_owned(),
        generation: "1-2".into(),
        gui_executable_sha256: "0".repeat(64),
        target: super::super::weavelo_worker::Target {
            peer_device_id: "synthetic-peer".into(),
            source: super::super::weavelo_worker::Source::Window(42),
            connection_id: 7,
        },
    };
    let mut id = identity();
    id.desktop_id = route.desktop_id();
    let workers = fixture(
        &dir.path().join("state"),
        &dir.path().join("lock"),
        &id,
        "normal",
    );
    {
        let mut state = workers.state.lock().await;
        let a = state.approval.as_mut().unwrap();
        a.weavelo = Some(route);
        a.input = false;
    }
    let frame = FrameRef {
        binding: Binding {
            identity: id.clone(),
            epoch: Uuid::new_v4(),
        },
        source_revision: 1,
        frame_sequence: 1,
        mapping: FrameMapping {
            frame_size: [2, 2],
            content: ContentRect {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
            },
            logical_size: [2, 2],
            desktop_position: [0, 0],
            rotation: Rotation::R0,
        },
    };
    let request = Request::Act {
        frame,
        sequence: 1,
        actions: vec![Action::PointerMove { x: 0., y: 0. }],
    };
    assert!(
        workers
            .dispatch(Uuid::new_v4(), &id, 8, request.clone())
            .await
            .is_err()
    );
    let result = workers
        .dispatch(Uuid::new_v4(), &id, 7, request)
        .await
        .unwrap();
    assert_eq!(result["reason"], "weavelo_observe_only");
    assert!(workers.state.lock().await.pipe.is_none());
}

pub(crate) const FIXTURE: &str = r#"
import sys,struct,json,time,hashlib
MODE="normal"
def exact(n):
 b=b""
 while len(b)<n:
  v=sys.stdin.buffer.read(n-len(b))
  if not v:return None
  b+=v
 return b
binding=None
sequence=0
while True:
 head=exact(4)
 if head is None:break
 raw=exact(struct.unpack(">I",head)[0])
 if raw is None:break
 e=json.loads(raw); r=e["request"]; op=r["operation"]; pixels=None; data=b""
 if op=="open" and MODE=="open_timeout":continue
 if op=="open":
  binding={"identity":r["identity"],"epoch":"44444444-4444-4444-8444-444444444444"}
  result={"Ok":{"state":"opened","binding":binding,"expires_at_ms":int(time.time()*1000)+r["lease_ms"]}}
 elif op=="observe":
  sequence+=1
  frame={"binding":binding,"source_revision":1,"frame_sequence":sequence,"mapping":{"frame_size":[2,2],"content":{"left":0,"top":0,"width":2,"height":2},"logical_size":[2,2],"desktop_position":[0,0],"rotation":"r0"}}
  if MODE=="geometry":frame["mapping"]["frame_size"]=[4,2]
  planes=[b"abcd",b"ef"]
  pixels={"encoding":"nv12","size":[2,2],"color":[1,1,1,1],"plane_lengths":[4,2],"plane_sha256":[hashlib.sha256(p).hexdigest() for p in planes]}
  data=b"".join(planes)
  if MODE=="corrupt":data=b"ABCDef"
  result={"Ok":{"state":"observed","frame":frame}}
 elif op=="act":
  if MODE=="act_eof":sys.exit(9)
  result={"Ok":{"state":"accepted","sequence":r["sequence"],"accepted_actions":len(r["actions"]),"effect_confirmed":False}}
 elif op=="close":
  result={"Err":"outcome_unknown"} if MODE=="close_unknown" else {"Ok":{"state":"closed"}}
 else:sys.exit(10)
 if MODE=="oversized" and op=="observe":
  sys.stdout.buffer.write(struct.pack(">I",65537));sys.stdout.buffer.flush();continue
 h=json.dumps({"request_id":e["request_id"],"result":result,"pixels":pixels}).encode()
 sys.stdout.buffer.write(struct.pack(">I",len(h))+h+data);sys.stdout.buffer.flush()
"#;
pub(crate) fn identity() -> Identity {
    Identity {
        owner: "offline-owner".into(),
        task_id: Uuid::new_v4(),
        device_id: Uuid::new_v4(),
        desktop_id: "offline-private-pipe".into(),
    }
}
pub(crate) fn fixture(path: &Path, lock_path: &Path, id: &Identity, mode: &str) -> NativeWorkers {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::create_dir_all(path);
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        if let Some(parent) = lock_path.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut workers = NativeWorkers::disabled(path);
    workers.lock_path = lock_path.into();
    let a = LaunchApproval {
        weavelo: None,
        enabled: true,
        observe: true,
        input: true,
        capability_version: 7,
        identity: CapabilityIdentity::from_execution(id),
        desktop_uid: uid().unwrap(),
        desktop_session: "offline".into(),
        logical_size: [2, 2],
        logical_position: [0, 0],
        max_frame_bytes: MAX_WORKER_PIXELS,
        worker_path: PathBuf::new(),
        worker_sha256: String::new(),
        agent_sha256: String::new(),
    };
    let mut state = workers.state.try_lock().unwrap();
    state.approval = Some(a);
    let mut source = FIXTURE.replace(
        "MODE=\"normal\"",
        &format!("MODE={}", serde_json::to_string(mode).unwrap()),
    );
    if mode == "four_k" {
        source = source
            .replace("\"frame_size\":[2,2]", "\"frame_size\":[3840,2160]")
            .replace("\"width\":2,\"height\":2", "\"width\":3840,\"height\":2160")
            .replace("\"size\":[2,2]", "\"size\":[3840,2160]")
            .replace(
                "planes=[b\"abcd\",b\"ef\"]",
                "planes=[bytes(8294400),bytes(4147200)]",
            )
            .replace(
                "\"plane_lengths\":[4,2]",
                "\"plane_lengths\":[len(p) for p in planes]",
            );
    }
    state.fixture = Some(source);
    drop(state);
    workers
}
async fn open(w: &NativeWorkers, id: &Identity) -> Binding {
    let v = w
        .dispatch(
            Uuid::new_v4(),
            id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 30000,
            },
        )
        .await
        .unwrap();
    serde_json::from_value(v["binding"].clone()).unwrap()
}
#[tokio::test]
async fn actual_stdio_frame_hash_query_and_owner_epoch_revoke() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "normal");
    let binding = open(&w, &id).await;
    let result = w
        .dispatch(
            Uuid::new_v4(),
            &id,
            7,
            Request::Observe {
                binding: binding.clone(),
            },
        )
        .await
        .unwrap();
    let artifact: FrameArtifact = serde_json::from_value(result["frame_artifact"].clone()).unwrap();
    assert_eq!(artifact.bytes, 6);
    assert_eq!(artifact.sha256, digest(b"abcdef"));
    assert_eq!(
        artifact.pixels.plane_sha256,
        vec![digest(b"abcd"), digest(b"ef")]
    );
    assert_eq!(w.read_frame(&artifact, &id, 7).await.unwrap(), b"abcdef");
    let mut changed = artifact.clone();
    changed.pixels.color = [9; 4];
    assert!(w.read_frame(&changed, &id, 7).await.is_err());
    let mut stranger = id.clone();
    stranger.task_id = Uuid::new_v4();
    assert!(w.read_frame(&artifact, &stranger, 7).await.is_err());
    assert!(
        w.dispatch(
            Uuid::new_v4(),
            &stranger,
            7,
            Request::Close {
                binding: binding.clone()
            }
        )
        .await
        .is_err()
    );
    let mut wrong = binding.clone();
    wrong.epoch = Uuid::new_v4();
    assert!(!w.revoke(Some(&wrong)).await);
    assert!(w.revoke(Some(&binding)).await);
    assert!(w.read_frame(&artifact, &id, 7).await.is_err());
    assert!(
        !dir.path()
            .join("desktop-frames")
            .join(artifact.operation_id.to_string())
            .exists()
    );
}
#[tokio::test]
async fn actual_pipe_rejects_bytes_metadata_corruption_and_bounds() {
    for mode in ["corrupt", "geometry", "oversized"] {
        let dir = tempfile::tempdir().unwrap();
        let id = identity();
        let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, mode);
        let binding = open(&w, &id).await;
        assert_eq!(
            w.dispatch(Uuid::new_v4(), &id, 7, Request::Observe { binding })
                .await
                .unwrap()["error"],
            "outcome_unknown"
        );
        assert_eq!(
            w.dispatch(
                Uuid::new_v4(),
                &id,
                7,
                Request::Open {
                    identity: id.clone(),
                    lease_ms: 1000
                }
            )
            .await
            .unwrap()["error"],
            "outcome_unknown"
        );
    }
}
#[tokio::test]
async fn actual_file_lock_excludes_independent_agent_and_task() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let id = identity();
    let mut other = id.clone();
    other.task_id = Uuid::new_v4();
    let lock = dir.path().join("same-desktop.lock");
    let a = fixture(&first, &lock, &id, "normal");
    let b = fixture(&second, &lock, &other, "normal");
    let binding = open(&a, &id).await;
    assert!(
        b.dispatch(
            Uuid::new_v4(),
            &other,
            7,
            Request::Open {
                identity: other.clone(),
                lease_ms: 1000
            }
        )
        .await
        .is_err()
    );
    assert!(a.revoke(Some(&binding)).await);
    let other_binding = open(&b, &other).await;
    assert!(b.revoke(Some(&other_binding)).await);
}
#[tokio::test]
async fn cache_budget_expiry_and_file_authority() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "normal");
    let binding = open(&w, &id).await;
    let mut artifacts = Vec::new();
    for _ in 0..2 {
        let r = w
            .dispatch(
                Uuid::new_v4(),
                &id,
                7,
                Request::Observe {
                    binding: binding.clone(),
                },
            )
            .await
            .unwrap();
        artifacts
            .push(serde_json::from_value::<FrameArtifact>(r["frame_artifact"].clone()).unwrap());
    }
    assert!(
        w.dispatch(
            Uuid::new_v4(),
            &id,
            7,
            Request::Observe {
                binding: binding.clone()
            }
        )
        .await
        .is_err()
    );
    let file = dir
        .path()
        .join("desktop-frames")
        .join(artifacts[0].operation_id.to_string());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(w.read_frame(&artifacts[0], &id, 7).await.is_err());
    for artifact in w.state.lock().await.artifacts.values_mut() {
        artifact.expires_at_ms = 0;
    }
    assert!(w.artifact(artifacts[1].operation_id).await.is_err());
    assert!(
        !dir.path()
            .join("desktop-frames")
            .join(artifacts[1].operation_id.to_string())
            .exists()
    );
    assert!(w.read_frame(&artifacts[1], &id, 7).await.is_err());
    assert!(w.revoke(Some(&binding)).await);
}
#[tokio::test]
async fn close_unknown_stays_unknown_and_default_stays_off() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let disabled = NativeWorkers::disabled(dir.path());
    assert_eq!(
        disabled
            .dispatch(
                Uuid::new_v4(),
                &id,
                7,
                Request::Open {
                    identity: id.clone(),
                    lease_ms: 1000
                }
            )
            .await
            .unwrap()["error"],
        "unavailable"
    );
    let w = fixture(
        dir.path(),
        &dir.path().join("desktop.lock"),
        &id,
        "close_unknown",
    );
    let binding = open(&w, &id).await;
    assert_eq!(
        w.dispatch(Uuid::new_v4(), &id, 7, Request::Close { binding })
            .await
            .unwrap()["error"],
        "outcome_unknown"
    );
    assert_eq!(
        w.dispatch(
            Uuid::new_v4(),
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 1000
            }
        )
        .await
        .unwrap()["error"],
        "outcome_unknown"
    );
}

#[tokio::test]
async fn unknown_cleanup_quarantines_actual_desktop_lock() {
    let dir = tempfile::tempdir().unwrap();
    let a_dir = dir.path().join("a");
    let b_dir = dir.path().join("b");
    std::fs::create_dir(&a_dir).unwrap();
    std::fs::create_dir(&b_dir).unwrap();
    let id = identity();
    let mut other = id.clone();
    other.task_id = Uuid::new_v4();
    let lock = dir.path().join("desktop.lock");
    let a = fixture(&a_dir, &lock, &id, "close_unknown");
    let b = fixture(&b_dir, &lock, &other, "normal");
    let binding = open(&a, &id).await;
    assert_eq!(
        a.dispatch(Uuid::new_v4(), &id, 7, Request::Close { binding })
            .await
            .unwrap()["error"],
        "outcome_unknown"
    );
    a.revoke(None).await;
    assert!(
        b.dispatch(
            Uuid::new_v4(),
            &other,
            7,
            Request::Open {
                identity: other.clone(),
                lease_ms: 1000
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn persistent_capability_serves_new_task_after_confirmed_close() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "normal")
        .with_store(store.clone());
    let b = open(&w, &id).await;
    assert_eq!(
        w.dispatch(Uuid::new_v4(), &id, 7, Request::Close { binding: b })
            .await
            .unwrap()["state"],
        "closed"
    );
    let mut next = id.clone();
    next.task_id = Uuid::new_v4();
    let mut foreign = next.clone();
    foreign.owner = "another-owner".into();
    assert!(
        w.dispatch(
            Uuid::new_v4(),
            &foreign,
            7,
            Request::Open {
                identity: foreign.clone(),
                lease_ms: 1000
            }
        )
        .await
        .is_err()
    );
    let b = open(&w, &next).await;
    assert_eq!(b.identity.task_id, next.task_id);
    assert!(w.close_binding(&b).await);
}
#[tokio::test]
async fn durable_unknown_guard_survives_fresh_state_and_new_grant() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    let w = fixture(
        dir.path(),
        &dir.path().join("desktop.lock"),
        &id,
        "close_unknown",
    )
    .with_store(store.clone());
    let b = open(&w, &id).await;
    assert_eq!(
        w.dispatch(Uuid::new_v4(), &id, 7, Request::Close { binding: b })
            .await
            .unwrap()["error"],
        "outcome_unknown"
    );
    // New runtime state and independent OS lock cannot erase durable intent.
    let fresh = fixture(
        dir.path(),
        &dir.path().join("fresh-process.lock"),
        &id,
        "normal",
    )
    .with_store(store);
    fresh
        .state
        .lock()
        .await
        .approval
        .as_mut()
        .unwrap()
        .capability_version = 8;
    let mut next = id.clone();
    next.task_id = Uuid::new_v4();
    assert_eq!(
        fresh
            .dispatch(
                Uuid::new_v4(),
                &next,
                8,
                Request::Open {
                    identity: next.clone(),
                    lease_ms: 1000
                }
            )
            .await
            .unwrap()["error"],
        "outcome_unknown"
    );
}

#[tokio::test]
async fn unknown_close_retains_owned_exit_evidence_and_original_guard() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    let w = fixture(
        dir.path(),
        &dir.path().join("desktop.lock"),
        &id,
        "close_unknown",
    )
    .with_store(store.clone());
    let original = Uuid::new_v4();
    let opened = w
        .dispatch(
            original,
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 30000,
            },
        )
        .await
        .unwrap();
    let binding: Binding = serde_json::from_value(opened["binding"].clone()).unwrap();
    let closed = w
        .dispatch(Uuid::new_v4(), &id, 7, Request::Close { binding })
        .await
        .unwrap();
    assert_eq!(closed["error"], "outcome_unknown");
    let key = digest(format!("{}:{}", id.device_id, id.desktop_id).as_bytes());
    let guard = store
        .get::<serde_json::Value>("desktop_guard", &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guard["operation_id"], original.to_string());
    assert_eq!(guard["state"], "opening_or_active");
    assert!(guard["worker_pid"].as_u64().unwrap() > 0);
    assert_eq!(guard["worker_exit"]["pid"], guard["worker_pid"]);
    assert_eq!(guard["worker_exit"]["reaped"], true);
    assert_eq!(guard["worker_exit"]["exit_code"], 0);
    assert_eq!(guard["worker_exit"]["forced_stop"], false);
    assert!(w.state.lock().await.poisoned);
}
#[tokio::test]
async fn action_eof_keeps_actual_nonzero_worker_exit_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "act_eof")
        .with_store(store.clone());
    let binding = open(&w, &id).await;
    let observed = w
        .dispatch(Uuid::new_v4(), &id, 7, Request::Observe { binding })
        .await
        .unwrap();
    let frame: FrameRef = serde_json::from_value(observed["frame"].clone()).unwrap();
    let acted = w
        .dispatch(
            Uuid::new_v4(),
            &id,
            7,
            Request::Act {
                frame,
                sequence: 1,
                actions: vec![Action::PointerMove { x: 1.0, y: 1.0 }],
            },
        )
        .await
        .unwrap();
    assert_eq!(acted["error"], "outcome_unknown");
    let key = digest(format!("{}:{}", id.device_id, id.desktop_id).as_bytes());
    let guard = store
        .get::<serde_json::Value>("desktop_guard", &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guard["worker_exit"]["exit_code"], 9);
    assert_eq!(guard["worker_exit"]["reaped"], true);
    assert_eq!(guard["worker_exit"]["forced_stop"], false);
}
#[tokio::test]
async fn eof_ignoring_owned_child_is_reaped_and_forced_stop_is_explicit() {
    let mut command = Command::new("/usr/bin/python3");
    command.args([
        "-I",
        "-B",
        "-u",
        "-c",
        "import sys,time;sys.stdin.buffer.read();time.sleep(10)",
    ]);
    let mut pipe = Pipe::spawn_command(command).unwrap();
    let original = pipe.worker_pid;
    assert!(pipe.disconnect().await.is_err());
    let exit = pipe.exit_evidence.as_ref().unwrap();
    assert_eq!(exit["pid"], serde_json::json!(original));
    assert_eq!(exit["reaped"], true);
    assert_eq!(exit["forced_stop"], true);
    assert_eq!(exit["signal"], 9);
    assert!(pipe.child.try_wait().unwrap().is_some());
}

#[tokio::test]
async fn open_timeout_retains_unknown_and_creation_identity() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    let w = fixture(
        dir.path(),
        &dir.path().join("desktop.lock"),
        &id,
        "open_timeout",
    )
    .with_store(store.clone());
    let original = Uuid::new_v4();
    let result = w
        .dispatch(
            original,
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 1000,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["error"], "outcome_unknown");
    let evidence = &result["worker_lifecycle"];
    assert_eq!(evidence["operation_id"], original.to_string());
    assert_eq!(evidence["worker_exit"]["reaped"], true);
    assert!(
        evidence["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["phase"] == "open_sent")
    );
    assert!(
        !evidence["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["phase"] == "reply_received")
    );
    #[cfg(target_os = "linux")]
    {
        assert_eq!(evidence["worker_identity"]["identity_state"], "observed");
        assert!(evidence["worker_identity"]["start_ticks"].as_u64().unwrap() > 0);
    }
    assert!(w.state.lock().await.poisoned);
    let again = w
        .dispatch(
            Uuid::new_v4(),
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 1000,
            },
        )
        .await
        .unwrap();
    assert_eq!(again["error"], "outcome_unknown");
    assert_eq!(
        again["worker_lifecycle"]["worker_identity"],
        evidence["worker_identity"]
    );
}
#[tokio::test]
async fn lifecycle_persistence_failure_never_sends_open_or_reports_success() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_lifecycle BEFORE UPDATE ON kv WHEN NEW.kind='desktop_guard' BEGIN SELECT RAISE(ABORT,'injected lifecycle persistence failure'); END;")
        .execute(&store.pool).await.unwrap();
    let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "normal")
        .with_store(store.clone());
    let original = Uuid::new_v4();
    let result = w
        .dispatch(
            original,
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 1000,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["error"], "outcome_unknown");
    assert_eq!(result["worker_lifecycle"]["persistence_failed"], true);
    assert_eq!(result["worker_lifecycle"]["worker_exit"]["reaped"], true);
    assert!(
        !result["worker_lifecycle"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["phase"] == "open_sent")
    );
    let key = digest(format!("{}:{}", id.device_id, id.desktop_id).as_bytes());
    assert_eq!(
        store
            .get::<serde_json::Value>("desktop_guard", &key)
            .await
            .unwrap()
            .unwrap()["operation_id"],
        original.to_string()
    );
    assert!(w.state.lock().await.poisoned);
}
#[tokio::test]
async fn parent_interruption_keeps_original_guard_without_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    let key = digest(format!("{}:{}", id.device_id, id.desktop_id).as_bytes());
    let original = Uuid::new_v4();
    let guard = serde_json::json!({"operation_id":original,"state":"opening_or_active",
        "worker_lifecycle":{"parent_identity":{"pid":1,"identity_state":"observed","start_ticks":1,"boot_id":"11111111-1111-4111-8111-111111111111"},"worker_exit":null}});
    store
        .put("desktop_guard", &key, &guard, i64::MAX)
        .await
        .unwrap();
    let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "normal")
        .with_store(store.clone());
    let result = w
        .dispatch(
            Uuid::new_v4(),
            &id,
            7,
            Request::Open {
                identity: id.clone(),
                lease_ms: 1000,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["error"], "outcome_unknown");
    assert_eq!(result["recovery_reason"], "parent_runtime_interrupted");
    assert_eq!(result["original_operation_id"], original.to_string());
    assert!(w.state.lock().await.pipe.is_none());
    assert_eq!(
        store
            .get::<serde_json::Value>("desktop_guard", &key)
            .await
            .unwrap()
            .unwrap(),
        guard
    );
}

fn completed_close_guard(original: Uuid, binding: &Binding) -> serde_json::Value {
    let exit = serde_json::json!({"protocol":1,"pid":u32::MAX,"reaped":true,"exit_success":true,"exit_code":0,"signal":null,"forced_stop":false});
    serde_json::json!({"operation_id":original,"identity":binding.identity,"capability_version":7,"worker_pid":u32::MAX,
        "worker_exit":exit,"worker_lifecycle":{"protocol":1,"operation_id":original,"binding":binding,"persistence_failed":false,
        "shutdown_status":"confirmed","worker_identity":{"pid":u32::MAX,"identity_state":"observed","start_ticks":1,
        "boot_id":"11111111-1111-4111-8111-111111111111"},"worker_exit":exit}})
}
#[tokio::test]
async fn existing_close_reconciles_only_exact_confirmed_owned_terminal_guard() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity();
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    let binding = Binding {
        identity: id.clone(),
        epoch: Uuid::new_v4(),
    };
    let original = Uuid::new_v4();
    let key = digest(format!("{}:{}", id.device_id, id.desktop_id).as_bytes());
    let guard = completed_close_guard(original, &binding);
    store
        .put("desktop_guard", &key, &guard, i64::MAX)
        .await
        .unwrap();
    let unrelated = serde_json::json!({"operation_id":Uuid::new_v4(),"state":"opening_or_active"});
    store
        .put("desktop_guard", "unrelated", &unrelated, i64::MAX)
        .await
        .unwrap();
    let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "normal")
        .with_store(store.clone());
    w.state.lock().await.poisoned = true;
    let result = w
        .dispatch(Uuid::new_v4(), &id, 7, Request::Close { binding })
        .await
        .unwrap();
    assert_eq!(result["state"], "closed");
    assert_eq!(result["recovered_from_guard"], true);
    assert_eq!(result["original_operation_id"], original.to_string());
    assert_eq!(result["original_outcome_unchanged"], true);
    assert!(
        store
            .get::<serde_json::Value>("desktop_guard", &key)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get::<serde_json::Value>("desktop_guard", "unrelated")
            .await
            .unwrap()
            .unwrap(),
        unrelated
    );
    assert!(w.state.lock().await.pipe.is_none());
}
#[tokio::test]
async fn recovery_close_preserves_legacy_partial_and_conflicting_guards() {
    for mode in 0..6 {
        let dir = tempfile::tempdir().unwrap();
        let id = identity();
        let store = crate::store::Store::open(&dir.path().join("db"))
            .await
            .unwrap();
        let binding = Binding {
            identity: id.clone(),
            epoch: Uuid::new_v4(),
        };
        let key = digest(format!("{}:{}", id.device_id, id.desktop_id).as_bytes());
        let mut guard = completed_close_guard(Uuid::new_v4(), &binding);
        match mode {
            0 => {
                guard.as_object_mut().unwrap().remove("worker_lifecycle");
            }
            1 => guard["worker_lifecycle"]["shutdown_status"] = serde_json::json!("unknown"),
            2 => guard["worker_lifecycle"]["persistence_failed"] = serde_json::json!(true),
            3 => guard["worker_lifecycle"]["worker_exit"]["reaped"] = serde_json::json!(false),
            4 => guard["worker_lifecycle"]["binding"]["epoch"] = serde_json::json!(Uuid::new_v4()),
            _ => {
                guard["worker_lifecycle"]["worker_identity"] = process_identity(std::process::id())
            }
        }
        store
            .put("desktop_guard", &key, &guard, i64::MAX)
            .await
            .unwrap();
        let w = fixture(dir.path(), &dir.path().join("desktop.lock"), &id, "normal")
            .with_store(store.clone());
        let result = w
            .dispatch(Uuid::new_v4(), &id, 7, Request::Close { binding })
            .await
            .unwrap();
        assert_eq!(result["error"], "outcome_unknown");
        assert_eq!(
            store
                .get::<serde_json::Value>("desktop_guard", &key)
                .await
                .unwrap()
                .unwrap(),
            guard
        );
        assert!(w.state.lock().await.pipe.is_none());
    }
}

async fn capability_fixture(dir: &Path) -> (NativeWorkers, Identity, PathBuf) {
    let id = identity();
    let w = fixture(dir, &dir.join("desktop.lock"), &id, "normal");
    let source = dir.join("desktop-launch.json");
    let bytes = b"offline scoped approval";
    std::fs::write(&source, bytes).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    w.state.lock().await.source = Some((source.clone(), digest(bytes)));
    (w, id, source)
}
#[cfg(target_os = "macos")]
#[tokio::test]
async fn newer_compatible_approval_refreshes_without_reviving_old_or_broader_grants() {
    use super::super::weavelo_route::{Route, Source, Target};
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
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
    let mut id = identity();
    id.desktop_id = route.desktop_id();
    let binary = std::env::current_exe().unwrap();
    let hash = digest(&std::fs::read(&binary).unwrap());
    let mut approval = LaunchApproval {
        weavelo: Some(route),
        enabled: true,
        observe: true,
        input: false,
        capability_version: 7,
        identity: CapabilityIdentity::from_execution(&id),
        desktop_uid: uid().unwrap(),
        desktop_session: "weavelo".into(),
        logical_size: [2, 2],
        logical_position: [0, 0],
        max_frame_bytes: MAX_WORKER_PIXELS,
        worker_path: binary,
        worker_sha256: hash.clone(),
        agent_sha256: hash,
    };
    let path = dir.path().join("desktop-launch.json");
    let original = serde_json::to_vec(&approval).unwrap();
    std::fs::write(&path, &original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let workers = NativeWorkers::load(dir.path(), &id.device_id.to_string());
    assert!(workers.state.lock().await.approval.is_some());
    approval.capability_version = 8;
    approval.weavelo.as_mut().unwrap().gui_executable_sha256 = "1".repeat(64);
    let renewed = serde_json::to_vec(&approval).unwrap();
    std::fs::write(&path, &renewed).unwrap();
    workers.refresh_compatible_approval().await;
    assert_eq!(
        workers
            .state
            .lock()
            .await
            .approval
            .as_ref()
            .unwrap()
            .capability_version,
        8
    );
    assert_eq!(std::fs::read(&path).unwrap(), renewed);
    assert_eq!(
        workers.current_capability().await,
        Some((8, approval.identity.clone()))
    );
    let mut broader = approval.clone();
    broader.capability_version = 9;
    broader.weavelo.as_mut().unwrap().target.peer_device_id = "other-peer".into();
    broader.identity.desktop_id = broader.weavelo.as_ref().unwrap().desktop_id();
    std::fs::write(&path, serde_json::to_vec(&broader).unwrap()).unwrap();
    workers.refresh_compatible_approval().await;
    assert_eq!(
        workers
            .state
            .lock()
            .await
            .approval
            .as_ref()
            .unwrap()
            .capability_version,
        8
    );
    workers.revoke(None).await;
    assert!(workers.current_capability().await.is_none());
    std::fs::write(&path, &original).unwrap();
    workers.refresh_compatible_approval().await;
    assert!(workers.state.lock().await.approval.is_none());
    std::fs::write(&path, &renewed).unwrap();
    workers.refresh_compatible_approval().await;
    assert!(workers.state.lock().await.approval.is_none());
}

#[tokio::test]
async fn capability_reports_current_ready_authority_without_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let (w, _, source) = capability_fixture(dir.path()).await;
    let before = std::fs::read(&source).unwrap();
    let report = w
        .capabilities_with_probe("screencapturekit", |_| true)
        .await;
    assert!(report.available);
    assert_eq!(report.backend, "macos_pid_private_window_poc");
    assert_eq!(std::fs::read(source).unwrap(), before);
    assert!(w.state.lock().await.pipe.is_none());
}
#[tokio::test]
async fn capability_reports_changed_missing_or_insecure_approval_unavailable() {
    for mode in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let (w, _, source) = capability_fixture(dir.path()).await;
        match mode {
            0 => std::fs::write(&source, b"revoked or changed").unwrap(),
            1 => std::fs::remove_file(&source).unwrap(),
            _ => std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap(),
        }
        let report = w
            .capabilities_with_probe("screencapturekit", |_| {
                panic!("revoked authority reached platform probe")
            })
            .await;
        assert!(!report.available);
        assert_eq!(report.backend, "desktop_approval_changed");
        assert!(w.state.lock().await.approval.is_some());
        assert!(w.state.lock().await.pipe.is_none());
    }
}
#[tokio::test]
async fn capability_reports_unknown_without_clearing_guard_or_poison() {
    for poison in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut w, id, _) = capability_fixture(dir.path()).await;
        let store = crate::store::Store::open(&dir.path().join("db"))
            .await
            .unwrap();
        let key = digest(format!("{}:{}", id.device_id, id.desktop_id).as_bytes());
        let guard = serde_json::json!({"operation_id":Uuid::new_v4(),"state":"opening_or_active"});
        store
            .put("desktop_guard", &key, &guard, i64::MAX)
            .await
            .unwrap();
        w = w.with_store(store.clone());
        w.state.lock().await.poisoned = poison;
        let report = w
            .capabilities_with_probe("screencapturekit", |_| {
                panic!("unresolved intent reached platform probe")
            })
            .await;
        assert!(!report.available);
        assert_eq!(
            report.backend,
            if poison {
                "outcome_unknown"
            } else {
                "desktop_recovery_required"
            }
        );
        assert_eq!(
            store
                .get::<serde_json::Value>("desktop_guard", &key)
                .await
                .unwrap()
                .unwrap(),
            guard
        );
        assert_eq!(w.state.lock().await.poisoned, poison);
        assert!(w.state.lock().await.pipe.is_none());
    }
}
#[tokio::test]
async fn capability_reports_state_read_failure_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let (w, _, _) = capability_fixture(dir.path()).await;
    let store = crate::store::Store::open(&dir.path().join("db"))
        .await
        .unwrap();
    store.pool.close().await;
    let w = w.with_store(store);
    let report = w
        .capabilities_with_probe("screencapturekit", |_| {
            panic!("unavailable state reached platform probe")
        })
        .await;
    assert!(!report.available);
    assert_eq!(report.backend, "desktop_state_unavailable");
}
#[tokio::test]
async fn capability_reports_platform_permission_reason_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let (w, _, _) = capability_fixture(dir.path()).await;
    for status in [
        "macos_console_unavailable",
        "macos_screen_recording_required",
        "macos_accessibility_required",
    ] {
        let report = w.capabilities_with_probe(status, |_| true).await;
        assert!(!report.available);
        assert_eq!(report.backend, status);
    }
    assert!(w.state.lock().await.pipe.is_none());
}
#[tokio::test]
async fn capability_reports_unconfigured_system_blocker_without_grant() {
    let dir = tempfile::tempdir().unwrap();
    let w = NativeWorkers::disabled(dir.path());
    for status in [
        "macos_screen_recording_required",
        "macos_accessibility_required",
        "macos_console_unavailable",
        "screencapturekit",
    ] {
        let report = w
            .capabilities_with_probe(status, |_| {
                panic!("unconfigured authority reached target probe")
            })
            .await;
        assert!(!report.available);
        assert_eq!(
            report.backend,
            if status == "screencapturekit" {
                "window_scoped_authority_required"
            } else {
                status
            }
        );
    }
    assert!(w.state.lock().await.approval.is_none());
    assert!(w.state.lock().await.pipe.is_none());
}
#[tokio::test]
async fn capability_reports_missing_window_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let (w, _, _) = capability_fixture(dir.path()).await;
    let report = w
        .capabilities_with_probe("screencapturekit", |_| false)
        .await;
    assert!(!report.available);
    assert_eq!(report.backend, "window_target_unavailable");
    assert!(w.state.lock().await.pipe.is_none());
}

#[test]
fn four_k_raw_pixels_fit_sixteen_mib_and_larger_layouts_fail() {
    let mut p = RawPixels {
        encoding: "nv12".into(),
        size: [3840, 2160],
        color: [1; 4],
        plane_lengths: vec![8294400, 4147200],
        plane_sha256: vec![digest(b"a"), digest(b"b")],
    };
    assert!(validate_pixels(&p).is_ok());
    p.size = [8192, 8192];
    p.plane_lengths = vec![67108864, 33554432];
    assert!(validate_pixels(&p).is_err());
}
