//! Existing-pipe caller collection adapter. No listener, grant or execution.
use crate::{
    privilege::{Grant, PlanRequest},
    protocol::{MAX_FRAME, Record, digest},
    windows_pipe::{self, Assessment, CallerToken, Observation, Policy},
};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};

pub struct AssessmentInput<'a> {
    pub policy: &'a Policy,
    pub request: PlanRequest,
    /// Boundary/SCM facts remain unverified; claimed caller fields are discarded.
    pub observation: Observation,
    pub grant: Grant,
    pub previous: Option<&'a Record>,
    pub outcome_unknown: bool,
    pub now: u64,
}
pub(crate) fn assess_with_collector(
    mut input: AssessmentInput<'_>,
    frame: &[u8],
    collect: impl FnOnce() -> Result<CallerToken>,
) -> Result<Assessment> {
    // Discard identity claims before invoking the collector. A failure cannot
    // fall back to JSON metadata, a process token or a previous successful query.
    input.observation.caller = None;
    input.observation.impersonation_succeeded = false;
    input.observation.revert_succeeded = false;
    // One bounded newline-delimited existing PlanRequest, never SID/grant claims.
    ensure!(
        !frame.is_empty() && frame.len() <= MAX_FRAME && frame.last() == Some(&b'\n'),
        "invalid_or_oversized_request_frame"
    );
    let body = &frame[..frame.len() - 1];
    ensure!(
        !body.contains(&b'\n') && !body.contains(&b'\r'),
        "multiple_or_noncanonical_request_frames"
    );
    let decoded: PlanRequest = serde_json::from_slice(body)?;
    decoded.validate()?;
    input.request.validate()?;
    let request_sha256 = digest(&decoded)?;
    ensure!(
        request_sha256 == digest(&input.request)?
            && request_sha256 == input.observation.frame_request_sha256,
        "request_frame_plan_binding_mismatch"
    );
    let frame_sha256 = format!("{:x}", Sha256::digest(frame));
    input.request = decoded;
    let caller = collect()?;
    input.observation.caller = Some(caller);
    input.observation.impersonation_succeeded = true;
    input.observation.revert_succeeded = true;
    let mut assessment = windows_pipe::assess(
        input.policy,
        input.request,
        &input.observation,
        input.grant,
        input.previous,
        input.outcome_unknown,
        input.now,
    )?;
    assessment.binding_sha256 = digest(&(&assessment.binding_sha256, &frame_sha256))?;
    assessment.frame_sha256 = Some(frame_sha256);
    Ok(assessment)
}
/// Reads the kernel identity associated with this connection's last message.
/// Trusted server code must retain/exclusively use the same pipe across its
/// frame read and this synchronous call, supplying those exact frame bytes.
/// This API validates their contents; it cannot attest that bytes came from the handle.
/// No collector/identity override is public.
/// Caller-token collection alone does not authenticate DACL, SCM or frame binding;
/// the result still has native_peer_authenticated=false and execution disabled.
#[cfg(windows)]
pub fn assess_after_read(
    pipe: std::os::windows::io::BorrowedHandle<'_>,
    frame: &[u8],
    input: AssessmentInput<'_>,
) -> Result<Assessment> {
    use windows_pipe::{ImpersonationLevel, Sid, TokenSource};
    assess_with_collector(input, frame, || {
        let identity = octendra_admin_win32::collect_after_read(pipe)?;
        let flags = identity.logon_attributes();
        Ok(CallerToken {
            source: TokenSource::PipeImpersonation,
            level: ImpersonationLevel::Impersonation,
            user_sid: Sid::parse(identity.user_sid())?,
            logon_sid: Sid::parse(identity.logon_sid())?,
            logon_group_enabled: flags & 4 != 0,
            logon_group_is_logon_id: flags & 0xc0000000 == 0xc0000000,
            logon_group_deny_only: flags & 0x10 != 0,
            // Collector refuses ambiguous level/container/restriction queries.
            app_container: false,
            restricted: false,
        })
    })
}
