use anyhow::Result;
use borsh::BorshSerialize;
use sha2::{Digest, Sha256};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;

pub const PROGRAM_ID: Pubkey = solana_sdk::pubkey!("CQrz5E2egFB8AyHDBVGrCai3c1msyyXsgmD6BuhXQpQd");
pub const CONFIG_SEED: &[u8] = b"bridge-config";
pub const VAULT_AUTHORITY_SEED: &[u8] = b"vault-authority";
pub const RECEIPT_SEED: &[u8] = b"receipt";
pub const APPROVAL_SEED: &[u8] = b"approval";

fn disc(name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("global:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash[..8]);
    out
}

pub fn config_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CONFIG_SEED], &PROGRAM_ID)
}

pub fn vault_authority_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_AUTHORITY_SEED], &PROGRAM_ID)
}

pub fn receipt_pda(operation: &[u8; 32]) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[RECEIPT_SEED, operation], &PROGRAM_ID)
}

pub fn approval_pda(operation: &[u8; 32], direction: u8) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[APPROVAL_SEED, operation, &[direction]], &PROGRAM_ID)
}

#[derive(BorshSerialize)]
pub struct ApproveFields {
    pub operation: [u8; 32],
    pub direction: u8,
    pub destination: Pubkey,
    pub amount_commitment: [u8; 32],
    pub zama_reservation: [u8; 32],
    pub previous_operation: [u8; 32],
    pub expiry: i64,
    pub proof_commitment: [u8; 32],
}

#[derive(BorshSerialize)]
pub struct LockFields {
    pub operation: [u8; 32],
    pub destination: Pubkey,
    pub amount_commitment: [u8; 32],
    pub zama_reservation: [u8; 32],
    pub previous_operation: [u8; 32],
    pub expiry: i64,
    pub transfer_data: [u8; 167],
    pub vault_decryptable: [u8; 36],
}

#[derive(BorshSerialize)]
pub struct MovementFields {
    pub destination: Pubkey,
    pub amount_commitment: [u8; 32],
    pub zama_reservation: [u8; 32],
    pub previous_operation: [u8; 32],
    pub expiry: i64,
    pub transfer_data: [u8; 167],
}

pub fn initialize_ix(
    payer: Pubkey,
    mint: Pubkey,
    vault: Pubkey,
    token_program: Pubkey,
    chain_id: [u8; 32],
    attesters: [Pubkey; 3],
) -> Result<Instruction> {
    let (config, _) = config_pda();
    let (vault_authority, _) = vault_authority_pda();
    let mut data = disc("initialize").to_vec();
    data.extend_from_slice(&chain_id);
    for attester in attesters {
        data.extend_from_slice(attester.as_ref());
    }
    Ok(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(config, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        ],
        data,
    })
}

pub fn approve_ix(payer: Pubkey, args: &ApproveFields) -> Result<Instruction> {
    let (config, _) = config_pda();
    let (receipt, _) = receipt_pda(&args.operation);
    let (approval, _) = approval_pda(&args.operation, args.direction);
    let mut data = disc("approve_operation").to_vec();
    data.extend_from_slice(&args.try_to_vec()?);
    Ok(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(config, false),
            AccountMeta::new(receipt, false),
            AccountMeta::new(approval, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        ],
        data,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn lock_ix(
    payer: Pubkey,
    source_authority: Pubkey,
    source: Pubkey,
    vault: Pubkey,
    mint: Pubkey,
    token_program: Pubkey,
    equality: Pubkey,
    validity: Pubkey,
    range: Pubkey,
    args: &LockFields,
) -> Result<Instruction> {
    let (config, _) = config_pda();
    let (receipt, _) = receipt_pda(&args.operation);
    let (vault_authority, _) = vault_authority_pda();
    let mut data = disc("lock_confidential").to_vec();
    data.extend_from_slice(&args.try_to_vec()?);
    Ok(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(source_authority, true),
            AccountMeta::new_readonly(config, false),
            AccountMeta::new(receipt, false),
            AccountMeta::new(payer, true),
            AccountMeta::new(source, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new_readonly(equality, false),
            AccountMeta::new_readonly(validity, false),
            AccountMeta::new_readonly(range, false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        ],
        data,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn move_ix(
    name: &str,
    vault: Pubkey,
    destination: Pubkey,
    mint: Pubkey,
    token_program: Pubkey,
    equality: Pubkey,
    validity: Pubkey,
    range: Pubkey,
    operation: &[u8; 32],
    direction: u8,
    args: &MovementFields,
) -> Result<Instruction> {
    let (config, _) = config_pda();
    let (receipt, _) = receipt_pda(operation);
    let (approval, _) = approval_pda(operation, direction);
    let (vault_authority, _) = vault_authority_pda();
    let mut data = disc(name).to_vec();
    data.extend_from_slice(&args.try_to_vec()?);
    Ok(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(config, false),
            AccountMeta::new(receipt, false),
            AccountMeta::new(approval, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(destination, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new_readonly(equality, false),
            AccountMeta::new_readonly(validity, false),
            AccountMeta::new_readonly(range, false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data,
    })
}
