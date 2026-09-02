use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{invoke, invoke_signed};
use spl_token_2022::extension::{
    confidential_transfer::ConfidentialTransferAccount, BaseStateWithExtensions,
    PodStateWithExtensions,
};
use spl_token_2022::pod::PodAccount;

use crate::errors::EscrowError;

pub const TOKEN_2022_PROGRAM_ID: Pubkey = spl_token_2022::ID;
pub const CONFIDENTIAL_TRANSFER_PREFIX: u8 = 27;
pub const CONFIDENTIAL_TRANSFER_IX: u8 = 7;
pub const TRANSFER_DATA_LEN: usize = 167;

#[derive(Clone, Copy)]
pub struct TransferCiphertexts {
    pub decryptable: [u8; 36],
    pub auditor_lo: [u8; 64],
    pub auditor_hi: [u8; 64],
}

pub fn require_token_2022(program: &Pubkey) -> Result<()> {
    require_keys_eq!(
        *program,
        TOKEN_2022_PROGRAM_ID,
        EscrowError::WrongTokenProgram
    );
    Ok(())
}

pub fn require_confidential_account(
    token_program: &Pubkey,
    expected_mint: &Pubkey,
    expected_owner: Option<&Pubkey>,
    account: &AccountInfo,
    require_no_public_credits: bool,
) -> Result<()> {
    require_token_2022(token_program)?;
    require_keys_eq!(
        *account.owner,
        TOKEN_2022_PROGRAM_ID,
        EscrowError::WrongTokenProgram
    );
    let data = account.try_borrow_data()?;
    let state = PodStateWithExtensions::<PodAccount>::unpack(&data)
        .map_err(|_| error!(EscrowError::ConfidentialConfig))?;
    require!(
        state.base.mint.as_ref() == expected_mint.as_ref(),
        EscrowError::WrongMint
    );
    if let Some(owner) = expected_owner {
        require!(
            state.base.owner.as_ref() == owner.as_ref(),
            EscrowError::WrongVaultAuthority
        );
    }
    let confidential = state
        .get_extension::<ConfidentialTransferAccount>()
        .map_err(|_| error!(EscrowError::ConfidentialConfig))?;
    if require_no_public_credits {
        require!(
            !bool::from(confidential.allow_non_confidential_credits),
            EscrowError::NonConfidentialCreditsEnabled
        );
    }
    Ok(())
}

pub fn parse_transfer_ciphertexts(data: &[u8]) -> Result<TransferCiphertexts> {
    require!(
        data.len() == TRANSFER_DATA_LEN,
        EscrowError::WrongTransferInstruction
    );
    let mut decryptable = [0u8; 36];
    let mut auditor_lo = [0u8; 64];
    let mut auditor_hi = [0u8; 64];
    decryptable.copy_from_slice(&data[0..36]);
    auditor_lo.copy_from_slice(&data[36..100]);
    auditor_hi.copy_from_slice(&data[100..164]);
    require!(
        data[164] == 0 && data[165] == 0 && data[166] == 0,
        EscrowError::InlineProofsForbidden
    );
    Ok(TransferCiphertexts {
        decryptable,
        auditor_lo,
        auditor_hi,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn confidential_transfer_cpi<'info>(
    token_program: AccountInfo<'info>,
    source: AccountInfo<'info>,
    mint: AccountInfo<'info>,
    destination: AccountInfo<'info>,
    equality_ctx: AccountInfo<'info>,
    validity_ctx: AccountInfo<'info>,
    range_ctx: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    transfer_data: &[u8],
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    require_token_2022(token_program.key)?;
    require!(
        transfer_data.len() == TRANSFER_DATA_LEN,
        EscrowError::WrongTransferInstruction
    );
    let mut data = Vec::with_capacity(2 + TRANSFER_DATA_LEN);
    data.push(CONFIDENTIAL_TRANSFER_PREFIX);
    data.push(CONFIDENTIAL_TRANSFER_IX);
    data.extend_from_slice(transfer_data);
    let accounts = vec![
        AccountMeta::new(*source.key, false),
        AccountMeta::new_readonly(*mint.key, false),
        AccountMeta::new(*destination.key, false),
        AccountMeta::new_readonly(*equality_ctx.key, false),
        AccountMeta::new_readonly(*validity_ctx.key, false),
        AccountMeta::new_readonly(*range_ctx.key, false),
        AccountMeta::new_readonly(*authority.key, true),
    ];
    let ix = Instruction {
        program_id: TOKEN_2022_PROGRAM_ID,
        accounts,
        data,
    };
    let infos = &[
        source,
        mint,
        destination,
        equality_ctx,
        validity_ctx,
        range_ctx,
        authority,
        token_program,
    ];
    if signer_seeds.is_empty() {
        invoke(&ix, infos)?;
    } else {
        invoke_signed(&ix, infos, signer_seeds)?;
    }
    Ok(())
}

pub fn apply_pending_cpi<'info>(
    token_program: AccountInfo<'info>,
    account: AccountInfo<'info>,
    authority: AccountInfo<'info>,
    expected_counter: u64,
    new_decryptable: &[u8; 36],
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    require_token_2022(token_program.key)?;
    let mut data = Vec::with_capacity(2 + 8 + 36);
    data.push(CONFIDENTIAL_TRANSFER_PREFIX);
    data.push(8);
    data.extend_from_slice(&expected_counter.to_le_bytes());
    data.extend_from_slice(new_decryptable);
    let ix = Instruction {
        program_id: TOKEN_2022_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*account.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data,
    };
    let infos = &[account, authority, token_program];
    invoke_signed(&ix, infos, signer_seeds)?;
    Ok(())
}

pub fn pending_credit_counter(account: &AccountInfo) -> Result<u64> {
    let data = account.try_borrow_data()?;
    let state = PodStateWithExtensions::<PodAccount>::unpack(&data)
        .map_err(|_| error!(EscrowError::ConfidentialConfig))?;
    let confidential = state
        .get_extension::<ConfidentialTransferAccount>()
        .map_err(|_| error!(EscrowError::ConfidentialConfig))?;
    Ok(u64::from(confidential.pending_balance_credit_counter))
}
