use anchor_lang::prelude::*;

#[error_code]
pub enum EscrowError {
    #[msg("Token program must be Token-2022")]
    WrongTokenProgram,
    #[msg("Mint does not match the bridge configuration")]
    WrongMint,
    #[msg("Source token account is not valid for this operation")]
    WrongSource,
    #[msg("Destination token account is not valid for this operation")]
    WrongDestination,
    #[msg("Vault token account is not the configured confidential vault")]
    WrongVault,
    #[msg("Vault authority PDA does not match")]
    WrongVaultAuthority,
    #[msg("Confidential transfer account is missing or misconfigured")]
    ConfidentialConfig,
    #[msg("Vault must reject non-confidential credits")]
    NonConfidentialCreditsEnabled,
    #[msg("Transfer instruction must be ConfidentialTransfer::Transfer")]
    WrongTransferInstruction,
    #[msg("Proof locations must be pre-verified context accounts")]
    InlineProofsForbidden,
    #[msg("Two distinct configured attesters are required")]
    AttestationThreshold,
    #[msg("Attester is not in the configured set")]
    UnknownAttester,
    #[msg("Attester already approved this operation")]
    DuplicateAttester,
    #[msg("Ed25519 instruction is missing or not immediately preceding")]
    MissingEd25519,
    #[msg("Ed25519 instruction offsets or indices are invalid")]
    InvalidEd25519Layout,
    #[msg("Ed25519 message is not the approved digest")]
    DigestMismatch,
    #[msg("Approval digest does not match the bound operation")]
    ApprovalDigestMismatch,
    #[msg("Approval has already been consumed")]
    ApprovalConsumed,
    #[msg("Lock receipt does not match the bound operation")]
    ReceiptMismatch,
    #[msg("Operation is not in a state that allows this action")]
    InvalidReceiptState,
    #[msg("Cancellation is rejected after mint authorization")]
    CancelAfterMint,
    #[msg("Proof commitment does not match the transfer ciphertexts")]
    ProofCommitmentMismatch,
    #[msg("Operation expiry has passed")]
    Expired,
    #[msg("Bridge configuration does not match this program or chain")]
    WrongBridgeBinding,
    #[msg("Previous operation binding does not match")]
    PreviousOperationMismatch,
    #[msg("Zama reservation binding does not match")]
    ReservationMismatch,
    #[msg("Amount commitment binding does not match")]
    AmountCommitmentMismatch,
    #[msg("Attester set must contain three distinct keys")]
    InvalidAttesterSet,
}
