//! Server protection policy for agent-driven remote execution.

use remote_hosts_domain::{AgentHint, EntityState, StateReasonCode};
use serde::{Deserialize, Serialize};

/// Static protection policy attached to a host or access path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServerProtectionPolicy {
    /// Maximum new SSH handshakes per 10 minutes.
    pub max_new_ssh_handshakes_per_10_min: u32,
    /// Maximum parallel exec channels per host.
    pub max_parallel_exec_channels_per_host: u32,
    /// Maximum parallel probe jobs per host.
    pub max_parallel_probe_jobs_per_host: u32,
    /// Maximum persistent PTYs per host.
    pub max_persistent_ptys_per_host: u32,
    /// Maximum active logical workspaces per host.
    #[serde(default = "default_max_active_workspaces_per_host")]
    pub max_active_workspaces_per_host: u32,
    /// Maximum queued operations per host.
    pub max_operation_queue_depth_per_host: u32,
    /// Default command timeout in seconds.
    pub default_exec_timeout_seconds: u64,
    /// Default output limit in bytes.
    pub default_output_limit_bytes: usize,
    /// Maximum one-shot input payload for persistent PTY sessions.
    pub max_pty_input_bytes: usize,
    /// Cooldown when overload is detected.
    pub overload_cooldown_seconds: u64,
}

impl ServerProtectionPolicy {
    /// Unrestricted channel baseline: platform entrypoints own access control.
    pub fn production_default() -> Self {
        Self {
            max_new_ssh_handshakes_per_10_min: u32::MAX,
            max_parallel_exec_channels_per_host: u32::MAX,
            max_parallel_probe_jobs_per_host: u32::MAX,
            max_persistent_ptys_per_host: u32::MAX,
            max_active_workspaces_per_host: default_max_active_workspaces_per_host(),
            max_operation_queue_depth_per_host: u32::MAX,
            default_exec_timeout_seconds: 3_600,
            default_output_limit_bytes: 16 * 1024 * 1024,
            max_pty_input_bytes: 1024 * 1024,
            overload_cooldown_seconds: 0,
        }
    }

    /// Always allows execution; built-in rate/capacity throttling is disabled so the
    /// service acts as a direct, unrestricted transport channel.
    pub fn decide(
        &self,
        _queued_operations: u32,
        _active_exec_channels: u32,
        _active_probe_jobs: u32,
        _active_ptys: u32,
        _overload_cooldown_active: bool,
    ) -> ProtectionDecision {
        ProtectionDecision {
            allowed: true,
            state: EntityState::Healthy,
            reason_code: StateReasonCode::None,
            agent_hint: None,
            retry_after_seconds: None,
            human_message: "request allowed".to_owned(),
        }
    }

    /// Always allows logical workspace allocation without artificial capacity gates.
    pub fn decide_workspace_creation(&self, _active_workspaces: u32) -> ProtectionDecision {
        ProtectionDecision {
            allowed: true,
            state: EntityState::Healthy,
            reason_code: StateReasonCode::None,
            agent_hint: None,
            retry_after_seconds: None,
            human_message: "request allowed".to_owned(),
        }
    }
}

const fn default_max_active_workspaces_per_host() -> u32 {
    u32::MAX
}

impl Default for ServerProtectionPolicy {
    fn default() -> Self {
        Self::production_default()
    }
}

/// Result of a protection policy check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProtectionDecision {
    /// Whether execution is allowed.
    pub allowed: bool,
    /// Resulting state.
    pub state: EntityState,
    /// Reason code.
    pub reason_code: StateReasonCode,
    /// Agent hint.
    pub agent_hint: Option<AgentHint>,
    /// Retry delay in seconds.
    pub retry_after_seconds: Option<u64>,
    /// Human-readable message.
    pub human_message: String,
}

#[cfg(test)]
mod tests {
    use remote_hosts_domain::EntityState;

    use super::ServerProtectionPolicy;

    #[test]
    fn default_policy_allows_all_channel_activity_without_artificial_throttles() {
        let policy = ServerProtectionPolicy::default();
        let decision = policy.decide(100, 100, 10, 50, true);
        assert!(decision.allowed);
        assert_eq!(decision.state, EntityState::Healthy);
        assert!(policy.decide_workspace_creation(1000).allowed);
    }
}
