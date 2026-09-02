use anchor_lang::prelude::*;

use crate::canonical::{
    cancel_permitted, operation_digest, proof_commitment, release_permitted, DIRECTION_CANCEL,
    DIRECTION_LOCK, DIRECTION_MINT, DIRECTION_RELEASE, RECEIPT_CANCELLED, RECEIPT_LOCKED,
    RECEIPT_MINT_AUTHORIZED, RECEIPT_RELEASED,
};
use crate::confidential::{
    apply_pending_cpi, confidential_transfer_cpi, parse_transfer_ciphertexts,
    pending_credit_counter, require_confidential_account, require_token_2022,
};
use crate::ed25519::read_preceding_ed25519;
use crate::errors::EscrowError;
use crate::state::{
    BridgeConfig, LockReceipt, OperationApproval, APPROVAL_SEED, CONFIG_SEED, RECEIPT_SEED,
    VAULT_AUTHORITY_SEED,
};

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitializeArgs {
    pub chain_id: [u8; 32],
    pub attesters: [Pubkey; 3],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct LockArgs {
    pub operation: [u8; 32],
    pub destination: Pubkey,
    pub amount_commitment: [u8; 32],
    pub zama_reservation: [u8; 32],
    pub previous_operation: [u8; 32],
    pub expiry: i64,
    pub transfer_data: [u8; 167],
    pub vault_decryptable: [u8; 36],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ApproveArgs {
    pub operation: [u8; 32],
    pub direction: u8,
    pub destination: Pubkey,
    pub amount_commitment: [u8; 32],
    pub zama_reservation: [u8; 32],
    pub previous_operation: [u8; 32],
    pub expiry: i64,
    pub proof_commitment: [u8; 32],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct MovementArgs {
    pub destination: Pubkey,
    pub amount_commitment: [u8; 32],
    pub zama_reservation: [u8; 32],
    pub previous_operation: [u8; 32],
    pub expiry: i64,
    pub transfer_data: [u8; 167],
}

pub fn initialize(ctx: Context<Initialize>, args: InitializeArgs) -> Result<()> {
    require_token_2022(&ctx.accounts.token_program.key())?;
    require!(
        args.attesters[0] != args.attesters[1]
            && args.attesters[0] != args.attesters[2]
            && args.attesters[1] != args.attesters[2],
        EscrowError::InvalidAttesterSet
    );
    let config = &mut ctx.accounts.config;
    config.chain_id = args.chain_id;
    config.token_program = ctx.accounts.token_program.key();
    config.mint = ctx.accounts.mint.key();
    config.vault = ctx.accounts.vault.key();
    config.attesters = args.attesters;
    config.bump = ctx.bumps.config;
    config.vault_authority_bump = ctx.bumps.vault_authority;
    let vault_authority = ctx.accounts.vault_authority.key();
    require_confidential_account(
        &config.token_program,
        &config.mint,
        Some(&vault_authority),
        &ctx.accounts.vault,
        true,
    )?;
    Ok(())
}

pub fn lock_confidential(ctx: Context<LockConfidential>, args: LockArgs) -> Result<()> {
    let config = &ctx.accounts.config;
    require_keys_eq!(config.mint, ctx.accounts.mint.key(), EscrowError::WrongMint);
    require_keys_eq!(
        config.vault,
        ctx.accounts.vault.key(),
        EscrowError::WrongVault
    );
    require_keys_eq!(
        config.token_program,
        ctx.accounts.token_program.key(),
        EscrowError::WrongTokenProgram
    );
    require!(
        Clock::get()?.unix_timestamp < args.expiry,
        EscrowError::Expired
    );
    require_confidential_account(
        &config.token_program,
        &config.mint,
        None,
        &ctx.accounts.source,
        false,
    )?;
    require_confidential_account(
        &config.token_program,
        &config.mint,
        Some(&ctx.accounts.vault_authority.key()),
        &ctx.accounts.vault,
        true,
    )?;
    let ciphertexts = parse_transfer_ciphertexts(&args.transfer_data)?;
    let lock_proof = proof_commitment(
        &ciphertexts.decryptable,
        &ciphertexts.auditor_lo,
        &ciphertexts.auditor_hi,
        &ctx.accounts.equality_proof_context.key(),
        &ctx.accounts.validity_proof_context.key(),
        &ctx.accounts.range_proof_context.key(),
    );
    confidential_transfer_cpi(
        ctx.accounts.token_program.to_account_info(),
        ctx.accounts.source.to_account_info(),
        ctx.accounts.mint.to_account_info(),
        ctx.accounts.vault.to_account_info(),
        ctx.accounts.equality_proof_context.to_account_info(),
        ctx.accounts.validity_proof_context.to_account_info(),
        ctx.accounts.range_proof_context.to_account_info(),
        ctx.accounts.source_authority.to_account_info(),
        &args.transfer_data,
        &[],
    )?;
    let pending = pending_credit_counter(&ctx.accounts.vault)?;
    apply_pending_cpi(
        ctx.accounts.token_program.to_account_info(),
        ctx.accounts.vault.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        pending,
        &args.vault_decryptable,
        &[&[
            VAULT_AUTHORITY_SEED,
            &[ctx.accounts.config.vault_authority_bump],
        ]],
    )?;
    let receipt = &mut ctx.accounts.receipt;
    receipt.operation = args.operation;
    receipt.destination = args.destination;
    receipt.amount_commitment = args.amount_commitment;
    receipt.zama_reservation = args.zama_reservation;
    receipt.previous_operation = args.previous_operation;
    receipt.expiry = args.expiry;
    receipt.lock_proof_commitment = lock_proof;
    receipt.status = RECEIPT_LOCKED;
    receipt.bump = ctx.bumps.receipt;
    let _ = DIRECTION_LOCK;
    Ok(())
}

pub fn approve_operation(ctx: Context<ApproveOperation>, args: ApproveArgs) -> Result<()> {
    require!(
        args.direction == DIRECTION_MINT
            || args.direction == DIRECTION_CANCEL
            || args.direction == DIRECTION_RELEASE,
        EscrowError::InvalidReceiptState
    );
    let config = &ctx.accounts.config;
    let receipt = &mut ctx.accounts.receipt;
    require!(
        receipt.operation == args.operation,
        EscrowError::ReceiptMismatch
    );
    require!(
        receipt.amount_commitment == args.amount_commitment,
        EscrowError::AmountCommitmentMismatch
    );
    require!(
        receipt.zama_reservation == args.zama_reservation,
        EscrowError::ReservationMismatch
    );
    require!(
        receipt.previous_operation == args.previous_operation,
        EscrowError::PreviousOperationMismatch
    );
    let now = Clock::get()?.unix_timestamp;
    if args.direction == DIRECTION_MINT {
        require!(now < receipt.expiry, EscrowError::Expired);
        require!(args.expiry == receipt.expiry, EscrowError::ReceiptMismatch);
    } else {
        require!(now < args.expiry, EscrowError::Expired);
    }
    if args.direction == DIRECTION_CANCEL {
        require!(
            receipt.destination == args.destination,
            EscrowError::WrongDestination
        );
    }
    let expected = operation_digest(
        &config.chain_id,
        ctx.program_id,
        &ctx.accounts.config.key(),
        &args.operation,
        args.direction,
        &receipt.key(),
        &args.destination,
        &args.amount_commitment,
        &args.zama_reservation,
        &args.previous_operation,
        args.expiry,
        &args.proof_commitment,
    );
    let signed = read_preceding_ed25519(&ctx.accounts.instructions_sysvar)?;
    require!(signed.digest == expected, EscrowError::DigestMismatch);
    let index = config
        .attester_index(&signed.attester)
        .ok_or(error!(EscrowError::UnknownAttester))?;
    let approval = &mut ctx.accounts.approval;
    let empty = approval.signer_bitmap == 0 && approval.digest == [0u8; 32];
    let expired = !empty && now >= approval.expiry;
    if empty || (expired && !approval.consumed) {
        require!(!approval.consumed, EscrowError::ApprovalConsumed);
        approval.operation = args.operation;
        approval.direction = args.direction;
        approval.digest = expected;
        approval.consumed = false;
        approval.signer_bitmap = 0;
        approval.expiry = args.expiry;
        approval.bump = ctx.bumps.approval;
    } else {
        require!(!approval.consumed, EscrowError::ApprovalConsumed);
        require!(!expired, EscrowError::Expired);
        require!(
            approval.digest == expected,
            EscrowError::ApprovalDigestMismatch
        );
        require!(
            approval.direction == args.direction,
            EscrowError::ApprovalDigestMismatch
        );
        require!(
            approval.operation == args.operation,
            EscrowError::ReceiptMismatch
        );
        require!(approval.expiry == args.expiry, EscrowError::ReceiptMismatch);
    }
    let bit = 1u8 << index;
    require!(
        approval.signer_bitmap & bit == 0,
        EscrowError::DuplicateAttester
    );
    approval.signer_bitmap |= bit;
    if args.direction == DIRECTION_MINT && approval.distinct_signers() >= 2 {
        require!(
            receipt.status == RECEIPT_LOCKED || receipt.status == RECEIPT_MINT_AUTHORIZED,
            EscrowError::InvalidReceiptState
        );
        receipt.status = RECEIPT_MINT_AUTHORIZED;
    }
    Ok(())
}

pub fn cancel_confidential(ctx: Context<MoveConfidential>, args: MovementArgs) -> Result<()> {
    move_vault_tokens(ctx, args, DIRECTION_CANCEL)
}

pub fn release_confidential(ctx: Context<MoveConfidential>, args: MovementArgs) -> Result<()> {
    move_vault_tokens(ctx, args, DIRECTION_RELEASE)
}

fn move_vault_tokens(
    ctx: Context<MoveConfidential>,
    args: MovementArgs,
    direction: u8,
) -> Result<()> {
    let config = &ctx.accounts.config;
    let receipt = &ctx.accounts.receipt;
    let approval = &ctx.accounts.approval;
    require_keys_eq!(
        config.vault,
        ctx.accounts.vault.key(),
        EscrowError::WrongVault
    );
    require_keys_eq!(config.mint, ctx.accounts.mint.key(), EscrowError::WrongMint);
    require_keys_eq!(
        ctx.accounts.destination.key(),
        args.destination,
        EscrowError::WrongDestination
    );
    let now = Clock::get()?.unix_timestamp;
    require!(now < args.expiry, EscrowError::Expired);
    require!(now < approval.expiry, EscrowError::Expired);
    require!(approval.expiry == args.expiry, EscrowError::ReceiptMismatch);
    require!(!approval.consumed, EscrowError::ApprovalConsumed);
    require!(
        approval.direction == direction,
        EscrowError::ApprovalDigestMismatch
    );
    require!(
        approval.distinct_signers() >= 2,
        EscrowError::AttestationThreshold
    );
    require!(
        receipt.status != RECEIPT_CANCELLED,
        EscrowError::InvalidReceiptState
    );
    require!(
        receipt.status != RECEIPT_RELEASED,
        EscrowError::InvalidReceiptState
    );
    if direction == DIRECTION_CANCEL {
        require!(
            receipt.status != RECEIPT_MINT_AUTHORIZED,
            EscrowError::CancelAfterMint
        );
        require!(
            cancel_permitted(receipt.status),
            EscrowError::InvalidReceiptState
        );
    } else {
        require!(
            release_permitted(receipt.status),
            EscrowError::InvalidReceiptState
        );
    }
    require!(
        receipt.amount_commitment == args.amount_commitment,
        EscrowError::AmountCommitmentMismatch
    );
    require!(
        receipt.zama_reservation == args.zama_reservation,
        EscrowError::ReservationMismatch
    );
    require!(
        receipt.previous_operation == args.previous_operation,
        EscrowError::PreviousOperationMismatch
    );
    if direction == DIRECTION_CANCEL {
        require!(
            receipt.destination == args.destination,
            EscrowError::WrongDestination
        );
    }
    require_confidential_account(
        &config.token_program,
        &config.mint,
        Some(&ctx.accounts.vault_authority.key()),
        &ctx.accounts.vault,
        true,
    )?;
    require_confidential_account(
        &config.token_program,
        &config.mint,
        None,
        &ctx.accounts.destination,
        false,
    )?;
    let ciphertexts = parse_transfer_ciphertexts(&args.transfer_data)?;
    let computed = proof_commitment(
        &ciphertexts.decryptable,
        &ciphertexts.auditor_lo,
        &ciphertexts.auditor_hi,
        &ctx.accounts.equality_proof_context.key(),
        &ctx.accounts.validity_proof_context.key(),
        &ctx.accounts.range_proof_context.key(),
    );
    let expected = operation_digest(
        &config.chain_id,
        ctx.program_id,
        &ctx.accounts.config.key(),
        &receipt.operation,
        direction,
        &receipt.key(),
        &args.destination,
        &args.amount_commitment,
        &args.zama_reservation,
        &args.previous_operation,
        args.expiry,
        &computed,
    );
    require!(
        approval.digest == expected,
        EscrowError::ProofCommitmentMismatch
    );
    let bump = config.vault_authority_bump;
    let seeds: &[&[u8]] = &[VAULT_AUTHORITY_SEED, &[bump]];
    confidential_transfer_cpi(
        ctx.accounts.token_program.to_account_info(),
        ctx.accounts.vault.to_account_info(),
        ctx.accounts.mint.to_account_info(),
        ctx.accounts.destination.to_account_info(),
        ctx.accounts.equality_proof_context.to_account_info(),
        ctx.accounts.validity_proof_context.to_account_info(),
        ctx.accounts.range_proof_context.to_account_info(),
        ctx.accounts.vault_authority.to_account_info(),
        &args.transfer_data,
        &[seeds],
    )?;
    let approval = &mut ctx.accounts.approval;
    let receipt = &mut ctx.accounts.receipt;
    approval.consumed = true;
    receipt.status = if direction == DIRECTION_CANCEL {
        RECEIPT_CANCELLED
    } else {
        RECEIPT_RELEASED
    };
    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + BridgeConfig::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, BridgeConfig>,
    /// CHECK: Token-2022 mint, validated by extension checks
    pub mint: UncheckedAccount<'info>,
    /// CHECK: confidential vault token account
    pub vault: UncheckedAccount<'info>,
    /// CHECK: PDA authority for the vault
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump)]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: must be Token-2022
    pub token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(args: LockArgs)]
pub struct LockConfidential<'info> {
    pub source_authority: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, BridgeConfig>,
    #[account(
        init,
        payer = payer,
        space = 8 + LockReceipt::INIT_SPACE,
        seeds = [RECEIPT_SEED, args.operation.as_ref()],
        bump
    )]
    pub receipt: Account<'info, LockReceipt>,
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: confidential source
    #[account(mut)]
    pub source: UncheckedAccount<'info>,
    /// CHECK: configured vault
    #[account(mut)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: mint
    pub mint: UncheckedAccount<'info>,
    /// CHECK: vault authority PDA
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump = config.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: equality proof context
    pub equality_proof_context: UncheckedAccount<'info>,
    /// CHECK: ciphertext validity proof context
    pub validity_proof_context: UncheckedAccount<'info>,
    /// CHECK: range proof context
    pub range_proof_context: UncheckedAccount<'info>,
    /// CHECK: Token-2022
    pub token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(args: ApproveArgs)]
pub struct ApproveOperation<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, BridgeConfig>,
    #[account(
        mut,
        seeds = [RECEIPT_SEED, args.operation.as_ref()],
        bump = receipt.bump
    )]
    pub receipt: Account<'info, LockReceipt>,
    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + OperationApproval::INIT_SPACE,
        seeds = [APPROVAL_SEED, args.operation.as_ref(), &[args.direction]],
        bump
    )]
    pub approval: Account<'info, OperationApproval>,
    /// CHECK: instructions sysvar
    pub instructions_sysvar: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MoveConfidential<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, BridgeConfig>,
    #[account(
        mut,
        seeds = [RECEIPT_SEED, receipt.operation.as_ref()],
        bump = receipt.bump
    )]
    pub receipt: Account<'info, LockReceipt>,
    #[account(
        mut,
        seeds = [APPROVAL_SEED, receipt.operation.as_ref(), &[approval.direction]],
        bump = approval.bump
    )]
    pub approval: Account<'info, OperationApproval>,
    /// CHECK: vault
    #[account(mut)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: destination
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    /// CHECK: mint
    pub mint: UncheckedAccount<'info>,
    /// CHECK: vault authority
    #[account(seeds = [VAULT_AUTHORITY_SEED], bump = config.vault_authority_bump)]
    pub vault_authority: UncheckedAccount<'info>,
    /// CHECK: equality proof context
    pub equality_proof_context: UncheckedAccount<'info>,
    /// CHECK: ciphertext validity proof context
    pub validity_proof_context: UncheckedAccount<'info>,
    /// CHECK: range proof context
    pub range_proof_context: UncheckedAccount<'info>,
    /// CHECK: Token-2022
    pub token_program: UncheckedAccount<'info>,
}
