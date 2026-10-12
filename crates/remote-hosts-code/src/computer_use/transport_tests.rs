use super::*;
use std::os::unix::fs::PermissionsExt;

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
    let mut workers = NativeWorkers::disabled(path);
    workers.lock_path = lock_path.into();
    let a = LaunchApproval {
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
