pub mod attest;
pub mod canonical;
pub mod canton;
pub mod canton_history;
pub mod confidential;
pub mod journal;
pub mod program;
pub mod recovery;
pub mod relayer;
pub mod reservation;
pub mod setup;
pub mod txsize;
pub mod units;
pub mod workflow;
pub mod zama;

#[cfg(test)]
mod leak_tests {
    use super::canonical::amount_commitment;

    #[test]
    fn public_commitment_does_not_embed_plaintext_amount() {
        let commitment = amount_commitment(100_000, &[1u8; 32]);
        let amount = 100_000u64.to_le_bytes();
        assert!(!commitment.windows(8).any(|window| window == amount));
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::program::{approval_pda, receipt_pda};

    #[test]
    fn retry_after_interrupt_targets_the_same_receipt_and_approval() {
        let operation = [9u8; 32];
        let (first_receipt, _) = receipt_pda(&operation);
        let (second_receipt, _) = receipt_pda(&operation);
        assert_eq!(first_receipt, second_receipt);
        let (first_approval, _) = approval_pda(&operation, 3);
        let (second_approval, _) = approval_pda(&operation, 3);
        assert_eq!(first_approval, second_approval);
        let (other_op, _) = receipt_pda(&[8u8; 32]);
        assert_ne!(first_receipt, other_op);
    }
}
