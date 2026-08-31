use anchor_lang::prelude::*;
use solana_program::sysvar::instructions::{
    load_current_index_checked, load_instruction_at_checked, ID as INSTRUCTIONS_ID,
};

use crate::errors::EscrowError;

const ED25519_PROGRAM_ID: Pubkey = solana_program::ed25519_program::ID;
const CURRENT_IX: u16 = u16::MAX;
const PUBKEY_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;
const DIGEST_LEN: usize = 32;
const HEADER_LEN: usize = 2;
const OFFSETS_LEN: usize = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ed25519Approval {
    pub attester: Pubkey,
    pub digest: [u8; 32],
}

pub fn read_preceding_ed25519(instructions_sysvar: &AccountInfo) -> Result<Ed25519Approval> {
    require_keys_eq!(
        *instructions_sysvar.key,
        INSTRUCTIONS_ID,
        EscrowError::MissingEd25519
    );
    let current = load_current_index_checked(instructions_sysvar)?;
    require!(current > 0, EscrowError::MissingEd25519);
    let prior = load_instruction_at_checked((current - 1) as usize, instructions_sysvar)?;
    require_keys_eq!(
        prior.program_id,
        ED25519_PROGRAM_ID,
        EscrowError::MissingEd25519
    );
    parse_ed25519_instruction(&prior.data)
}

fn parse_ed25519_instruction(data: &[u8]) -> Result<Ed25519Approval> {
    require!(
        data.len() >= HEADER_LEN + OFFSETS_LEN,
        EscrowError::InvalidEd25519Layout
    );
    let num_signatures = data[0];
    require!(num_signatures == 1, EscrowError::InvalidEd25519Layout);
    require!(data[1] == 0, EscrowError::InvalidEd25519Layout);

    let signature_offset = u16::from_le_bytes(data[2..4].try_into().unwrap()) as usize;
    let signature_ix = u16::from_le_bytes(data[4..6].try_into().unwrap());
    let pubkey_offset = u16::from_le_bytes(data[6..8].try_into().unwrap()) as usize;
    let pubkey_ix = u16::from_le_bytes(data[8..10].try_into().unwrap());
    let message_offset = u16::from_le_bytes(data[10..12].try_into().unwrap()) as usize;
    let message_size = u16::from_le_bytes(data[12..14].try_into().unwrap()) as usize;
    let message_ix = u16::from_le_bytes(data[14..16].try_into().unwrap());

    require!(
        signature_ix == CURRENT_IX && pubkey_ix == CURRENT_IX && message_ix == CURRENT_IX,
        EscrowError::InvalidEd25519Layout
    );
    require!(
        message_size == DIGEST_LEN,
        EscrowError::InvalidEd25519Layout
    );
    require!(
        range_in_data(data.len(), pubkey_offset, PUBKEY_LEN),
        EscrowError::InvalidEd25519Layout
    );
    require!(
        range_in_data(data.len(), signature_offset, SIGNATURE_LEN),
        EscrowError::InvalidEd25519Layout
    );
    require!(
        range_in_data(data.len(), message_offset, DIGEST_LEN),
        EscrowError::InvalidEd25519Layout
    );
    require!(
        pubkey_offset >= HEADER_LEN + OFFSETS_LEN,
        EscrowError::InvalidEd25519Layout
    );
    require!(
        signature_offset >= HEADER_LEN + OFFSETS_LEN,
        EscrowError::InvalidEd25519Layout
    );
    require!(
        message_offset >= HEADER_LEN + OFFSETS_LEN,
        EscrowError::InvalidEd25519Layout
    );
    require!(
        !ranges_overlap(pubkey_offset, PUBKEY_LEN, signature_offset, SIGNATURE_LEN),
        EscrowError::InvalidEd25519Layout
    );
    require!(
        !ranges_overlap(pubkey_offset, PUBKEY_LEN, message_offset, DIGEST_LEN),
        EscrowError::InvalidEd25519Layout
    );
    require!(
        !ranges_overlap(signature_offset, SIGNATURE_LEN, message_offset, DIGEST_LEN),
        EscrowError::InvalidEd25519Layout
    );

    let attester = Pubkey::try_from(&data[pubkey_offset..pubkey_offset + PUBKEY_LEN])
        .map_err(|_| error!(EscrowError::InvalidEd25519Layout))?;
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&data[message_offset..message_offset + DIGEST_LEN]);
    let _signature = &data[signature_offset..signature_offset + SIGNATURE_LEN];
    Ok(Ed25519Approval { attester, digest })
}

fn range_in_data(len: usize, offset: usize, size: usize) -> bool {
    offset
        .checked_add(size)
        .map(|end| end <= len)
        .unwrap_or(false)
}

fn ranges_overlap(a: usize, a_len: usize, b: usize, b_len: usize) -> bool {
    let a_end = a.saturating_add(a_len);
    let b_end = b.saturating_add(b_len);
    a < b_end && b < a_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(pubkey: [u8; 32], signature: [u8; 64], digest: [u8; 32]) -> Vec<u8> {
        let pubkey_offset = HEADER_LEN + OFFSETS_LEN;
        let signature_offset = pubkey_offset + PUBKEY_LEN;
        let message_offset = signature_offset + SIGNATURE_LEN;
        let mut data = vec![1, 0];
        data.extend_from_slice(&(signature_offset as u16).to_le_bytes());
        data.extend_from_slice(&CURRENT_IX.to_le_bytes());
        data.extend_from_slice(&(pubkey_offset as u16).to_le_bytes());
        data.extend_from_slice(&CURRENT_IX.to_le_bytes());
        data.extend_from_slice(&(message_offset as u16).to_le_bytes());
        data.extend_from_slice(&(DIGEST_LEN as u16).to_le_bytes());
        data.extend_from_slice(&CURRENT_IX.to_le_bytes());
        data.extend_from_slice(&pubkey);
        data.extend_from_slice(&signature);
        data.extend_from_slice(&digest);
        data
    }

    #[test]
    fn accepts_well_formed_single_signature() {
        let parsed = parse_ed25519_instruction(&encode([3u8; 32], [9u8; 64], [4u8; 32])).unwrap();
        assert_eq!(parsed.attester, Pubkey::new_from_array([3u8; 32]));
        assert_eq!(parsed.digest, [4u8; 32]);
    }

    #[test]
    fn rejects_wrong_instruction_index() {
        let mut data = encode([3u8; 32], [9u8; 64], [4u8; 32]);
        data[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_ed25519_instruction(&data).is_err());
    }

    #[test]
    fn rejects_truncated_message() {
        let mut data = encode([3u8; 32], [9u8; 64], [4u8; 32]);
        data[12..14].copy_from_slice(&16u16.to_le_bytes());
        assert!(parse_ed25519_instruction(&data).is_err());
    }

    #[test]
    fn rejects_overlapping_offsets() {
        let mut data = encode([3u8; 32], [9u8; 64], [4u8; 32]);
        data[6..8].copy_from_slice(&2u16.to_le_bytes());
        assert!(parse_ed25519_instruction(&data).is_err());
    }
}
