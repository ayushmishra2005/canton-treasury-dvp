use anyhow::{anyhow, Result};

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

pub fn attesters_needed(
    existing: Option<&OnChainApproval>,
    digest: &[u8; 32],
    expiry: i64,
    now: i64,
    replace_expired: bool,
) -> Result<[bool; 2]> {
    let Some(existing) = existing else {
        if now >= expiry {
            return Err(anyhow!("authorization has expired"));
        }
        return Ok([true, true]);
    };
    if existing.consumed {
        return Err(anyhow!("approval already consumed"));
    }
    if now >= existing.expiry {
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
        return Err(anyhow!("existing approval does not match this operation"));
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
    now: i64,
    journal_expiry: i64,
    receipt_status: Option<u8>,
) -> bool {
    receipt_status != Some(RECEIPT_RELEASED) && journal_expiry > 0 && now >= journal_expiry
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

    #[test]
    fn expired_release_approval_refreshes_materials() {
        assert!(should_refresh_release_materials(
            2_000,
            1_500,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
        assert!(!should_refresh_release_materials(
            1_000,
            1_500,
            Some(RECEIPT_MINT_AUTHORIZED)
        ));
        assert!(!should_refresh_release_materials(
            2_000,
            1_500,
            Some(RECEIPT_RELEASED)
        ));
    }

    #[test]
    fn expired_unused_approval_needs_both_attesters_again() {
        let expired = OnChainApproval {
            signer_bitmap: 0b011,
            consumed: false,
            expiry: 1_500,
            digest: [7u8; 32],
        };
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
        let authorized = OnChainApproval {
            signer_bitmap: 0b011,
            consumed: false,
            expiry: 1_500,
            digest: [7u8; 32],
        };
        assert_eq!(
            attesters_needed(Some(&authorized), &[7u8; 32], 1_500, 2_000, false).unwrap(),
            [false, false]
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
