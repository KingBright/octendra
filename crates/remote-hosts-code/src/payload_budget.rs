//! Budgets apply to encoded wire bytes, never characters or unescaped source.
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::json;

pub(crate) const POLL_BYTES: usize = 512 * 1024;
pub(crate) const COMMAND_BYTES: usize = 64 * 1024;

pub(crate) fn poll_bytes(job: &impl Serialize) -> Result<usize> {
    Ok(serde_json::to_vec(&json!({"job": job}))?.len())
}
pub(crate) fn admit(job: &impl Serialize) -> Result<()> {
    ensure!(
        poll_bytes(job)? <= POLL_BYTES,
        "payload_too_large: encoded poll envelope exceeds byte budget"
    );
    Ok(())
}
pub(crate) fn command(command: &str) -> Result<()> {
    ensure!(
        command.len() <= COMMAND_BYTES,
        "payload_too_large: terminal command exceeds byte budget"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_utf8_and_json_escape_bytes_are_counted() {
        let source = "中文\n\t\"\\";
        let job = json!({"content":source});
        let actual = serde_json::to_vec(&json!({"job":job})).unwrap();
        assert_eq!(poll_bytes(&job).unwrap(), actual.len());
        assert!(actual.len() > source.chars().count());
        assert!(actual.len() > source.len());
    }
    #[test]
    fn poll_exact_boundary_and_one_byte_over() {
        let empty = json!({"content":""});
        let overhead = poll_bytes(&empty).unwrap();
        let at = json!({"content":"x".repeat(POLL_BYTES-overhead)});
        assert_eq!(poll_bytes(&at).unwrap(), POLL_BYTES);
        admit(&at).unwrap();
        let over = json!({"content":"x".repeat(POLL_BYTES-overhead+1)});
        assert!(
            admit(&over)
                .unwrap_err()
                .to_string()
                .starts_with("payload_too_large:")
        );
    }
    #[test]
    fn terminal_exact_boundary_counts_utf8_bytes() {
        command(&"x".repeat(COMMAND_BYTES)).unwrap();
        assert!(command(&"x".repeat(COMMAND_BYTES + 1)).is_err());
        command(&"é".repeat(COMMAND_BYTES / 2)).unwrap();
        assert!(command(&"é".repeat(COMMAND_BYTES / 2 + 1)).is_err());
    }
}
