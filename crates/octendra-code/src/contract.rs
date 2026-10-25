//! Single-source adapter definitions and request-scoped exposure evidence.
use crate::{auth::Principal, capabilities, hash, tools};
use anyhow::{Result, ensure};
use axum::http::{HeaderMap, HeaderValue};
use rmcp::model::Tool;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

pub fn authorized_tools(p: &Principal) -> Vec<Tool> {
    tools::catalog()
        .into_iter()
        .filter(|t| tools::has_required_scopes(p, &t.name))
        .collect()
}
/// Discovery advertises consent metadata; it does not grant desktop authority.
pub fn discoverable_tools(_p: &Principal) -> Vec<Tool> {
    tools::catalog()
}
pub fn bundle() -> Value {
    let catalog = tools::catalog();
    let sha = hash(serde_json::to_vec(&catalog).expect("static catalog"));
    json!({"protocol":1,"version":env!("CARGO_PKG_VERSION"),"server_tools_sha256":sha,"adapter_tools_sha256":sha,
        "skill_revision":capabilities::embedded_skill_revision(),"tools":catalog,
        "transport_contract":{"request_id":"x-octendra-request-id","adapter_catalog":"x-octendra-adapter-catalog-sha256",
            "adapter_skill":"x-octendra-adapter-skill-revision","host_catalog":"x-octendra-host-catalog-sha256",
            "host_tools":"x-octendra-host-tools","host_report_optional":true,
            "file_parameter":"Resolve the host file reference into the catalog's file object. Never inline file bytes."},
        "boundary":"Adapter-sent catalog is not proof of final host exposure. No final host report means unknown."})
}
pub fn verify(value: &Value) -> Result<()> {
    ensure!(
        value == &bundle(),
        "adapter_contract_drift: regenerate from this Gateway catalog and Skill bundle"
    );
    Ok(())
}
pub fn export(path: &Path, check: bool) -> Result<()> {
    if check {
        return verify(&serde_json::from_slice(&std::fs::read(path)?)?);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(&bundle())?)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
/// Controlled adapters attach this on every upstream request. The final host
/// report is deliberately not manufactured from the adapter's own catalog.
pub fn adapter_headers(p: &Principal) -> HeaderMap {
    catalog_headers(&discoverable_tools(p))
}
/// Catalog evidence is computed from discovery, never inferred OAuth grants.
pub fn catalog_headers(catalog: &[Tool]) -> HeaderMap {
    let mut h = HeaderMap::new();
    let sha = hash(serde_json::to_vec(catalog).expect("filtered catalog"));
    h.insert(
        "x-octendra-adapter-catalog-sha256",
        HeaderValue::from_str(&sha).expect("hash"),
    );
    h.insert(
        "x-octendra-adapter-skill-revision",
        HeaderValue::from_str(capabilities::embedded_skill_revision()).expect("hash"),
    );
    h.insert(
        "x-octendra-request-id",
        HeaderValue::from_str(&format!("req_{}", uuid::Uuid::new_v4().simple())).expect("request"),
    );
    h
}
fn digest_header<'a>(h: &'a HeaderMap, key: &str) -> Option<&'a str> {
    h.get(key)?.to_str().ok().filter(|v| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
pub fn exposure(h: &HeaderMap, p: &Principal) -> Value {
    let expected = hash(serde_json::to_vec(&discoverable_tools(p)).expect("catalog"));
    let authorized = hash(serde_json::to_vec(&authorized_tools(p)).expect("authorized catalog"));
    let adapter = digest_header(h, "x-octendra-adapter-catalog-sha256");
    let skill = digest_header(h, "x-octendra-adapter-skill-revision");
    let host = digest_header(h, "x-octendra-host-catalog-sha256");
    let names = h
        .get("x-octendra-host-tools")
        .and_then(|v| v.to_str().ok())
        .filter(|s| s.len() <= 8192)
        .and_then(|s| {
            let names: BTreeSet<_> = s.split(',').filter(|s| !s.is_empty()).collect();
            names
                .iter()
                .all(|s| {
                    s.len() <= 128
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                })
                .then_some(names)
        });
    let missing: Option<Vec<_>> = names.as_ref().map(|names| {
        discoverable_tools(p)
            .into_iter()
            .filter(|t| !names.contains(t.name.as_ref()))
            .map(|t| t.name.to_string())
            .collect()
    });
    let adapter_status = match adapter {
        None => "unknown_not_reported",
        Some(v) if v == expected && skill == Some(capabilities::embedded_skill_revision()) => {
            "reported_match"
        }
        Some(_) => "reported_mismatch",
    };
    let host_status = match host {
        None => "unknown_not_reported",
        Some(v) if v == expected && missing.as_ref().is_some_and(|names| !names.is_empty()) => {
            "reported_inconsistent"
        }
        Some(v) if v == expected => "reported_match",
        Some(_) => "reported_mismatch",
    };
    json!({"connector_catalog":{"status":adapter_status,"reported_tools_sha256":adapter,"reported_skill_revision":skill,"expected_discoverable_tools_sha256":expected,"expected_authorized_tools_sha256":authorized,
        "comparison_basis":"discoverable_tools","authorization_evidence":false,"hash_semantics_revision":2,
        "evidence_source":"authenticated request headers, not the server catalog"},
        "current_session_exposure":{"comparison_basis":"discoverable_tools","authorization_evidence":false,"status":host_status,"reported_tools_sha256":host,"missing_tools":missing,
            "host_catalog_hash_supplied":host.is_some(),"reason":if host.is_none(){"final host exposure not reported; do not infer read-only or full capability"}else{"caller-reported final host catalog; does not grant permissions"}},
        "boundary":"Reports are per authenticated request and are never borrowed from another client or session."})
}
#[cfg(test)]
mod tests {
    use super::*;
    fn p() -> Principal {
        Principal {
            owner: "fixture".into(),
            scopes: vec![
                "code:read".into(),
                "code:write".into(),
                "terminal:exec".into(),
            ],
        }
    }
    #[test]
    fn generated_adapter_cannot_drift_from_catalog_or_skill() {
        let value = bundle();
        verify(&value).unwrap();
        for key in [
            "server_tools_sha256",
            "adapter_tools_sha256",
            "skill_revision",
        ] {
            let mut changed = value.clone();
            changed[key] = json!("0".repeat(64));
            assert!(verify(&changed).is_err());
        }
        let mut changed = value;
        changed["tools"].as_array_mut().unwrap().pop();
        assert!(verify(&changed).is_err());
    }
    #[test]
    fn contradictory_host_hash_and_names_are_not_reported_as_a_match() {
        let p = p();
        let mut h = adapter_headers(&p);
        let sha = hash(serde_json::to_vec(&discoverable_tools(&p)).unwrap());
        h.insert(
            "x-octendra-host-catalog-sha256",
            HeaderValue::from_str(&sha).unwrap(),
        );
        h.insert(
            "x-octendra-host-tools",
            HeaderValue::from_static("devices_list"),
        );
        assert_eq!(
            exposure(&h, &p)["current_session_exposure"]["status"],
            "reported_inconsistent"
        );
    }

    #[test]
    fn adapter_report_is_not_a_final_host_report() {
        let p = p();
        let h = adapter_headers(&p);
        let e = exposure(&h, &p);
        assert_eq!(e["connector_catalog"]["status"], "reported_match");
        assert_eq!(
            e["current_session_exposure"]["status"],
            "unknown_not_reported"
        );
        assert!(!h.contains_key("x-octendra-host-catalog-sha256"));
        assert!(crate::receipts::valid_request_id(
            h["x-octendra-request-id"].to_str().unwrap()
        ));
    }
    #[test]
    fn host_reports_missing_tools_without_changing_authorization() {
        let p = p();
        let mut h = adapter_headers(&p);
        h.insert(
            "x-octendra-host-catalog-sha256",
            HeaderValue::from_static(
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
        );
        h.insert(
            "x-octendra-host-tools",
            HeaderValue::from_static("devices_list,code_read"),
        );
        let e = exposure(&h, &p);
        assert_eq!(e["current_session_exposure"]["status"], "reported_mismatch");
        assert!(
            e["current_session_exposure"]["missing_tools"]
                .as_array()
                .unwrap()
                .contains(&json!("terminal_exec"))
        );
        assert_eq!(
            exposure(&HeaderMap::new(), &p)["current_session_exposure"]["status"],
            "unknown_not_reported"
        );
    }
    #[test]
    fn discoverable_and_authorized_hashes_are_distinct_for_read_only_scopes() {
        let p = Principal {
            owner: "fixture".into(),
            scopes: vec!["code:read".into()],
        };
        let before = p.scopes.clone();
        let e = exposure(&adapter_headers(&p), &p);
        let c = &e["connector_catalog"];
        assert_eq!(c["status"], "reported_match");
        assert_eq!(c["comparison_basis"], "discoverable_tools");
        assert_eq!(c["hash_semantics_revision"], 2);
        assert_eq!(c["authorization_evidence"], false);
        assert_eq!(
            c["expected_discoverable_tools_sha256"],
            hash(serde_json::to_vec(&discoverable_tools(&p)).unwrap())
        );
        assert_eq!(
            c["expected_authorized_tools_sha256"],
            hash(serde_json::to_vec(&authorized_tools(&p)).unwrap())
        );
        assert_ne!(
            c["expected_discoverable_tools_sha256"],
            c["expected_authorized_tools_sha256"]
        );
        assert!(
            discoverable_tools(&p)
                .iter()
                .any(|t| t.name == "desktop_worker")
        );
        assert!(
            authorized_tools(&p)
                .iter()
                .any(|t| t.name.starts_with("desktop_"))
        );
        assert!(tools::has_required_scopes(&p, "desktop_worker"));
        assert!(
            !authorized_tools(&p)
                .iter()
                .any(|t| t.name == "desktop_observer" || t.name == "desktop_frame")
        );
        assert_eq!(before, p.scopes);
    }
    #[test]
    fn authorized_subset_hash_is_not_accepted_as_discovery_evidence() {
        let p = Principal {
            owner: "fixture".into(),
            scopes: vec!["code:read".into()],
        };
        let sha = hash(serde_json::to_vec(&authorized_tools(&p)).unwrap());
        let mut h = adapter_headers(&p);
        for key in [
            "x-octendra-adapter-catalog-sha256",
            "x-octendra-host-catalog-sha256",
        ] {
            h.insert(key, HeaderValue::from_str(&sha).unwrap());
        }
        let e = exposure(&h, &p);
        assert_eq!(e["connector_catalog"]["status"], "reported_mismatch");
        assert_eq!(e["current_session_exposure"]["status"], "reported_mismatch");
    }
    #[test]
    fn full_scope_hashes_keep_existing_catalog_headers_compatible() {
        let mut p = p();
        p.scopes
            .extend(["desktop:observe".into(), "desktop:input".into()]);
        let h = adapter_headers(&p);
        let e = exposure(&h, &p);
        let c = &e["connector_catalog"];
        assert_eq!(c["status"], "reported_match");
        assert_eq!(
            c["reported_tools_sha256"],
            c["expected_discoverable_tools_sha256"]
        );
        assert_eq!(
            c["expected_authorized_tools_sha256"],
            c["expected_discoverable_tools_sha256"]
        );
        assert_eq!(
            h["x-octendra-adapter-skill-revision"],
            capabilities::embedded_skill_revision()
        );
        assert_eq!(
            e["current_session_exposure"]["status"],
            "unknown_not_reported"
        );
    }
}
