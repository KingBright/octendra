use super::*;
use std::cell::{Cell, RefCell};
fn expected() -> ExpectedIdentity {
    ExpectedIdentity {
        team_id: "ABC123DEF4".into(),
        sha256: "a".repeat(64),
    }
}
fn file() -> FileState {
    FileState {
        path: HELPER_PATH.into(),
        sha256: "a".repeat(64),
        size: 2048,
        device: 1,
        inode: 7,
        uid: 0,
        mode: 0o100755,
    }
}
struct Fixture {
    file: FileState,
    snapshots: Cell<usize>,
    changed: bool,
    missing: bool,
    signature_ok: bool,
    metadata_ok: bool,
    metadata: String,
    calls: RefCell<Vec<Vec<String>>>,
}
impl Fixture {
    fn new() -> Self {
        Self {
            file: file(),
            snapshots: Cell::new(0),
            changed: false,
            missing: false,
            signature_ok: true,
            metadata_ok: true,
            metadata: "Identifier=com.remotehosts.admin\nTeamIdentifier=ABC123DEF4\n".into(),
            calls: RefCell::new(vec![]),
        }
    }
}
impl Driver for Fixture {
    fn snapshot(&self) -> Result<FileState> {
        ensure!(!self.missing, "fixture_helper_missing");
        let n = self.snapshots.get();
        self.snapshots.set(n + 1);
        let mut f = self.file.clone();
        if self.changed && n > 0 {
            f.inode += 1;
        }
        Ok(f)
    }
    async fn codesign(&self, args: &[String]) -> Result<(bool, String)> {
        self.calls.borrow_mut().push(args.to_vec());
        Ok(if args[0] == "--verify" {
            (self.signature_ok, String::new())
        } else {
            (self.metadata_ok, self.metadata.clone())
        })
    }
}
#[test]
fn requirement_is_closed_developer_id_team_and_identifier_profile() {
    let args = verify_args(&expected()).unwrap();
    assert_eq!(&args[..3], ["--verify", "--strict", "--all-architectures"]);
    assert!(args[3].starts_with("-R=anchor apple generic and "));
    assert!(args[3].contains("certificate leaf[subject.OU] = \"ABC123DEF4\""));
    assert!(args[3].contains("field.1.2.840.113635.100.6.1.13"));
    assert!(args[3].ends_with("identifier \"com.remotehosts.admin\""));
    assert_eq!(args[4], HELPER_PATH);
    assert_eq!(args.len(), 5);
    assert!(!args.iter().any(|s| s == "--sign" || s == "-s"));
}
#[test]
fn expected_identity_rejects_requirement_injection_and_bad_digests() {
    for team in [
        "",
        "ABC123DEF",
        "abc123def4",
        "ABC123DEF4\" or true",
        "ABC123/EF4",
        "ＡBC123DEF4",
    ] {
        let mut e = expected();
        e.team_id = team.into();
        assert!(e.validate().is_err());
    }
    for hash in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
        let mut e = expected();
        e.sha256 = hash;
        assert!(e.validate().is_err());
    }
}
#[test]
fn metadata_rejects_ad_hoc_missing_duplicate_and_wrong_identity() {
    for text in [
        "Identifier=com.remotehosts.admin\nTeamIdentifier=ABC123DEF4\nSignature=adhoc",
        "Identifier=com.other\nTeamIdentifier=ABC123DEF4",
        "Identifier=com.remotehosts.admin\nTeamIdentifier=ZZZ123DEF4",
        "Identifier=com.remotehosts.admin",
        "Identifier=com.remotehosts.admin\nTeamIdentifier=ABC123DEF4\nTeamIdentifier=ABC123DEF4",
        "Identifier=com.remotehosts.admin\nIdentifier=com.remotehosts.admin\nTeamIdentifier=ABC123DEF4",
    ] {
        assert!(parse_identity(text, &expected()).is_err());
    }
}
#[tokio::test]
async fn signature_success_is_only_file_evidence_never_peer_grant_or_execution() {
    let d = Fixture::new();
    let out = verify_using(&d, &expected(), Identity::OrdinaryUser)
        .await
        .unwrap();
    assert!(out.file_identity_verified);
    assert!(!out.peer_authenticated && !out.grant_verified && !out.grants_authority);
    assert!(!out.capability.execution_implemented && !out.capability.execution_available);
    assert_eq!(out.capability.helper, Helper::DetectedUnverified);
    assert_eq!(out.capability.grant, Grant::NotChecked);
    assert_eq!(
        out.binding_sha256,
        digest(&(&expected(), &file(), &verify_args(&expected()).unwrap())).unwrap()
    );
    assert_eq!(d.calls.borrow()[0], verify_args(&expected()).unwrap());
    assert_eq!(
        d.calls.borrow()[1],
        ["--display", "--verbose=4", HELPER_PATH]
    );
}
#[tokio::test]
async fn unsafe_file_rejected_before_codesign() {
    for index in 0..6 {
        let mut d = Fixture::new();
        match index {
            0 => d.file.uid = 501,
            1 => d.file.mode = 0o100775,
            2 => d.file.mode = 0o104755,
            3 => d.file.mode = 0o100644,
            4 => d.file.sha256 = "b".repeat(64),
            _ => d.file.path = "/Users/test/helper".into(),
        }
        assert!(
            verify_using(&d, &expected(), Identity::OrdinaryUser)
                .await
                .is_err()
        );
        assert!(d.calls.borrow().is_empty());
    }
}
#[tokio::test]
async fn failed_signature_does_not_fall_back_to_display_or_run_helper() {
    let mut d = Fixture::new();
    d.signature_ok = false;
    assert!(
        verify_using(&d, &expected(), Identity::OrdinaryUser)
            .await
            .is_err()
    );
    assert_eq!(d.calls.borrow().len(), 1);
}
#[tokio::test]
async fn metadata_failure_or_wrong_team_is_not_accepted() {
    for failure in [true, false] {
        let mut d = Fixture::new();
        if failure {
            d.metadata_ok = false;
        } else {
            d.metadata = "Identifier=com.remotehosts.admin\nTeamIdentifier=OTHER12345".into();
        }
        assert!(
            verify_using(&d, &expected(), Identity::OrdinaryUser)
                .await
                .is_err()
        );
    }
}
#[tokio::test]
async fn helper_replacement_after_verification_invalidates_evidence() {
    let mut d = Fixture::new();
    d.changed = true;
    assert!(
        verify_using(&d, &expected(), Identity::OrdinaryUser)
            .await
            .unwrap_err()
            .to_string()
            .contains("replaced")
    );
}
#[tokio::test]
async fn missing_helper_is_not_a_discovery_based_authorization() {
    let mut d = Fixture::new();
    d.missing = true;
    assert!(
        verify_using(&d, &expected(), Identity::OrdinaryUser)
            .await
            .is_err()
    );
    assert!(d.calls.borrow().is_empty());
}
#[tokio::test]
async fn non_macos_never_uses_another_platform_verifier() {
    if !cfg!(target_os = "macos") {
        assert!(
            run(Command::VerifyIdentity {
                team_id: expected().team_id,
                expected_sha256: expected().sha256
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("macos_native")
        );
    }
}
