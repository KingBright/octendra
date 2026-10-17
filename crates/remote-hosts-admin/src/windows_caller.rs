//! Existing-pipe caller collection adapter. No listener, grant or execution.
use crate::{
    privilege::{Grant, PlanRequest},
    protocol::Record,
    windows_pipe::{self, Assessment, CallerToken, Observation, Policy},
};
use anyhow::Result;

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
    collect: impl FnOnce() -> Result<CallerToken>,
) -> Result<Assessment> {
    // Discard identity claims before invoking the collector. A failure cannot
    // fall back to JSON metadata, a process token or a previous successful query.
    input.observation.caller = None;
    input.observation.impersonation_succeeded = false;
    input.observation.revert_succeeded = false;
    let caller = collect()?;
    input.observation.caller = Some(caller);
    input.observation.impersonation_succeeded = true;
    input.observation.revert_succeeded = true;
    windows_pipe::assess(
        input.policy,
        input.request,
        &input.observation,
        input.grant,
        input.previous,
        input.outcome_unknown,
        input.now,
    )
}
/// Reads the kernel identity associated with this connection's last message.
/// Trusted server code must retain/exclusively use the same pipe across its
/// frame read and this synchronous call. No collector/identity override is public.
/// Caller-token collection alone does not authenticate DACL, SCM or frame binding;
/// the result still has native_peer_authenticated=false and execution disabled.
#[cfg(windows)]
pub fn assess_after_read(
    pipe: std::os::windows::io::BorrowedHandle<'_>,
    input: AssessmentInput<'_>,
) -> Result<Assessment> {
    use windows_pipe::{ImpersonationLevel, Sid, TokenSource};
    assess_with_collector(input, || {
        let identity = remote_hosts_admin_win32::collect_after_read(pipe)?;
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
