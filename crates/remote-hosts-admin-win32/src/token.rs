//! Pointer-aware token decoding using checked slices only. No pointer dereference.
use crate::CallerIdentity;
use anyhow::{Result, ensure};
pub(crate) const MAX_TOKEN: usize = 65536;
pub(crate) struct Span<'a> {
    pub bytes: &'a [u8],
    pub base: usize,
    pub pointer: usize,
}
impl Span<'_> {
    fn slice(&self, offset: usize, len: usize) -> Result<&[u8]> {
        self.bytes
            .get(
                offset
                    ..offset
                        .checked_add(len)
                        .ok_or_else(|| anyhow::anyhow!("token_offset_overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("token_bounds"))
    }
    fn u32(&self, offset: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(self.slice(offset, 4)?.try_into()?))
    }
    fn sid(&self, pointer_offset: usize, minimum_payload: usize) -> Result<String> {
        ensure!(
            matches!(self.pointer, 4 | 8) && self.bytes.len() <= MAX_TOKEN,
            "token_layout"
        );
        let ptr = match self.pointer {
            4 => self.u32(pointer_offset)? as usize,
            _ => usize::try_from(u64::from_le_bytes(
                self.slice(pointer_offset, 8)?.try_into()?,
            ))?,
        };
        let offset = ptr
            .checked_sub(self.base)
            .ok_or_else(|| anyhow::anyhow!("sid_pointer_before_buffer"))?;
        ensure!(
            offset >= minimum_payload && offset % 4 == 0,
            "sid_overlaps_header_or_is_unaligned"
        );
        let header = self.slice(offset, 8)?;
        ensure!(
            header[0] == 1 && (1..=15).contains(&header[1]),
            "invalid_binary_sid"
        );
        let count = header[1] as usize;
        let data = self.slice(offset, 8 + 4 * count)?;
        let authority = data[2..8]
            .iter()
            .fold(0u64, |n, b| (n << 8) | u64::from(*b));
        let mut text = format!("S-1-{authority}");
        for part in data[8..].chunks_exact(4) {
            text.push_str(&format!("-{}", u32::from_le_bytes(part.try_into()?)));
        }
        Ok(text)
    }
    fn logon(&self) -> Result<(String, u32)> {
        ensure!(
            matches!(self.pointer, 4 | 8) && self.bytes.len() <= MAX_TOKEN,
            "token_layout"
        );
        let count = self.u32(0)? as usize;
        ensure!(count <= 512, "token_group_limit");
        let offset = if self.pointer == 8 { 8 } else { 4 };
        let stride = if self.pointer == 8 { 16 } else { 8 };
        self.slice(offset, count * stride)?;
        let mut result = None;
        for index in 0..count {
            let at = offset + index * stride;
            let attributes = self.u32(at + self.pointer)?;
            // Partial LOGON_ID flags are ambiguous, never a usable identity.
            ensure!(
                attributes & 0xc0000000 == 0 || attributes & 0xc0000000 == 0xc0000000,
                "partial_logon_flag"
            );
            if attributes & 0xc0000000 == 0xc0000000 {
                ensure!(result.is_none(), "multiple_logon_sids");
                result = Some((self.sid(at, offset + count * stride)?, attributes));
            }
        }
        result.ok_or_else(|| anyhow::anyhow!("logon_sid_unavailable"))
    }
}
pub(crate) fn decode(
    user: Span<'_>,
    groups: Span<'_>,
    kind: u32,
    level: u32,
    app_container: u32,
    restricted: u32,
) -> Result<CallerIdentity> {
    // Identification-level TokenIsAppContainer=0 is not sufficient evidence:
    // Microsoft explicitly requires AccessDenied for that ambiguous case.
    ensure!(
        kind == 2 && level == 2,
        "local_impersonation_token_required"
    );
    ensure!(
        app_container == 0 && restricted == 0,
        "sandbox_or_restricted_token_denied"
    );
    ensure!(matches!(user.pointer, 4 | 8), "token_layout");
    let header_size = 2 * user.pointer;
    ensure!(user.bytes.len() >= header_size, "token_user_truncated");
    let user_sid = user.sid(0, header_size)?;
    let (logon_sid, attributes) = groups.logon()?;
    ensure!(
        attributes & 4 != 0 && attributes & 0x10 == 0,
        "logon_group_disabled_or_deny_only"
    );
    ensure!(
        logon_sid.starts_with("S-1-5-5-") && logon_sid.split('-').count() == 6,
        "invalid_logon_sid"
    );
    Ok(CallerIdentity {
        user_sid,
        logon_sid,
        logon_attributes: attributes,
    })
}
#[cfg(test)]
mod tests;
