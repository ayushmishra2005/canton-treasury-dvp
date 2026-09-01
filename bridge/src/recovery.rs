use anyhow::{anyhow, Result};

use crate::reservation::RESERVATION_REDEEMED;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionDecision {
    Continue,
    RecordAndExit,
}

#[allow(clippy::too_many_arguments)]
pub fn completed_operation_decision(
    zama_status: Result<u8>,
    zama_approved: Option<Result<bool>>,
    receipt_status: Result<Option<u8>>,
    dest_pending: Result<u64>,
    dest_available: Result<u64>,
    expected_amount: u64,
    has_canton_settlement: bool,
    has_canton_redemption: bool,
    payout_matches: bool,
) -> Result<CompletionDecision> {
    let status = zama_status.map_err(|err| anyhow!("failed to read reservation status: {err}"))?;
    if status != RESERVATION_REDEEMED {
        return Ok(CompletionDecision::Continue);
    }
    let approved = zama_approved
        .ok_or_else(|| anyhow!("reservation approval was not read"))?
        .map_err(|err| anyhow!("failed to read reservation approval: {err}"))?;
    if !approved {
        return Err(anyhow!(
            "Zama rejected the reservation; no lock or mint will be attempted"
        ));
    }
    let receipt = receipt_status.map_err(|err| anyhow!("failed to read receipt status: {err}"))?;
    if receipt != Some(RECEIPT_RELEASED) {
        return Err(anyhow!(
            "Zama redemption is not enough; Solana receipt is not released"
        ));
    }
    let pending =
        dest_pending.map_err(|err| anyhow!("failed to read destination pending credits: {err}"))?;
    let available = dest_available
        .map_err(|err| anyhow!("failed to read destination available balance: {err}"))?;
    if pending != 0 || available != expected_amount {
        return Err(anyhow!(
            "destination balances do not match the completed operation"
        ));
    }
    if !has_canton_settlement || !has_canton_redemption {
        return Err(anyhow!(
            "Canton settlement or redemption evidence is missing"
        ));
    }
    if !payout_matches {
        return Err(anyhow!(
            "payout destination does not match the recorded operation"
        ));
    }
    Ok(CompletionDecision::RecordAndExit)
}

pub const RECEIPT_LOCKED: u8 = 1;
pub const RECEIPT_MINT_AUTHORIZED: u8 = 2;
pub const RECEIPT_CANCELLED: u8 = 3;
pub const RECEIPT_RELEASED: u8 = 4;

pub const APPROVAL_DISCRIMINATOR: usize = 8;
pub const APPROVAL_BITMAP_OFFSET: usize = APPROVAL_DISCRIMINATOR + 32 + 1 + 32;
pub const APPROVAL_CONSUMED_OFFSET: usize = APPROVAL_BITMAP_OFFSET + 1;
pub const APPROVAL_EXPIRY_OFFSET: usize = APPROVAL_CONSUMED_OFFSET + 1 + 1;
pub const RECEIPT_STATUS_OFFSET: usize = APPROVAL_DISCRIMINATOR + 32 + 32 + 32 + 32 + 32 + 8 + 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnChainApproval {
    pub signer_bitmap: u8,
    pub consumed: bool,
    pub expiry: i64,
    pub digest: [u8; 32],
}

pub fn approval_expired_on_chain(expiry: i64, chain_now: i64) -> bool {
    chain_now >= expiry
}

pub fn attesters_needed(
    existing: Option<&OnChainApproval>,
    digest: &[u8; 32],
    expiry: i64,
    chain_now: i64,
    replace_expired: bool,
) -> Result<[bool; 2]> {
    let Some(existing) = existing else {
        if approval_expired_on_chain(expiry, chain_now) {
            return Err(anyhow!("authorization has expired"));
        }
        return Ok([true, true]);
    };
    if existing.consumed {
        return Err(anyhow!("approval already consumed"));
    }
    if approval_expired_on_chain(existing.expiry, chain_now) {
        if replace_expired {
            return Ok([true, true]);
        }
        if existing.digest == *digest
            && existing.expiry == expiry
            && existing.signer_bitmap.count_ones() >= 2
        {
            return Ok([false, false]);
        }
        return Err(anyhow!("mint authorization has expired"));
    }
    if existing.digest != *digest || existing.expiry != expiry {
        return Err(anyhow!(
            "digest mismatch while approval is valid; refusing to replace an unexpired approval"
        ));
    }
    Ok([
        existing.signer_bitmap & 1 == 0,
        existing.signer_bitmap & 2 == 0,
    ])
}

pub fn should_submit_release(journal_released: bool, receipt_status: Option<u8>) -> bool {
    !journal_released && receipt_status != Some(RECEIPT_RELEASED)
}

pub fn should_apply_pending(pending: u64, available: u64, expected_final: u64) -> Result<bool> {
    if pending == 0 {
        if available == expected_final {
            return Ok(false);
        }
        return Err(anyhow!(
            "destination available {available} does not match expected {expected_final}"
        ));
    }
    Ok(true)
}

pub fn should_refresh_release_materials(
    chain_now: i64,
    on_chain: Option<&OnChainApproval>,
    journal_expiry: i64,
    receipt_status: Option<u8>,
) -> bool {
    if receipt_status == Some(RECEIPT_RELEASED) {
        return false;
    }
    if let Some(approval) = on_chain {
        if approval.consumed {
            return false;
        }
        return approval_expired_on_chain(approval.expiry, chain_now);
    }
    journal_expiry > 0 && approval_expired_on_chain(journal_expiry, chain_now)
}

pub fn decode_chain_clock(data: &[u8]) -> Result<i64> {
    const UNIX_TIMESTAMP_OFFSET: usize = 32;
    data.get(UNIX_TIMESTAMP_OFFSET..UNIX_TIMESTAMP_OFFSET + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or_else(|| anyhow!("Solana clock sysvar is truncated"))
}

pub fn decode_approval(data: &[u8]) -> Result<OnChainApproval> {
    if data.len() < APPROVAL_EXPIRY_OFFSET + 8 {
        return Err(anyhow!("approval account is truncated"));
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&data[APPROVAL_DISCRIMINATOR + 32 + 1..APPROVAL_BITMAP_OFFSET]);
    let expiry = i64::from_le_bytes(
        data[APPROVAL_EXPIRY_OFFSET..APPROVAL_EXPIRY_OFFSET + 8]
            .try_into()
            .map_err(|_| anyhow!("approval expiry"))?,
    );
    Ok(OnChainApproval {
        signer_bitmap: data[APPROVAL_BITMAP_OFFSET],
        consumed: data[APPROVAL_CONSUMED_OFFSET] != 0,
        expiry,
        digest,
    })
}

pub fn decode_receipt_status(data: &[u8]) -> Result<u8> {
    data.get(RECEIPT_STATUS_OFFSET)
        .copied()
        .ok_or_else(|| anyhow!("receipt account is truncated"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_submits_only_the_missing_attestation() {
        let first = OnChainApproval {
            signer_bitmap: 0b001,
            consumed: false,
            expiry: 5_000,
            digest: [7u8; 32],
        };
        assert_eq!(
            attesters_needed(Some(&first), &[7u8; 32], 5_000, 1_000, false).unwrap(),
            [false, true]
        );
    }

    #[test]
    fn resume_recognises_existing_two_of_three() {
        let both = OnChainApproval {
            signer_bitmap: 0b011,
            consumed: false,
            expiry: 5_000,
            digest: [7u8; 32],
        };
        assert_eq!(
            attesters_needed(Some(&both), &[7u8; 32], 5_000, 1_000, false).unwrap(),
            [false, false]
        );
        assert_eq!(
            attesters_needed(None, &[7u8; 32], 5_000, 1_000, false).unwrap(),
            [true, true]
        );
    }

    #[test]
    fn released_receipt_does_not_release_again() {
        assert!(!should_submit_release(false, Some(RECEIPT_RELEASED)));
        assert!(should_submit_release(false, Some(RECEIPT_MINT_AUTHORIZED)));
        assert!(!should_submit_release(true, Some(RECEIPT_RELEASED)));
    }

    #[test]
    fn applied_pending_is_not_applied_twice() {
        let amount = 100_000_000_000u64;
        assert!(!should_apply_pending(0, amount, amount).unwrap());
        assert!(should_apply_pending(1, 0, amount).unwrap());
        assert!(should_apply_pending(0, 0, amount).is_err());
        assert!(should_apply_pending(0, amount * 2, amount).is_err());
    }

    fn live_approval() -> OnChainApproval {
        OnChainApproval {
            signer_bitmap: 0b011,
            consumed: false,
            expiry: 1_500,
            digest: [7u8; 32],
        }
    }

    #[test]
    fn host_clock_ahead_of_chain_does_not_replace_a_live_approval() {
        let host_now = 2_000;
        let chain_now = 1_000;
        let approval = live_approval();
        assert!(host_now >= approval.expiry);
        assert!(!approval_expired_on_chain(approval.expiry, chain_now));
        assert!(!should_refresh_release_materials(
            chain_now,
            Some(&approval),
            approval.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
        assert_eq!(
            attesters_needed(
                Some(&approval),
                &approval.digest,
                approval.expiry,
                chain_now,
                true
            )
            .unwrap(),
            [false, false]
        );
        assert!(attesters_needed(Some(&approval), &[9u8; 32], 3_000, chain_now, true).is_err());
    }

    #[test]
    fn host_clock_behind_chain_replaces_after_chain_expiry() {
        let host_now = 1_000;
        let chain_now = 2_000;
        let approval = live_approval();
        assert!(host_now < approval.expiry);
        assert!(approval_expired_on_chain(approval.expiry, chain_now));
        assert!(should_refresh_release_materials(
            chain_now,
            Some(&approval),
            approval.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
        assert_eq!(
            attesters_needed(Some(&approval), &[9u8; 32], 3_000, chain_now, true).unwrap(),
            [true, true]
        );
    }

    #[test]
    fn chain_time_equal_to_expiry_is_expired() {
        let approval = live_approval();
        assert!(approval_expired_on_chain(approval.expiry, approval.expiry));
        assert!(should_refresh_release_materials(
            approval.expiry,
            Some(&approval),
            approval.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
        assert_eq!(
            attesters_needed(Some(&approval), &[9u8; 32], 3_000, approval.expiry, true).unwrap(),
            [true, true]
        );
    }

    #[test]
    fn approval_still_valid_is_reused() {
        let approval = live_approval();
        assert!(!should_refresh_release_materials(
            1_000,
            Some(&approval),
            approval.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
        assert_eq!(
            attesters_needed(
                Some(&approval),
                &approval.digest,
                approval.expiry,
                1_000,
                true
            )
            .unwrap(),
            [false, false]
        );
    }

    #[test]
    fn approval_expired_refreshes_materials() {
        let approval = live_approval();
        assert!(should_refresh_release_materials(
            2_000,
            Some(&approval),
            approval.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
        assert!(!should_refresh_release_materials(
            2_000,
            Some(&approval),
            approval.expiry,
            Some(RECEIPT_RELEASED)
        ));
        let consumed = OnChainApproval {
            consumed: true,
            ..approval
        };
        assert!(!should_refresh_release_materials(
            2_000,
            Some(&consumed),
            consumed.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
    }

    #[test]
    fn one_missing_attestation_is_the_only_submit() {
        let first = OnChainApproval {
            signer_bitmap: 0b001,
            consumed: false,
            expiry: 5_000,
            digest: [7u8; 32],
        };
        assert_eq!(
            attesters_needed(Some(&first), &[7u8; 32], 5_000, 1_000, true).unwrap(),
            [false, true]
        );
        assert!(!should_refresh_release_materials(
            1_000,
            Some(&first),
            5_000,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
    }

    #[test]
    fn conflicting_attestation_does_not_count_toward_quorum() {
        let first = OnChainApproval {
            signer_bitmap: 0b001,
            consumed: false,
            expiry: 5_000,
            digest: [7u8; 32],
        };
        let err = attesters_needed(Some(&first), &[9u8; 32], 5_000, 1_000, false).unwrap_err();
        assert!(
            err.to_string()
                .contains("digest mismatch while approval is valid"),
            "{err}"
        );
        assert_eq!(first.signer_bitmap.count_ones(), 1);
        assert_eq!(
            attesters_needed(Some(&first), &[7u8; 32], 5_000, 1_000, false).unwrap(),
            [false, true]
        );
    }

    #[test]
    fn digest_mismatch_while_approval_is_valid_is_rejected() {
        let approval = live_approval();
        let err = attesters_needed(Some(&approval), &[9u8; 32], 3_000, 1_000, true).unwrap_err();
        assert!(
            err.to_string()
                .contains("digest mismatch while approval is valid"),
            "{err}"
        );
        assert!(!should_refresh_release_materials(
            1_000,
            Some(&approval),
            approval.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
    }

    #[test]
    fn successful_replacement_after_expiry() {
        let expired = live_approval();
        assert_eq!(
            attesters_needed(Some(&expired), &[9u8; 32], 3_000, 2_000, true).unwrap(),
            [true, true]
        );
        assert!(should_refresh_release_materials(
            2_000,
            Some(&expired),
            expired.expiry,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
    }

    #[test]
    fn repeated_resume_after_release_does_not_release_again() {
        assert!(!should_submit_release(false, Some(RECEIPT_RELEASED)));
        assert!(!should_submit_release(true, Some(RECEIPT_RELEASED)));
        assert!(!should_refresh_release_materials(
            9_000,
            Some(&OnChainApproval {
                consumed: true,
                expiry: 1_500,
                digest: [7u8; 32],
                signer_bitmap: 0b011,
            }),
            1_500,
            Some(RECEIPT_RELEASED)
        ));
    }

    #[test]
    fn exactly_one_confidential_release() {
        assert!(should_submit_release(false, Some(RECEIPT_MINT_AUTHORIZED)));
        assert!(!should_submit_release(false, Some(RECEIPT_RELEASED)));
        assert!(!should_submit_release(true, Some(RECEIPT_MINT_AUTHORIZED)));
        assert!(!should_submit_release(true, Some(RECEIPT_RELEASED)));
    }

    #[test]
    fn journal_expiry_is_ignored_while_an_on_chain_approval_is_live() {
        let approval = live_approval();
        assert!(!should_refresh_release_materials(
            1_000,
            Some(&approval),
            500,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
    }

    #[test]
    fn chain_clock_sysvar_decodes_unix_timestamp() {
        let mut data = vec![0u8; 40];
        data[32..40].copy_from_slice(&1_700_000_000i64.to_le_bytes());
        assert_eq!(decode_chain_clock(&data).unwrap(), 1_700_000_000);
        assert!(decode_chain_clock(&[0u8; 16]).is_err());
    }

    #[test]
    fn expired_unused_approval_needs_both_attesters_again() {
        let expired = live_approval();
        assert_eq!(
            attesters_needed(Some(&expired), &[9u8; 32], 3_000, 2_000, true).unwrap(),
            [true, true]
        );
        let expired_one = OnChainApproval {
            signer_bitmap: 0b001,
            consumed: false,
            expiry: 1_500,
            digest: [7u8; 32],
        };
        assert!(attesters_needed(Some(&expired_one), &[7u8; 32], 1_500, 2_000, false).is_err());
        assert_eq!(
            attesters_needed(Some(&expired), &[7u8; 32], 1_500, 2_000, false).unwrap(),
            [false, false]
        );
    }

    #[test]
    fn redeemed_alone_is_not_completion() {
        let err = completed_operation_decision(
            Ok(4),
            Some(Ok(true)),
            Ok(Some(RECEIPT_MINT_AUTHORIZED)),
            Ok(0),
            Ok(0),
            100_000_000_000,
            true,
            true,
            true,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not released") || err.to_string().contains("receipt"),
            "{err}"
        );
        assert!(completed_operation_decision(
            Ok(4),
            Some(Ok(true)),
            Ok(Some(RECEIPT_RELEASED)),
            Ok(0),
            Ok(100_000_000_000),
            100_000_000_000,
            false,
            true,
            true,
        )
        .is_err());
        assert!(completed_operation_decision(
            Ok(4),
            Some(Ok(false)),
            Ok(Some(RECEIPT_RELEASED)),
            Ok(0),
            Ok(100_000_000_000),
            100_000_000_000,
            true,
            true,
            true,
        )
        .is_err());
        assert!(completed_operation_decision(
            Ok(4),
            Some(Err(anyhow!("decrypt failed"))),
            Ok(Some(RECEIPT_RELEASED)),
            Ok(0),
            Ok(100_000_000_000),
            100_000_000_000,
            true,
            true,
            true,
        )
        .is_err());
        assert!(completed_operation_decision(
            Ok(4),
            Some(Ok(true)),
            Err(anyhow!("rpc down")),
            Ok(0),
            Ok(100_000_000_000),
            100_000_000_000,
            true,
            true,
            true,
        )
        .is_err());
    }

    #[test]
    fn crash_after_solana_release_resumes_without_second_release() {
        assert!(!should_submit_release(false, Some(RECEIPT_RELEASED)));
        assert_eq!(
            completed_operation_decision(
                Ok(2),
                Some(Ok(true)),
                Ok(Some(RECEIPT_RELEASED)),
                Ok(0),
                Ok(100_000_000_000),
                100_000_000_000,
                true,
                true,
                true,
            )
            .unwrap(),
            CompletionDecision::Continue
        );
    }

    #[test]
    fn crash_after_zama_redeem_records_completion_without_new_work() {
        assert_eq!(
            completed_operation_decision(
                Ok(4),
                Some(Ok(true)),
                Ok(Some(RECEIPT_RELEASED)),
                Ok(0),
                Ok(100_000_000_000),
                100_000_000_000,
                true,
                true,
                true,
            )
            .unwrap(),
            CompletionDecision::RecordAndExit
        );
    }

    #[test]
    fn matching_completed_evidence_is_recorded() {
        assert_eq!(
            completed_operation_decision(
                Ok(4),
                Some(Ok(true)),
                Ok(Some(RECEIPT_RELEASED)),
                Ok(0),
                Ok(100_000_000_000),
                100_000_000_000,
                true,
                true,
                true,
            )
            .unwrap(),
            CompletionDecision::RecordAndExit
        );
        assert_eq!(
            completed_operation_decision(
                Ok(2),
                Some(Ok(true)),
                Ok(Some(RECEIPT_MINT_AUTHORIZED)),
                Ok(0),
                Ok(0),
                100_000_000_000,
                false,
                false,
                true,
            )
            .unwrap(),
            CompletionDecision::Continue
        );
    }

    #[test]
    fn approval_and_receipt_offsets_match_account_layout() {
        assert_eq!(APPROVAL_BITMAP_OFFSET, 73);
        assert_eq!(APPROVAL_CONSUMED_OFFSET, 74);
        assert_eq!(APPROVAL_EXPIRY_OFFSET, 76);
        assert_eq!(RECEIPT_STATUS_OFFSET, 208);
        let mut approval = vec![0u8; 84];
        approval[APPROVAL_BITMAP_OFFSET] = 0b101;
        approval[APPROVAL_CONSUMED_OFFSET] = 1;
        approval[APPROVAL_EXPIRY_OFFSET..APPROVAL_EXPIRY_OFFSET + 8]
            .copy_from_slice(&1_234i64.to_le_bytes());
        approval[APPROVAL_DISCRIMINATOR + 32 + 1..APPROVAL_BITMAP_OFFSET]
            .copy_from_slice(&[3u8; 32]);
        let decoded = decode_approval(&approval).unwrap();
        assert_eq!(decoded.signer_bitmap, 0b101);
        assert!(decoded.consumed);
        assert_eq!(decoded.expiry, 1_234);
        assert_eq!(decoded.digest, [3u8; 32]);
        let mut receipt = vec![0u8; 210];
        receipt[RECEIPT_STATUS_OFFSET] = RECEIPT_RELEASED;
        assert_eq!(decode_receipt_status(&receipt).unwrap(), RECEIPT_RELEASED);
    }
}
