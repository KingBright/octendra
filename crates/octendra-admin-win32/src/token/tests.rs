use super::*;
#[test]
fn overlapping_misaligned_sid_and_unbounded_layout_are_rejected() {
    for offset in [8, 17] {
        let (mut u, g) = fixtures(8);
        pointer(&mut u, 0, 8, BASE + offset);
        let value = binary(&[21, 1, 2, 3, 1001]);
        u[offset..offset + value.len()].copy_from_slice(&value);
        assert!(run(&u, &g, 8).is_err());
    }
    let (u, mut g) = fixtures(8);
    pointer(&mut g, 8, 8, BASE + 20);
    let value = binary(&[5, 4, 5]);
    g[20..20 + value.len()].copy_from_slice(&value);
    assert!(run(&u, &g, 8).is_err());
    assert!(run(&u, &g, usize::MAX).is_err());
}
const BASE: usize = 0x10000;
fn binary(sub: &[u32]) -> Vec<u8> {
    let mut b = vec![1, sub.len() as u8, 0, 0, 0, 0, 0, 5];
    for n in sub {
        b.extend_from_slice(&n.to_le_bytes());
    }
    b
}
fn pointer(b: &mut [u8], at: usize, p: usize, value: usize) {
    if p == 8 {
        b[at..at + 8].copy_from_slice(&(value as u64).to_le_bytes());
    } else {
        b[at..at + 4].copy_from_slice(&(value as u32).to_le_bytes());
    }
}
fn fixtures(p: usize) -> (Vec<u8>, Vec<u8>) {
    let mut u = vec![0u8; 64];
    pointer(&mut u, 0, p, BASE + 16);
    let sid = binary(&[21, 1, 2, 3, 1001]);
    u[16..16 + sid.len()].copy_from_slice(&sid);
    let mut g = vec![0u8; 64];
    g[..4].copy_from_slice(&1u32.to_le_bytes());
    let offset = if p == 8 { 8 } else { 4 };
    pointer(&mut g, offset, p, BASE + 32);
    g[offset + p..offset + p + 4].copy_from_slice(&0xc0000004u32.to_le_bytes());
    let sid = binary(&[5, 4, 5]);
    g[32..32 + sid.len()].copy_from_slice(&sid);
    (u, g)
}
fn span(b: &[u8], p: usize) -> Span<'_> {
    Span {
        bytes: b,
        base: BASE,
        pointer: p,
    }
}
fn run(u: &[u8], g: &[u8], p: usize) -> Result<CallerIdentity> {
    decode(span(u, p), span(g, p), 2, 2, 0, 0)
}
#[test]
fn checked_binary_sid_and_logon_groups_support_x64_and_x86_layouts() {
    for p in [4, 8] {
        let (u, g) = fixtures(p);
        let id = run(&u, &g, p).unwrap();
        assert_eq!(id.user_sid(), "S-1-5-21-1-2-3-1001");
        assert_eq!(id.logon_sid(), "S-1-5-5-4-5");
        assert_eq!(id.logon_attributes(), 0xc0000004);
    }
}
#[test]
fn sid_pointers_before_after_and_at_end_never_dereference() {
    for value in [0, BASE - 1, BASE + 63, BASE + 64, usize::MAX] {
        let (mut u, g) = fixtures(8);
        pointer(&mut u, 0, 8, value);
        assert!(run(&u, &g, 8).is_err());
    }
}
#[test]
fn invalid_sid_revision_count_and_truncated_subauthorities_are_rejected() {
    for i in 0..4 {
        let (mut u, g) = fixtures(8);
        match i {
            0 => u[16] = 2,
            1 => u[17] = 0,
            2 => u[17] = 16,
            _ => u.truncate(43),
        }
        assert!(run(&u, &g, 8).is_err());
    }
}
#[test]
fn invalid_pointer_layout_and_oversized_buffers_are_rejected() {
    let (u, g) = fixtures(8);
    assert!(run(&u, &g, 3).is_err());
    assert!(run(&u[..3], &g, 8).is_err());
    let mut u = u;
    u.resize(MAX_TOKEN + 1, 0);
    assert!(run(&u, &g, 8).is_err());
}
#[test]
fn group_count_limit_truncated_entries_and_missing_logon_fail_closed() {
    for count in [0, 2, 513, u32::MAX] {
        let (u, mut g) = fixtures(8);
        g[..4].copy_from_slice(&count.to_le_bytes());
        if count == 2 {
            g.truncate(30);
        }
        assert!(run(&u, &g, 8).is_err());
    }
}
#[test]
fn duplicate_logon_groups_are_rejected_even_when_sids_match() {
    let (u, mut g) = fixtures(8);
    g[..4].copy_from_slice(&2u32.to_le_bytes());
    let entry = g[8..24].to_vec();
    g[24..40].copy_from_slice(&entry);
    // Put the common SID after the now-larger entry table.
    pointer(&mut g, 8, 8, BASE + 40);
    pointer(&mut g, 24, 8, BASE + 40);
    let s = binary(&[5, 4, 5]);
    g[40..40 + s.len()].copy_from_slice(&s);
    assert!(run(&u, &g, 8).is_err());
}
#[test]
fn disabled_deny_only_and_partial_logon_flags_are_rejected() {
    for flags in [0u32, 4, 0x40000004, 0x80000004, 0xc0000000, 0xc0000014] {
        let (u, mut g) = fixtures(8);
        g[16..20].copy_from_slice(&flags.to_le_bytes());
        assert!(run(&u, &g, 8).is_err());
    }
}
#[test]
fn logon_flag_on_non_logon_sid_is_not_a_logon_identity() {
    let (u, mut g) = fixtures(8);
    let s = binary(&[32, 544]);
    g[32..32 + s.len()].copy_from_slice(&s);
    assert!(run(&u, &g, 8).is_err());
}
#[test]
fn non_logon_memberships_are_never_substituted_for_token_user() {
    let (u, mut g) = fixtures(8);
    g[..4].copy_from_slice(&2u32.to_le_bytes());
    pointer(&mut g, 8, 8, BASE + 40);
    g[16..20].copy_from_slice(&4u32.to_le_bytes());
    pointer(&mut g, 24, 8, BASE + 64);
    g[32..36].copy_from_slice(&0xc0000004u32.to_le_bytes());
    g.resize(96, 0);
    let ordinary = binary(&[32, 544]);
    g[40..40 + ordinary.len()].copy_from_slice(&ordinary);
    let logon = binary(&[5, 4, 5]);
    g[64..64 + logon.len()].copy_from_slice(&logon);
    assert_eq!(run(&u, &g, 8).unwrap().user_sid(), "S-1-5-21-1-2-3-1001");
}
#[test]
fn primary_anonymous_identification_and_delegation_tokens_are_rejected() {
    let (u, g) = fixtures(8);
    for (kind, level) in [(1, 2), (2, 0), (2, 1), (2, 3), (2, 99)] {
        assert!(decode(span(&u, 8), span(&g, 8), kind, level, 0, 0).is_err());
    }
}
#[test]
fn app_container_and_restriction_flags_never_assume_unknown_means_false() {
    let (u, g) = fixtures(8);
    for (app, r) in [(1, 0), (2, 0), (0, 1), (0, 2)] {
        assert!(decode(span(&u, 8), span(&g, 8), 2, 2, app, r).is_err());
    }
}
