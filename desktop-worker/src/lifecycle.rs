//! Fixed, bounded metadata on the private stderr pipe; never generic log text.
use uuid::Uuid;
pub fn phase(request: Option<Uuid>, phase: &'static str) {
    let Some(request_id) = request else { return };
    let Ok(mut bytes) = serde_json::to_vec(&serde_json::json!({
        "protocol":1,"request_id":request_id,"pid":std::process::id(),"phase":phase
    })) else {
        return;
    };
    if bytes.len() > 512 {
        return;
    }
    bytes.push(b'\n');
    // main made this pipe nonblocking. One write below PIPE_BUF; dropped
    // diagnostics cannot fabricate a response or hold shutdown open.
    unsafe {
        nix::libc::write(2, bytes.as_ptr().cast(), bytes.len());
    }
}
pub async fn shutdown<
    F: std::future::Future<Output = Result<(), remote_hosts_desktop_protocol::ErrorCode>>,
>(
    request: Option<Uuid>,
    future: F,
) -> Result<(), remote_hosts_desktop_protocol::ErrorCode> {
    phase(request, "shutdown_started");
    let result = match tokio::time::timeout(std::time::Duration::from_secs(2), future).await {
        Ok(result) => result,
        Err(_) => Err(remote_hosts_desktop_protocol::ErrorCode::OutcomeUnknown),
    };
    phase(
        request,
        if result.is_ok() {
            "shutdown_confirmed"
        } else {
            "shutdown_unknown"
        },
    );
    result
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn shutdown_timeout_cannot_report_confirmation() {
        let start = std::time::Instant::now();
        assert_eq!(
            super::shutdown(None, std::future::pending())
                .await
                .unwrap_err(),
            remote_hosts_desktop_protocol::ErrorCode::OutcomeUnknown
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }
    #[tokio::test]
    async fn shutdown_failure_is_preserved() {
        let result = super::shutdown(None, async {
            Err(remote_hosts_desktop_protocol::ErrorCode::OutcomeUnknown)
        })
        .await;
        assert!(result.is_err());
        assert!(super::shutdown(None, async { Ok(()) }).await.is_ok());
    }
}
