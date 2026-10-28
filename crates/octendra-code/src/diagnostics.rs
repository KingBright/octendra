//! Stable recovery guidance. Never serialize raw commands, URLs, or OS error chains.
use crate::now;
use serde_json::{Value, json};
pub(crate) fn is_code(message: &str, code: &str) -> bool {
    message == code
        || message
            .strip_prefix(code)
            .is_some_and(|rest| rest.starts_with(':'))
}
pub(crate) fn outcome_unknown(value: &Value) -> bool {
    value["error"] == "outcome_unknown"
        || (value.get("error").is_some()
            && (value["outcome"] == "unknown"
                || value["execution_state"] == "unknown"
                || value["state"] == "outcome_unknown"))
}
pub(crate) fn error_from_source(
    tool: &str,
    source: &anyhow::Error,
    operation: Option<&str>,
    stage: &str,
) -> Value {
    if source
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    {
        error(tool, "local_permission_denied", operation, stage)
    } else {
        error(tool, &source.to_string(), operation, stage)
    }
}
pub(crate) fn error(tool: &str, message: &str, operation: Option<&str>, stage: &str) -> Value {
    let task_rejection = [
        "task_authorization_missing",
        "task_authorization_version_required",
        "task_authorization_version_changed",
        "task_authorization_revoked",
        "task_authorization_expired",
        "task_authorization_expiry_required",
        "task_scope_denied",
        "task_device_denied",
    ]
    .into_iter()
    .find(|code| message == *code);
    let (code, recovery, outcome) = if let Some(code) = task_rejection {
        (
            code,
            "establish_owner_task_authorization_then_resume_original",
            "not_executed",
        )
    } else if matches!(
        message,
        "outcome_not_unknown" | "outcome_evidence_unavailable" | "outcome_resolution_conflict"
    ) {
        (
            message,
            "inspect_original_operation_without_reexecution",
            "not_executed",
        )
    } else if message.starts_with("insufficient_scope") {
        (
            "account_scope_denied",
            "check_current_account_scopes",
            "not_executed",
        )
    } else if message.starts_with("device_scope_denied") {
        (
            "device_scope_denied",
            "check_selected_device_scopes",
            "not_executed",
        )
    } else if message.starts_with("state_only_tool_denied") {
        (
            "state_only_tool_denied",
            "use_explicit_state_directory_read",
            "not_executed",
        )
    } else if message.starts_with("workspace_review_denied") {
        (
            "workspace_review_denied",
            "use_in_root_repository_or_bounded_file_reads",
            "not_executed",
        )
    } else if message.starts_with("workspace_root_denied") {
        (
            "workspace_root_denied",
            "select_configured_workspace_root",
            "not_executed",
        )
    } else if message == "device file writes disabled" {
        (
            "local_write_disabled",
            "request_owner_local_policy_review",
            "not_executed",
        )
    } else if message == "device terminal execution disabled" {
        (
            "local_exec_disabled",
            "request_owner_local_policy_review",
            "not_executed",
        )
    } else if message.starts_with("platform_authorization_required")
        || message == "authorization_required"
    {
        (
            "platform_authorization_required",
            "complete_platform_authorization_directly",
            "not_executed_or_partial",
        )
    } else if message.starts_with("source_address_policy_rejected") {
        (
            "source_address_policy_rejected",
            "inspect_original_source_policy_without_bypass",
            "not_executed_or_paused",
        )
    } else if message.contains("device_draining") {
        (
            "device_draining",
            "observe_original_updater",
            "not_executed",
        )
    } else if is_code(message, "invalid_arguments") {
        ("invalid_arguments", "correct_arguments", "not_executed")
    } else if message.contains("source_authorization_required") {
        (
            "source_authorization_required",
            "refresh_original_file_authorization_with_transfer_resume",
            "not_executed_or_paused",
        )
    } else if message.contains("file_source_rejected") {
        (
            "file_source_rejected",
            "inspect_original_file_identity_and_authorization",
            "not_executed_or_paused",
        )
    } else if message.contains("storage_capacity_insufficient")
        || message.contains("gateway_storage_limit")
        || message.contains("transfer_storage_limit")
    {
        (
            "storage_capacity_insufficient",
            "free_storage_or_request_a_smaller_transfer",
            "not_executed_or_paused",
        )
    } else if message.contains("device_feature_unavailable") {
        (
            "device_feature_unavailable",
            "upgrade_selected_device_and_refresh_capabilities",
            "not_executed",
        )
    } else if message.contains("change_set_unavailable")
        || message.contains("change_set workspace identity conflict")
    {
        (
            "change_set_unavailable",
            "inspect_workspace_change_sets_and_original_edit",
            "not_executed",
        )
    } else if message.contains("gc_preview_changed") {
        (
            "gc_preview_changed",
            "run_workspace_gc_preview_again_before_apply",
            "not_executed",
        )
    } else if message.contains("operation_unavailable") || message.contains("operation not found") {
        (
            "operation_unavailable",
            "inspect_original_operation_identifiers_without_reexecution",
            "not_executed",
        )
    } else if message == "submission_closed_not_enqueued" {
        (
            "submission_closed_not_enqueued",
            "inspect_submission_receipt_then_use_a_deliberate_new_key",
            "not_executed",
        )
    } else if message.contains("version_conflict") || message.contains("unique match") {
        (
            "version_conflict",
            "read_current_version_and_merge",
            "not_executed_or_partial",
        )
    } else if message.contains("idempotency_conflict") || message.contains("fingerprint conflict") {
        (
            "idempotency_conflict",
            "observe_original_operation",
            "not_executed",
        )
    } else if message.contains("device_offline") {
        (
            "device_offline",
            "reconnect_selected_device",
            "not_executed",
        )
    } else if message.contains("cursor_expired") || message.contains("cursor") {
        (
            "invalid_or_expired_cursor",
            "refresh_scoped_snapshot",
            "not_executed",
        )
    } else if is_code(message, "local_permission_denied") {
        (
            "local_permission_denied",
            "inspect_selected_device_local_file_or_os_permission",
            "not_executed_or_partial",
        )
    } else if (message.starts_with("octendra_authority_") || message.starts_with("octendra_task_"))
        && message.len() <= 96
        && message.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    {
        (
            message,
            if message == "octendra_task_authority_expired" {
                "reopen_original_desktop_session_and_capture_fresh_frame"
            } else {
                "inspect_existing_desktop_approval_and_refresh_capabilities"
            },
            "not_executed",
        )
    } else if is_code(message, "access_denied")
        || is_code(message, "authorization revoked")
        || message == "outside allowed directories"
    {
        (
            "access_denied",
            "check_original_device_and_permissions",
            "not_executed_or_partial",
        )
    } else if message.contains("workspace not found") {
        (
            "workspace_unavailable",
            "open_original_device_workspace",
            "not_executed",
        )
    } else if message.starts_with("payload_too_large:") || message == "command too large" {
        (
            "payload_too_large",
            "use_automatic_content_transfer_or_shorten_command",
            "not_executed",
        )
    } else if message.contains("budget")
        || message.contains("too large")
        || message.contains("limit")
    {
        (
            "capacity_limit",
            "narrow_request_and_observe_original",
            "unknown",
        )
    } else {
        (
            "execution_failed",
            "observe_original_and_inspect_scoped_evidence",
            "unknown",
        )
    };
    let (failure_boundary, last_confirmed_stage) = match stage {
        "gateway_dispatch" => ("gateway", "gateway_received_request"),
        "agent_execute" => ("device", "device_received_operation"),
        "host" => ("host", "host_observed_request"),
        _ => ("connector", "connector_observed_request"),
    };
    let execution_state = match outcome {
        "not_executed" => "not_started",
        "not_executed_or_paused" => "not_started_or_paused",
        "not_executed_or_partial" => "not_started_or_partial",
        _ => "unknown",
    };
    let retry_policy = match code {
        "task_authorization_missing"
        | "task_authorization_version_required"
        | "task_authorization_version_changed"
        | "task_authorization_revoked"
        | "task_authorization_expired"
        | "task_authorization_expiry_required"
        | "task_scope_denied"
        | "task_device_denied" => "explicit_resume_only_after_authorization_changes",
        "account_scope_denied"
        | "device_scope_denied"
        | "local_write_disabled"
        | "local_exec_disabled"
        | "workspace_review_denied"
        | "workspace_root_denied"
        | "state_only_tool_denied"
        | "platform_authorization_required"
        | "source_address_policy_rejected" => "do_not_replay_or_expand_authority",
        "invalid_arguments" => "retry_only_after_correcting_arguments",
        "device_offline" | "device_feature_unavailable" | "storage_capacity_insufficient" => {
            "retry_only_after_reported_condition_changes"
        }
        "idempotency_conflict" | "operation_unavailable" => "do_not_replay_observe_original",
        "version_conflict" | "change_set_unavailable" | "gc_preview_changed" => {
            "do_not_replay_reconcile_state_first"
        }
        _ if outcome == "not_executed" => "safe_only_after_confirming_precondition",
        _ => "do_not_replay_until_original_outcome_is_observed",
    };
    let user_action = match code {
        "device_offline" => "reconnect_selected_device",
        "source_authorization_required" => "refresh_original_file_authorization",
        "storage_capacity_insufficient" => "free_storage_or_reduce_request",
        "invalid_arguments" => "correct_arguments",
        "access_denied" => "check_account_and_device_permissions",
        "local_permission_denied" => "inspect_selected_device_local_file_or_os_permission",
        "task_authorization_missing"
        | "task_authorization_version_required"
        | "task_authorization_version_changed"
        | "task_authorization_revoked"
        | "task_authorization_expired"
        | "task_authorization_expiry_required"
        | "task_scope_denied"
        | "task_device_denied" => "review_owner_task_authorization",
        "account_scope_denied" => "check_current_account_scopes",
        "device_scope_denied" => "check_selected_device_scopes",
        "local_write_disabled" | "local_exec_disabled" => "request_owner_local_policy_review",
        "platform_authorization_required" => "complete_platform_authorization_directly",
        "source_address_policy_rejected" => "inspect_original_source_policy_without_bypass",
        _ => "none_until_observation_or_recovery_guidance_requires_it",
    };
    json!({"error":"tool_failed","error_code":code,"message":code,
        "stage":stage,"failure_boundary":failure_boundary,"last_confirmed_stage":last_confirmed_stage,
        "execution_state":execution_state,"execution_started":execution_state != "not_started","outcome":outcome,"recovery_action":recovery,
        "retry_policy":retry_policy,"user_action":user_action,"observed_at":now(),
        "evidence":{"error_code":code,"sanitized":true},
        "automatic_replay_safe":false,"operation_id":operation,"tool":tool,"diagnostic_protocol":2})
}
pub(crate) fn error_with_request(
    tool: &str,
    message: &str,
    request_id: &str,
    operation: Option<&str>,
    stage: &str,
) -> Value {
    let mut value = error(tool, message, operation, stage);
    value["request_id"] = json!(request_id);
    value
}
#[cfg(test)]
mod tests {
    #[test]
    fn permission_words_in_unrelated_errors_do_not_claim_authorization_failure() {
        for message in [
            "permission snapshot channel closed",
            "scope metadata could not be loaded",
            "authorized transport disconnected",
        ] {
            let value = super::error("desktop_worker", message, None, "agent_execute");
            assert_eq!(value["error_code"], "execution_failed", "{message}");
            assert_eq!(value["outcome"], "unknown");
            assert_ne!(value["user_action"], "check_account_and_device_permissions");
        }
        let denied = super::error("code_read", "insufficient_scope", None, "gateway_dispatch");
        assert_eq!(denied["error_code"], "account_scope_denied");
    }

    #[test]
    fn local_permission_failure_uses_the_io_source_without_serializing_its_context() {
        let source = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("private path /secret/device-file");
        let value =
            super::error_from_source("code_read", &source, Some("original"), "agent_execute");
        assert_eq!(value["error_code"], "local_permission_denied");
        assert_eq!(value["failure_boundary"], "device");
        assert!(!value.to_string().contains("secret"));
        assert_ne!(value["user_action"], "check_account_and_device_permissions");
    }

    #[test]
    fn desktop_authority_codes_keep_their_specific_recovery_boundary() {
        for message in [
            "octendra_authority_gui_executable_changed",
            "octendra_task_agent_changed",
            "octendra_authority_input_platform_mismatch",
        ] {
            let value = super::error("desktop_worker", message, None, "agent_execute");
            assert_eq!(value["error_code"], message);
            assert_eq!(value["execution_state"], "not_started");
            assert_eq!(
                value["recovery_action"],
                "inspect_existing_desktop_approval_and_refresh_capabilities"
            );
        }
        let expired = super::error(
            "desktop_worker",
            "octendra_task_authority_expired",
            None,
            "agent_execute",
        );
        assert_eq!(
            expired["recovery_action"],
            "reopen_original_desktop_session_and_capture_fresh_frame"
        );
        let unsafe_message = super::error(
            "desktop_worker",
            "octendra_task_failure: https://private/?token=secret",
            None,
            "agent_execute",
        );
        assert_eq!(unsafe_message["error_code"], "execution_failed");
        assert!(!unsafe_message.to_string().contains("secret"));
    }

    #[test]
    fn oversized_payload_is_rejected_before_execution() {
        for stage in ["gateway_dispatch", "agent_execute"] {
            let value = super::error(
                "terminal_exec",
                "payload_too_large: terminal command exceeds byte budget",
                None,
                stage,
            );
            assert_eq!(value["error_code"], "payload_too_large");
            assert_eq!(value["execution_state"], "not_started");
            assert_eq!(value["outcome"], "not_executed");
        }
        let legacy = super::error(
            "terminal_exec",
            "command too large",
            Some("original"),
            "agent_execute",
        );
        assert_eq!(legacy["error_code"], "payload_too_large");
        assert_eq!(legacy["execution_state"], "not_started");
        let capacity = super::error("code_read", "capacity limit", None, "agent_execute");
        assert_eq!(capacity["execution_state"], "unknown");
    }
    #[test]
    fn errors_never_expose_input_values() {
        let v = super::error(
            "terminal_exec",
            "failure https://host/?token=secret command-password",
            Some("id"),
            "execute",
        );
        let s = v.to_string();
        assert!(!s.contains("secret"));
        assert!(!s.contains("command-password"));
        assert_eq!(v["outcome"], "unknown");
        assert_eq!(v["diagnostic_protocol"], 2);
        assert_eq!(v["failure_boundary"], "connector");
        assert_eq!(v["last_confirmed_stage"], "connector_observed_request");
        assert_eq!(v["execution_state"], "unknown");
        assert_eq!(
            v["retry_policy"],
            "do_not_replay_until_original_outcome_is_observed"
        );
        assert_eq!(v["evidence"]["sanitized"], true);
        assert!(v["observed_at"].as_i64().is_some());
        assert_eq!(v["automatic_replay_safe"], false);
        let embedded = super::error(
            "terminal_exec",
            "command output mentioned invalid_arguments: in middle of text",
            Some("id"),
            "agent_execute",
        );
        assert_eq!(embedded["error_code"], "execution_failed");
        assert_eq!(embedded["outcome"], "unknown");
        assert_eq!(embedded["execution_state"], "unknown");
        for other in [
            "invalid_arguments_other",
            "invalid_arguments_other: different code",
        ] {
            let v = super::error("code_read", other, Some("id"), "agent_execute");
            assert_eq!(v["error_code"], "execution_failed");
            assert_eq!(v["outcome"], "unknown");
            assert_eq!(v["execution_state"], "unknown");
        }
    }
    #[test]
    fn request_receipt_is_distinct_from_remote_operation() {
        let value = super::error_with_request(
            "code_read",
            "device_offline",
            "req_fixture",
            None,
            "gateway_dispatch",
        );
        assert_eq!(value["request_id"], "req_fixture");
        assert!(value["operation_id"].is_null());
        assert_eq!(value["last_confirmed_stage"], "gateway_received_request");
        assert_eq!(value["execution_state"], "not_started");
    }
    #[test]
    fn workflow_recovery_codes_are_specific() {
        let source = super::error(
            "file_upload",
            "source_authorization_required",
            Some("id"),
            "agent_execute",
        );
        assert_eq!(source["error_code"], "source_authorization_required");
        assert_eq!(source["outcome"], "not_executed_or_paused");
        let storage = super::error(
            "file_download",
            "storage_capacity_insufficient",
            Some("id"),
            "agent_execute",
        );
        assert_eq!(
            storage["recovery_action"],
            "free_storage_or_request_a_smaller_transfer"
        );
        let change = super::error(
            "change_resume",
            "change_set_unavailable: journal missing",
            None,
            "agent_execute",
        );
        assert_eq!(change["error_code"], "change_set_unavailable");
        let gc = super::error(
            "workspace_gc",
            "gc_preview_changed: run preview again",
            None,
            "agent_execute",
        );
        assert_eq!(
            gc["recovery_action"],
            "run_workspace_gc_preview_again_before_apply"
        );
        let feature = super::error(
            "workspace_gc",
            "device_feature_unavailable: upgrade",
            None,
            "gateway_dispatch",
        );
        assert_eq!(feature["outcome"], "not_executed");
        assert_eq!(feature["failure_boundary"], "gateway");
        assert_eq!(feature["last_confirmed_stage"], "gateway_received_request");
        assert_eq!(feature["execution_state"], "not_started");
        assert_eq!(
            feature["retry_policy"],
            "retry_only_after_reported_condition_changes"
        );
    }
    #[test]
    fn conflicts_are_actionable_not_retry_promises() {
        let v = super::error(
            "code_apply_edits",
            "version_conflict: path",
            None,
            "execute",
        );
        assert_eq!(v["error_code"], "version_conflict");
        assert_eq!(v["recovery_action"], "read_current_version_and_merge");
    }
}
