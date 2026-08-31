use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"bridge-config";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault-authority";
pub const RECEIPT_SEED: &[u8] = b"receipt";
pub const APPROVAL_SEED: &[u8] = b"approval";

#[account]
#[derive(InitSpace)]
pub struct BridgeConfig {
    pub chain_id: [u8; 32],
    pub token_program: Pubkey,
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub attesters: [Pubkey; 3],
    pub bump: u8,
    pub vault_authority_bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct LockReceipt {
    pub operation: [u8; 32],
    pub destination: Pubkey,
    pub amount_commitment: [u8; 32],
    pub zama_reservation: [u8; 32],
    pub previous_operation: [u8; 32],
    pub expiry: i64,
    pub lock_proof_commitment: [u8; 32],
    pub status: u8,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct OperationApproval {
    pub operation: [u8; 32],
    pub direction: u8,
    pub digest: [u8; 32],
    pub signer_bitmap: u8,
    pub consumed: bool,
    pub bump: u8,
}

impl BridgeConfig {
    pub fn attester_index(&self, attester: &Pubkey) -> Option<u8> {
        self.attesters
            .iter()
            .position(|key| key == attester)
            .map(|index| index as u8)
    }
}

impl OperationApproval {
    pub fn distinct_signers(&self) -> u32 {
        (self.signer_bitmap & 0b111).count_ones()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(bitmap: u8) -> OperationApproval {
        OperationApproval {
            operation: [0u8; 32],
            direction: 1,
            digest: [0u8; 32],
            signer_bitmap: bitmap,
            consumed: false,
            bump: 0,
        }
    }

    #[test]
    fn one_attester_is_below_threshold() {
        assert_eq!(approval(0b001).distinct_signers(), 1);
        assert_eq!(approval(0b010).distinct_signers(), 1);
        assert!(approval(0b001).distinct_signers() < 2);
    }

    #[test]
    fn duplicate_attester_bit_does_not_count_twice() {
        assert_eq!(approval(0b001).distinct_signers(), 1);
        assert_eq!(approval(0b001 | 0b001).distinct_signers(), 1);
    }

    #[test]
    fn two_distinct_configured_attesters_meet_threshold() {
        assert_eq!(approval(0b101).distinct_signers(), 2);
        assert!(approval(0b101).distinct_signers() >= 2);
    }
}
