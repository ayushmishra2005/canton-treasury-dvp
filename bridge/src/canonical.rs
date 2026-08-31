use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;

pub const DIGEST_DOMAIN: &[u8] = b"canton-treasury-dvp/bridge-digest/v1";
pub const AMOUNT_DOMAIN: &[u8] = b"canton-treasury-dvp/amount-commitment/v1";
pub const PROOF_DOMAIN: &[u8] = b"canton-treasury-dvp/proof-commitment/v1";

pub const DIRECTION_LOCK: u8 = 1;
pub const DIRECTION_CANCEL: u8 = 2;
pub const DIRECTION_RELEASE: u8 = 3;
pub const DIRECTION_MINT: u8 = 4;

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

pub fn amount_commitment(amount: u64, blinding: &[u8; 32]) -> [u8; 32] {
    sha256(&[AMOUNT_DOMAIN, &amount.to_le_bytes(), blinding])
}

pub fn proof_commitment(
    decryptable: &[u8; 36],
    auditor_lo: &[u8; 64],
    auditor_hi: &[u8; 64],
    equality_ctx: &Pubkey,
    validity_ctx: &Pubkey,
    range_ctx: &Pubkey,
) -> [u8; 32] {
    sha256(&[
        PROOF_DOMAIN,
        decryptable,
        auditor_lo,
        auditor_hi,
        equality_ctx.as_ref(),
        validity_ctx.as_ref(),
        range_ctx.as_ref(),
    ])
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
    sha256(&[
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_commitment_is_not_an_unsalted_amount_hash() {
        let blinding = [9u8; 32];
        let committed = amount_commitment(100_000, &blinding);
        let unsalted = sha256(&[&100_000u64.to_le_bytes()]);
        assert_ne!(committed, unsalted);
        assert_ne!(committed, amount_commitment(100_000, &[8u8; 32]));
    }
}
