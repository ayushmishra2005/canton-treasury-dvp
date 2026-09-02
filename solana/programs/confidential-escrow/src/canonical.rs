use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hashv;

pub const DIGEST_DOMAIN: &[u8] = b"canton-treasury-dvp/bridge-digest/v1";
pub const AMOUNT_DOMAIN: &[u8] = b"canton-treasury-dvp/amount-commitment/v1";
pub const PROOF_DOMAIN: &[u8] = b"canton-treasury-dvp/proof-commitment/v1";

pub const DIRECTION_LOCK: u8 = 1;
pub const DIRECTION_CANCEL: u8 = 2;
pub const DIRECTION_RELEASE: u8 = 3;
pub const DIRECTION_MINT: u8 = 4;

pub const RECEIPT_LOCKED: u8 = 1;
pub const RECEIPT_MINT_AUTHORIZED: u8 = 2;
pub const RECEIPT_CANCELLED: u8 = 3;
pub const RECEIPT_RELEASED: u8 = 4;

pub fn cancel_permitted(status: u8) -> bool {
    status == RECEIPT_LOCKED
}

pub fn release_permitted(status: u8) -> bool {
    status == RECEIPT_MINT_AUTHORIZED
}

pub fn amount_commitment(amount: u64, blinding: &[u8; 32]) -> [u8; 32] {
    hashv(&[AMOUNT_DOMAIN, &amount.to_le_bytes(), blinding]).to_bytes()
}

pub fn proof_commitment(
    decryptable: &[u8; 36],
    auditor_lo: &[u8; 64],
    auditor_hi: &[u8; 64],
    equality_ctx: &Pubkey,
    validity_ctx: &Pubkey,
    range_ctx: &Pubkey,
) -> [u8; 32] {
    hashv(&[
        PROOF_DOMAIN,
        decryptable,
        auditor_lo,
        auditor_hi,
        equality_ctx.as_ref(),
        validity_ctx.as_ref(),
        range_ctx.as_ref(),
    ])
    .to_bytes()
}

#[allow(clippy::too_many_arguments)]
pub fn operation_digest(
    chain_id: &[u8; 32],
    program_id: &Pubkey,
    config: &Pubkey,
    operation: &[u8; 32],
    direction: u8,
    lock_receipt: &Pubkey,
    destination: &Pubkey,
    amount_commitment: &[u8; 32],
    zama_reservation: &[u8; 32],
    previous_operation: &[u8; 32],
    expiry: i64,
    proof_commitment: &[u8; 32],
) -> [u8; 32] {
    hashv(&[
        DIGEST_DOMAIN,
        chain_id,
        program_id.as_ref(),
        config.as_ref(),
        operation,
        &[direction],
        lock_receipt.as_ref(),
        destination.as_ref(),
        amount_commitment,
        zama_reservation,
        previous_operation,
        &expiry.to_le_bytes(),
        proof_commitment,
    ])
    .to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_commitment_is_hiding() {
        let first = amount_commitment(100_000, &[7u8; 32]);
        let second = amount_commitment(100_000, &[8u8; 32]);
        let other_amount = amount_commitment(100_001, &[7u8; 32]);
        assert_ne!(first, second);
        assert_ne!(first, other_amount);
        assert_ne!(first, {
            let mut unsalted = [0u8; 32];
            unsalted[..8].copy_from_slice(&100_000u64.to_le_bytes());
            unsalted
        });
    }

    #[test]
    fn digest_binds_every_field() {
        let program = Pubkey::new_from_array([1u8; 32]);
        let config = Pubkey::new_from_array([2u8; 32]);
        let receipt = Pubkey::new_from_array([3u8; 32]);
        let dest = Pubkey::new_from_array([4u8; 32]);
        let base = operation_digest(
            &[9u8; 32],
            &program,
            &config,
            &[5u8; 32],
            DIRECTION_RELEASE,
            &receipt,
            &dest,
            &[6u8; 32],
            &[7u8; 32],
            &[8u8; 32],
            42,
            &[10u8; 32],
        );
        let mutated = operation_digest(
            &[9u8; 32],
            &program,
            &config,
            &[5u8; 32],
            DIRECTION_RELEASE,
            &receipt,
            &dest,
            &[6u8; 32],
            &[7u8; 32],
            &[8u8; 32],
            43,
            &[10u8; 32],
        );
        assert_ne!(base, mutated);
    }

    #[test]
    fn cancel_before_mint_and_reject_after_mint_authorization() {
        assert!(cancel_permitted(RECEIPT_LOCKED));
        assert!(!cancel_permitted(RECEIPT_MINT_AUTHORIZED));
        assert!(!cancel_permitted(RECEIPT_CANCELLED));
        assert!(!cancel_permitted(RECEIPT_RELEASED));
        assert!(release_permitted(RECEIPT_MINT_AUTHORIZED));
        assert!(!release_permitted(RECEIPT_LOCKED));
    }
}
