use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
fn elf() -> Vec<u8> {
    let mut b = vec![0; 120];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    b[16..18].copy_from_slice(&2u16.to_le_bytes());
    b[18..20].copy_from_slice(&62u16.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&1u16.to_le_bytes());
    b[64..68].copy_from_slice(&1u32.to_le_bytes());
    b
}
fn fixture() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let device = uuid::Uuid::new_v4().to_string();
    let grant_id = uuid::Uuid::new_v4().to_string();
    let p = Policy {
        protocol: 1,
        device_id: device.clone(),
        grant_id: grant_id.clone(),
        allowed_uid: 1000,
        allowed_gid: 1000,
        home: "/unconfirmed".into(),
        platform: "linux".into(),
        enabled: false,
    };
    let g = ServiceGrant {
        protocol: 1,
        device_id: device.clone(),
        grant_id,
        enabled: false,
        not_before: 0,
        expires_at: 0,
        action: Action::ManagedServiceRestart,
        unit: managed_service::SERVICE.into(),
        unit_sha256: "0".repeat(64),
        executable: "/unconfirmed/remote-hosts-code".into(),
        executable_sha256: "0".repeat(64),
        agent_config: "/unconfirmed/agent.json".into(),
        agent_config_sha256: "0".repeat(64),
    };
    fs::write(root.join("remote-hosts-admin"), elf()).unwrap();
    fs::write(root.join("policy.json"), serde_json::to_vec(&p).unwrap()).unwrap();
    fs::write(
        root.join("service-grant.json"),
        serde_json::to_vec(&g).unwrap(),
    )
    .unwrap();
    fs::write(root.join("service.template"), HELPER_UNIT).unwrap();
    fs::write(root.join("INSTALL.txt"), "fixture data only").unwrap();
    let m = Manifest {
        protocol: 2,
        profile: PROFILE.into(),
        helper_version: env!("CARGO_PKG_VERSION").into(),
        platform: "linux".into(),
        machine: "x86_64".into(),
        device_id: device,
        account: "test".into(),
        source_commit: "a".repeat(40),
        source_inputs_sha256: "b".repeat(64),
        validation_kind: "unsigned-dev".into(),
        status: "prepared_disabled".into(),
        default_enabled: false,
        files: NAMES
            .into_iter()
            .map(|n| (n.into(), sha(&fs::read(root.join(n)).unwrap())))
            .collect(),
    };
    let raw = serde_json::to_vec_pretty(&m).unwrap();
    let hash = sha(&raw);
    fs::write(root.join("manifest.json"), raw).unwrap();
    (d, hash)
}
fn rehash(root: &Path) -> String {
    let mut m: Manifest = read_json(&root.join("manifest.json")).unwrap();
    for name in NAMES {
        m.files
            .insert(name.into(), sha(&fs::read(root.join(name)).unwrap()));
    }
    let raw = serde_json::to_vec_pretty(&m).unwrap();
    let h = sha(&raw);
    fs::write(root.join("manifest.json"), raw).unwrap();
    h
}
fn site(b: &Bundle) -> SiteEvidence {
    let mut p = b.policy.clone();
    p.home = "/home/test".into();
    let mut g = b.grant.clone();
    g.executable = "/var/lib/agent/remote-hosts-code".into();
    g.agent_config = "/etc/agent.json".into();
    g.unit_sha256 = "a".repeat(64);
    g.executable_sha256 = "b".repeat(64);
    g.agent_config_sha256 = "c".repeat(64);
    SiteEvidence {
        protocol: 1,
        manifest_sha256: b.manifest_sha256.clone(),
        account: "test".into(),
        policy: p,
        grant: g,
        snapshot: Snapshot {
            current_service_active: true,
            ..Snapshot::default()
        },
        boot_id: uuid::Uuid::new_v4().to_string(),
        observed_at: 1000,
    }
}
#[test]
fn disabled_bundle_verifies_without_install_or_authority() {
    let (d, h) = fixture();
    let v = verify_report(d.path(), &h).unwrap();
    assert_eq!(v["default_enabled"], false);
    assert_eq!(v["installed"], false);
    assert_eq!(v["execution_available"], false);
    assert_eq!(v["resource"], managed_service::SERVICE);
    assert!(
        v["adapters"]["windows"]
            .as_str()
            .unwrap()
            .contains("required")
    );
}
#[test]
fn enabled_policies_and_arbitrary_installer_members_are_rejected() {
    let (d, h) = fixture();
    fs::write(d.path().join("install.sh"), "exit 0").unwrap();
    assert!(verify(d.path(), &h).is_err());
    fs::remove_file(d.path().join("install.sh")).unwrap();
    let mut p: Policy = read_json(&d.path().join("policy.json")).unwrap();
    p.enabled = true;
    fs::write(
        d.path().join("policy.json"),
        serde_json::to_vec(&p).unwrap(),
    )
    .unwrap();
    let h = rehash(d.path());
    assert!(verify(d.path(), &h).is_err());
}
#[test]
fn tampered_manifest_members_and_wrong_service_entry_fail_closed() {
    let (d, h) = fixture();
    assert!(verify(d.path(), &"a".repeat(64)).is_err());
    fs::write(
        d.path().join("service.template"),
        HELPER_UNIT.replace("linux-helper serve", "serve"),
    )
    .unwrap();
    assert!(verify(d.path(), &h).is_err());
    let h = rehash(d.path());
    assert!(verify(d.path(), &h).is_err());
}
#[test]
fn symlink_hardlink_and_oversized_json_inputs_are_not_opened_as_regular_data() {
    let (d, _) = fixture();
    let p = d.path().join("link");
    symlink("manifest.json", &p).unwrap();
    assert!(bytes(&p, JSON_LIMIT).is_err());
    let p = d.path().join("hard");
    fs::hard_link(d.path().join("policy.json"), &p).unwrap();
    assert!(bytes(&p, JSON_LIMIT).is_err());
    let p = d.path().join("large");
    fs::write(&p, vec![b' '; (JSON_LIMIT + 1) as usize]).unwrap();
    assert!(read_json::<Value>(&p).is_err());
}
#[test]
fn dynamic_wrong_arch_and_overflowing_elf_tables_are_rejected() {
    static_elf(&elf()).unwrap();
    let mut b = elf();
    b[64..68].copy_from_slice(&3u32.to_le_bytes());
    assert!(static_elf(&b).is_err());
    let mut b = elf();
    b[18..20].copy_from_slice(&183u16.to_le_bytes());
    assert!(static_elf(&b).is_err());
    let mut b = elf();
    b[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(static_elf(&b).is_err());
}
#[tokio::test]
async fn offline_install_review_is_blocked_and_reuses_original_record_identity() {
    let (d, h) = fixture();
    let id = uuid::Uuid::new_v4().to_string();
    let r = prepare(d.path(), &h, &id, None, None, None, false)
        .await
        .unwrap();
    assert_eq!(r.record.plan.request_id, id);
    assert_eq!(r.record.plan.action, INSTALL_ACTION);
    assert!(!r.ready_for_confirmation && !r.grants_authority && !r.execution_enabled);
    assert!(r.approved.is_none());
    assert_eq!(r.record.state, State::Prepared);
    assert!(r.record.plan.snapshot.files.is_empty());
}
#[test]
fn explicit_validity_and_unattended_scope_are_required_for_reviewed_data() {
    let (d, h) = fixture();
    let b = verify(d.path(), &h).unwrap();
    let s = site(&b);
    let id = uuid::Uuid::new_v4().to_string();
    assert!(make_review(&b, &id, Some(s.clone()), Some(900), Some(2000), false, 1000).is_err());
    assert!(make_review(&b, &id, Some(s.clone()), Some(900), None, true, 1000).is_err());
    assert!(make_review(&b, &id, Some(s.clone()), Some(900), Some(999), true, 1000).is_err());
    let r = make_review(&b, &id, Some(s), Some(900), Some(2000), true, 1000).unwrap();
    assert!(r.ready_for_confirmation);
    assert!(!r.grants_authority && !r.execution_enabled);
    assert_eq!(r.record.plan_sha256, digest(&r.record.plan).unwrap());
}
#[test]
fn isolated_render_exports_only_private_data_and_refuses_replacement() {
    let (d, h) = fixture();
    let b = verify(d.path(), &h).unwrap();
    let s = site(&b);
    let id = uuid::Uuid::new_v4().to_string();
    let r = make_review(&b, &id, Some(s.clone()), Some(900), Some(2000), true, 1000).unwrap();
    let accepted = r.record.plan_sha256.clone();
    let export = tempfile::tempdir().unwrap();
    let out = export.path().join("render");
    let v = render_verified(b, r.clone(), &accepted, &out, s.clone(), 1001).unwrap();
    assert_eq!(v["installed"], false);
    assert_eq!(v["system_changes"], false);
    assert_eq!(v["grants_authority"], false);
    let g: ServiceGrant = read_json(&out.join("service-grant.json")).unwrap();
    assert!(g.enabled);
    assert_eq!(g.unit, managed_service::SERVICE);
    assert_eq!(
        fs::metadata(out.join("policy.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(render_verified(verify(d.path(), &h).unwrap(), r, &accepted, &out, s, 1002).is_err());
}
#[test]
fn changed_boot_resource_digest_destination_or_plan_acceptance_cannot_render() {
    for index in 0..4 {
        let (d, h) = fixture();
        let b = verify(d.path(), &h).unwrap();
        let s = site(&b);
        let id = uuid::Uuid::new_v4().to_string();
        let mut r =
            make_review(&b, &id, Some(s.clone()), Some(900), Some(2000), true, 1000).unwrap();
        let mut fresh = s;
        let mut accepted = r.record.plan_sha256.clone();
        match index {
            0 => fresh.boot_id = uuid::Uuid::new_v4().to_string(),
            1 => fresh.grant.agent_config_sha256 = "d".repeat(64),
            2 => {
                r.destinations
                    .insert("service.template".into(), "/etc/other.service".into());
            }
            _ => accepted = "e".repeat(64),
        }
        let out = d.path().join("must-not-exist");
        assert!(render_verified(b, r, &accepted, &out, fresh, 1001).is_err());
        assert!(!out.exists());
    }
}
#[tokio::test]
async fn root_site_path_has_no_ordinary_user_or_other_platform_fallback() {
    if !cfg!(target_os = "linux") || !nix::unistd::geteuid().is_root() {
        let (d, h) = fixture();
        let e = inspect_site(d.path(), &h).await.unwrap_err();
        assert!(e.to_string().contains("separate_linux_administrator"));
    }
}
#[test]
fn new_file_writer_never_overwrites_or_follows_symlinks() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("record.json");
    write_new(&p, &json!({"a":1})).unwrap();
    assert!(write_new(&p, &json!({"a":2})).is_err());
    let link = d.path().join("link");
    symlink(&p, &link).unwrap();
    assert!(write_new(&link, &json!({})).is_err());
    assert_eq!(read_json::<Value>(&p).unwrap()["a"], 1);
}
