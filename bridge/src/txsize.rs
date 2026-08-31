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
}
