use anyhow::Result;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

pub const LEGACY_LIMIT: usize = 1232;

pub fn serialize_legacy(
    instructions: &[Instruction],
    fee_payer: &Pubkey,
) -> Result<(Transaction, usize)> {
    let message = Message::new(instructions, Some(fee_payer));
    let mut transaction = Transaction::new_unsigned(message);
    transaction.message.recent_blockhash = Hash::default();
    if transaction.signatures.is_empty() {
        transaction.signatures = vec![Signature::default()];
    }
    let bytes = bincode::serialize(&transaction)?;
    Ok((transaction, bytes.len()))
}

pub fn report_size(label: &str, size: usize) {
    println!("TX_SIZE {label} serialized={size} legacy_limit={LEGACY_LIMIT}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;
    use solana_system_interface::instruction as system_instruction;

    #[test]
    fn legacy_size_includes_fee_payer_signature() {
        let payer = Pubkey::new_unique();
        let dest = Pubkey::new_unique();
        let ix = system_instruction::transfer(&payer, &dest, 1);
        let (_tx, size) = serialize_legacy(&[ix], &payer).unwrap();
        assert!(size > 64);
        assert!(size <= LEGACY_LIMIT);
    }

    #[test]
    fn lock_approve_and_release_fit_relayer_legacy_limit() {
        use crate::attest::ed25519_approve_ix;
        use crate::program::{
            approve_ix, lock_ix, move_ix, ApproveFields, LockFields, MovementFields,
        };
        use solana_sdk::signature::Keypair;

        let payer = Pubkey::new_unique();
        let source_authority = Pubkey::new_unique();
        let source = Pubkey::new_unique();
        let vault = Pubkey::new_unique();
        let dest = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let equality = Pubkey::new_unique();
        let validity = Pubkey::new_unique();
        let range = Pubkey::new_unique();
        let lock = lock_ix(
            payer,
            source_authority,
            source,
            vault,
            mint,
            spl_token_2022::id(),
            equality,
            validity,
            range,
            &LockFields {
                operation: [7u8; 32],
                destination: dest,
                amount_commitment: [8u8; 32],
                zama_reservation: [9u8; 32],
                previous_operation: [0u8; 32],
                expiry: 1,
                transfer_data: [0u8; 167],
                vault_decryptable: [0u8; 36],
            },
        )
        .unwrap();
        let (_tx, lock_size) = serialize_legacy(&[lock], &payer).unwrap();
        assert!(lock_size <= LEGACY_LIMIT, "lock {lock_size}");

        let release = move_ix(
            "release_confidential",
            vault,
            dest,
            mint,
            spl_token_2022::id(),
            equality,
            validity,
            range,
            &[7u8; 32],
            3,
            &MovementFields {
                destination: dest,
                amount_commitment: [8u8; 32],
                zama_reservation: [9u8; 32],
                previous_operation: [0u8; 32],
                expiry: 1,
                transfer_data: [0u8; 167],
            },
        )
        .unwrap();
        let (_tx, release_size) = serialize_legacy(&[release], &payer).unwrap();
        assert!(release_size <= LEGACY_LIMIT, "release {release_size}");

        let attester = Keypair::new();
        let approve = approve_ix(
            payer,
            &ApproveFields {
                operation: [7u8; 32],
                direction: 3,
                destination: dest,
                amount_commitment: [8u8; 32],
                zama_reservation: [9u8; 32],
                previous_operation: [0u8; 32],
                expiry: 1,
                proof_commitment: [4u8; 32],
            },
        )
        .unwrap();
        let ed = ed25519_approve_ix(&attester, &[3u8; 32]).unwrap();
        let (_tx, approve_size) = serialize_legacy(&[ed, approve], &payer).unwrap();
        assert!(approve_size <= LEGACY_LIMIT, "approve {approve_size}");
    }
}
