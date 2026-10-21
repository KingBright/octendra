//! Fixed, installed instance binding for Linux self-upgrade. Never infer a
//! maintenance service or accept a service/path from an upgrade request.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub schema: u32,
    pub role: String,
    pub service: String,
    pub executable: PathBuf,
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub definition: PathBuf,
    pub definition_sha256: String,
}

pub(crate) fn identity_path(config: &Path) -> Result<PathBuf> {
    Ok(config
        .parent()
        .and_then(Path::parent)
        .context("instance config root missing")?
        .join("bin/upgrade-identity.json"))
}

fn exact_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "instance path must be absolute");
    ensure!(
        path.components().all(|c| matches!(
            c,
            Component::RootDir | Component::Prefix(_) | Component::Normal(_)
        )),
        "instance path is not normalized"
    );
    for part in path.ancestors() {
        ensure!(
            !std::fs::symlink_metadata(part)?.file_type().is_symlink(),
            "instance path contains a symlink"
        );
    }
    Ok(())
}

fn new_namespace(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "octendra")
        && !path.components().any(|c| {
            let name = c
                .as_os_str()
                .to_string_lossy()
                .to_ascii_lowercase()
                .replace(['-', '_', ' '], "");
            name == "octendracode"
                || name.starts_with("remotehosts")
                || name.starts_with("remoteplay")
        })
}

impl Identity {
    fn validate(
        &self,
        role: &str,
        config: &Path,
        state: &Path,
        exe: &Path,
        definition: &Path,
    ) -> Result<()> {
        ensure!(
            self.schema == 1 && self.role == role && matches!(role, "gateway" | "agent"),
            "instance role/schema mismatch"
        );
        ensure!(
            self.service == format!("octendra-{role}.service"),
            "instance service is not the new role identity"
        );
        ensure!(
            self.config == config
                && self.state_dir == state
                && self.executable == exe
                && self.definition == definition,
            "instance path binding mismatch"
        );
        let root = config
            .parent()
            .and_then(Path::parent)
            .context("instance root missing")?;
        ensure!(
            config == root.join(format!("config/{role}.json"))
                && exe == root.join("bin/octendra-code")
                && new_namespace(root)
                && new_namespace(state),
            "instance paths are outside the new namespace"
        );
        for p in [config, state, exe, definition] {
            exact_path(p)?;
        }
        ensure!(
            self.definition_sha256.len() == 64
                && crate::hash(std::fs::read(definition)?) == self.definition_sha256,
            "installed service definition changed"
        );
        Ok(())
    }

    pub(crate) fn verify_process(&self, pid: u32) -> Result<()> {
        ensure!(pid > 1, "instance process identity missing");
        let proc = PathBuf::from(format!("/proc/{pid}"));
        ensure!(
            std::fs::read_link(proc.join("exe"))? == self.executable,
            "running instance executable mismatch"
        );
        self.verify_cgroup(&std::fs::read_to_string(proc.join("cgroup"))?)
    }

    fn verify_cgroup(&self, cgroup: &str) -> Result<()> {
        ensure!(
            cgroup.lines().any(|line| line
                .splitn(3, ':')
                .nth(2)
                .is_some_and(|path| path.split('/').any(|part| part == self.service))),
            "process is not in its bound service"
        );
        Ok(())
    }
}

pub(crate) fn load(role: &str, config: &Path, state: &Path, exe: &Path) -> Result<Identity> {
    let path = identity_path(config)?;
    exact_path(&path)?;
    let identity: Identity = serde_json::from_slice(&std::fs::read(path)?)
        .context("installed upgrade identity missing or invalid")?;
    let directory = if role == "gateway" {
        PathBuf::from("/etc/systemd/system")
    } else {
        PathBuf::from(std::env::var_os("HOME").context("agent home missing")?)
            .join(".config/systemd/user")
    };
    identity.validate(
        role,
        config,
        state,
        exe,
        &directory.join(format!("octendra-{role}.service")),
    )?;
    Ok(identity)
}

pub(crate) fn supports_upgrade_manifest(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| {
            v.get("instance_upgrade_protocol")
                .and_then(serde_json::Value::as_u64)
        })
        == Some(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parallel_upgrade_identity_rejects_old_service_paths_and_injection() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        for role in ["gateway", "agent"] {
            let root = base.join("octendra").join(role);
            let config = root.join(format!("config/{role}.json"));
            let exe = root.join("bin/octendra-code");
            let state = root.join("state");
            let definition = base.join(format!("octendra-{role}.service"));
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(&config, b"private fixture unchanged").unwrap();
            std::fs::write(&exe, b"fixture binary").unwrap();
            std::fs::write(&definition, b"fixture unit").unwrap();
            let identity = Identity {
                schema: 1,
                role: role.into(),
                service: format!("octendra-{role}.service"),
                executable: exe.clone(),
                config: config.clone(),
                state_dir: state.clone(),
                definition: definition.clone(),
                definition_sha256: crate::hash(b"fixture unit"),
            };
            let check = |i: &Identity| i.validate(role, &config, &state, &exe, &definition);
            check(&identity).unwrap();
            identity
                .verify_cgroup(&format!("0::/system.slice/{}\n", identity.service))
                .unwrap();
            assert!(
                identity
                    .verify_cgroup(&format!("0::/system.slice/octendra-code-{role}.service\n"))
                    .is_err()
            );
            for service in [
                format!("octendra-code-{role}.service"),
                "--all".into(),
                "other.service; id".into(),
                "../octendra-agent.service".into(),
            ] {
                let mut bad = identity.clone();
                bad.service = service;
                assert!(check(&bad).is_err());
            }
            for path in [&config, &exe, &state] {
                let mut bad = identity.clone();
                if path == &config {
                    bad.config = base.join("legacy.json");
                } else if path == &exe {
                    bad.executable = base.join("legacy-binary");
                } else {
                    bad.state_dir = base.join("legacy-state");
                }
                assert!(check(&bad).is_err());
            }
            std::fs::write(&definition, b"changed unit").unwrap();
            assert!(check(&identity).is_err());
            assert_eq!(
                std::fs::read(&config).unwrap(),
                b"private fixture unchanged"
            );
        }
    }
    #[test]
    fn old_or_malformed_candidate_cannot_restore_maintenance_targets() {
        assert!(supports_upgrade_manifest(
            br#"{"instance_upgrade_protocol":1}"#
        ));
        for bytes in [
            b"{}".as_slice(),
            br#"{"instance_upgrade_protocol":true}"#,
            br#"{"instance_upgrade_protocol":2}"#,
            b"not-json",
        ] {
            assert!(!supports_upgrade_manifest(bytes));
        }
    }

    #[test]
    fn missing_identity_fails_before_any_write() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("octendra");
        let result = load(
            "agent",
            &root.join("config/agent.json"),
            &root.join("state"),
            &root.join("bin/octendra-code"),
        );
        assert!(result.is_err());
        assert!(!root.exists());
    }
}
