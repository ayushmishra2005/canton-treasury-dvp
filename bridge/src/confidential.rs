use anyhow::{Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use solana_system_interface::instruction as system_instruction;
use solana_zk_sdk::encryption::auth_encryption::AeCiphertext;
use solana_zk_sdk::encryption::elgamal::{ElGamalCiphertext, ElGamalKeypair, ElGamalPubkey};
use solana_zk_sdk::zk_elgamal_proof_program::instruction::{
    close_context_state, ContextStateInfo, ProofInstruction,
};
use solana_zk_sdk::zk_elgamal_proof_program::proof_data::{
    BatchedGroupedCiphertext3HandlesValidityProofContext, BatchedRangeProofContext,
    CiphertextCommitmentEqualityProofContext,
};
use solana_zk_sdk::zk_elgamal_proof_program::state::ProofContextState;
use spl_token_2022::extension::confidential_transfer::ConfidentialTransferAccount;
use spl_token_2022::extension::{BaseStateWithExtensions, StateWithExtensions};
use spl_token_2022::state::Account as TokenAccount;
use spl_token_confidential_transfer_proof_generation::transfer::transfer_split_proof_data;
use std::mem::size_of;

pub const ZK_PROOF_PROGRAM_ID: Pubkey =
    solana_sdk::pubkey!("ZkE1Gama1Proof11111111111111111111111111111");

pub struct TransferProofs {
    pub equality: Keypair,
    pub validity: Keypair,
    pub range: Keypair,
    pub transfer_data: [u8; 167],
    pub decryptable: [u8; 36],
    pub auditor_lo: [u8; 64],
    pub auditor_hi: [u8; 64],
}

pub fn generate_transfer_proofs(
    rpc: &RpcClient,
    payer: &Keypair,
    source_elgamal: &ElGamalKeypair,
    source_aes: &solana_zk_sdk::encryption::auth_encryption::AeKey,
    source_token: &Pubkey,
    dest_elgamal: &ElGamalPubkey,
    amount: u64,
) -> Result<TransferProofs> {
    let source_acc = rpc.get_account(source_token)?;
    let source_state = StateWithExtensions::<TokenAccount>::unpack(&source_acc.data)?;
    let source_ext = source_state.get_extension::<ConfidentialTransferAccount>()?;
    let current_available: ElGamalCiphertext = source_ext
        .available_balance
        .try_into()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let current_decryptable: AeCiphertext = source_ext
        .decryptable_available_balance
        .try_into()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let proof_data = transfer_split_proof_data(
        &current_available,
        &current_decryptable,
        amount,
        source_elgamal,
        source_aes,
        dest_elgamal,
        None,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let current_plaintext = current_decryptable
        .decrypt(source_aes)
        .context("decrypt available balance")?;
    let new_plaintext = current_plaintext
        .checked_sub(amount)
        .context("insufficient confidential balance")?;
    let new_decryptable = source_aes.encrypt(new_plaintext);
    let decryptable_bytes: [u8; 36] = bytemuck_decryptable(&new_decryptable)?;
    let auditor_lo: [u8; 64] = bytemuck_ct(
        &proof_data
            .ciphertext_validity_proof_data_with_ciphertext
            .ciphertext_lo,
    )?;
    let auditor_hi: [u8; 64] = bytemuck_ct(
        &proof_data
            .ciphertext_validity_proof_data_with_ciphertext
            .ciphertext_hi,
    )?;
    let mut transfer_data = [0u8; 167];
    transfer_data[..36].copy_from_slice(&decryptable_bytes);
    transfer_data[36..100].copy_from_slice(&auditor_lo);
    transfer_data[100..164].copy_from_slice(&auditor_hi);

    let equality = Keypair::new();
    let validity = Keypair::new();
    let range = Keypair::new();
    let equality_size = size_of::<ProofContextState<CiphertextCommitmentEqualityProofContext>>();
    let validity_size =
        size_of::<ProofContextState<BatchedGroupedCiphertext3HandlesValidityProofContext>>();
    let range_size = size_of::<ProofContextState<BatchedRangeProofContext>>();
    let rent_eq = rpc.get_minimum_balance_for_rent_exemption(equality_size)?;
    let rent_va = rpc.get_minimum_balance_for_rent_exemption(validity_size)?;
    let rent_ra = rpc.get_minimum_balance_for_rent_exemption(range_size)?;
    let authority = payer.pubkey();
    let create_eq = system_instruction::create_account(
        &authority,
        &equality.pubkey(),
        rent_eq,
        equality_size as u64,
        &ZK_PROOF_PROGRAM_ID,
    );
    let create_va = system_instruction::create_account(
        &authority,
        &validity.pubkey(),
        rent_va,
        validity_size as u64,
        &ZK_PROOF_PROGRAM_ID,
    );
    let create_ra = system_instruction::create_account(
        &authority,
        &range.pubkey(),
        rent_ra,
        range_size as u64,
        &ZK_PROOF_PROGRAM_ID,
    );
    let eq_ctx = ContextStateInfo {
        context_state_account: &equality.pubkey(),
        context_state_authority: &authority,
    };
    let va_ctx = ContextStateInfo {
        context_state_account: &validity.pubkey(),
        context_state_authority: &authority,
    };
    let ra_ctx = ContextStateInfo {
        context_state_account: &range.pubkey(),
        context_state_authority: &authority,
    };
    let verify_eq = ProofInstruction::VerifyCiphertextCommitmentEquality
        .encode_verify_proof(Some(eq_ctx), &proof_data.equality_proof_data);
    let verify_va = ProofInstruction::VerifyBatchedGroupedCiphertext3HandlesValidity
        .encode_verify_proof(
            Some(va_ctx),
            &proof_data
                .ciphertext_validity_proof_data_with_ciphertext
                .proof_data,
        );
    let verify_ra = ProofInstruction::VerifyBatchedRangeProofU128
        .encode_verify_proof(Some(ra_ctx), &proof_data.range_proof_data);

    send(
        rpc,
        &[create_eq, create_va, create_ra, verify_va],
        &[payer, &equality, &validity, &range],
    )?;
    send(rpc, &[verify_ra], &[payer])?;
    send(rpc, &[verify_eq], &[payer])?;

    let _ = close_context_state;
    Ok(TransferProofs {
        equality,
        validity,
        range,
        transfer_data,
        decryptable: decryptable_bytes,
        auditor_lo,
        auditor_hi,
    })
}

fn send(rpc: &RpcClient, ixs: &[Instruction], signers: &[&Keypair]) -> Result<()> {
    let blockhash = rpc.get_latest_blockhash()?;
    let tx =
        Transaction::new_signed_with_payer(ixs, Some(&signers[0].pubkey()), signers, blockhash);
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

pub fn decryptable_bytes(value: &AeCiphertext) -> Result<[u8; 36]> {
    let pod: solana_zk_sdk::encryption::pod::auth_encryption::PodAeCiphertext = (*value).into();
    let bytes = bytemuck::bytes_of(&pod);
    anyhow::ensure!(bytes.len() == 36, "decryptable length {}", bytes.len());
    let mut out = [0u8; 36];
    out.copy_from_slice(bytes);
    Ok(out)
}

fn bytemuck_decryptable(value: &AeCiphertext) -> Result<[u8; 36]> {
    decryptable_bytes(value)
}

fn bytemuck_ct<T: Copy + bytemuck::Pod>(value: &T) -> Result<[u8; 64]> {
    let bytes = bytemuck::bytes_of(value);
    anyhow::ensure!(bytes.len() == 64, "ciphertext length");
    let mut out = [0u8; 64];
    out.copy_from_slice(bytes);
    Ok(out)
}
