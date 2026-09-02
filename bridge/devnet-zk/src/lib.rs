//! Off-chain confidential-transfer proofs for the current Devnet ZK verifier.
//!
//! Proof bytes are generated with `solana-zk-sdk` 6.0.1 and
//! `spl-token-confidential-transfer-proof-generation` 0.6.0, matching
//! solana-foundation/Confidential-Balances-Sample commit
//! `5e9b23dfe2a81ab7732ac12aaeba5d05abd7c1d3` (2026-07-09). Sources were
//! checked on 2026-09-02. The public API is byte arrays and raw instructions
//! so the coordinator can stay on its existing Solana 2.2 / Anchor line.

use anyhow::{anyhow, Context, Result};
use bytemuck::Pod;
use solana_address::Address;
use solana_zk_elgamal_proof_interface::instruction::{
    close_context_state, ContextStateInfo, ProofInstruction,
};
use solana_zk_elgamal_proof_interface::proof_data::{
    BatchedGroupedCiphertext3HandlesValidityProofContext,
    BatchedGroupedCiphertext3HandlesValidityProofData, BatchedRangeProofContext,
    CiphertextCommitmentEqualityProofContext, CiphertextCommitmentEqualityProofData,
    PubkeyValidityProofContext, PubkeyValidityProofData,
};
use solana_zk_elgamal_proof_interface::state::ProofContextState;
use solana_zk_sdk::encryption::auth_encryption::{AeCiphertext, AeKey};
use solana_zk_sdk::encryption::elgamal::{
    ElGamalCiphertext, ElGamalKeypair, ElGamalPubkey, ElGamalSecretKey,
};
use solana_zk_sdk::zk_elgamal_proof_program::pubkey_validity::build_pubkey_validity_proof_data;
use solana_zk_sdk::zk_elgamal_proof_program::VerifyZkProof;
use solana_zk_sdk_pod::encryption::elgamal::{
    PodElGamalCiphertext as PodElGamalCiphertextV6, PodElGamalPubkey as PodElGamalPubkeyV6,
};
use spl_token_confidential_transfer_proof_generation::transfer::transfer_split_proof_data;
use std::mem::size_of;
use zeroize::Zeroize;

pub fn zk_proof_program_id() -> [u8; 32] {
    solana_zk_elgamal_proof_interface::ID.to_bytes()
}

pub fn record_program_id() -> [u8; 32] {
    "recr1L3PCGKLbckBqMNcJhuuyU1zgo8nBhfLVsJNwr5"
        .parse::<Address>()
        .expect("record program id")
        .to_bytes()
}

pub const ELGAMAL_SECRET_LEN: usize = 32;
pub const ELGAMAL_PUBKEY_LEN: usize = 32;
pub const AES_KEY_LEN: usize = 16;
pub const AE_CIPHERTEXT_LEN: usize = 36;
pub const ELGAMAL_CIPHERTEXT_LEN: usize = 64;
pub const TRANSFER_DATA_LEN: usize = 167;
pub const RECORD_PROOF_OFFSET: u32 = 33;
pub const RECORD_FIRST_CHUNK: usize = 750;
pub const RECORD_WRITE_CHUNK: usize = 900;

#[derive(Clone)]
pub struct ConfidentialKeys {
    pub elgamal_secret: [u8; ELGAMAL_SECRET_LEN],
    pub elgamal_pubkey: [u8; ELGAMAL_PUBKEY_LEN],
    pub aes_key: [u8; AES_KEY_LEN],
}

impl Drop for ConfidentialKeys {
    fn drop(&mut self) {
        self.elgamal_secret.zeroize();
        self.aes_key.zeroize();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawAccountMeta {
    pub pubkey: [u8; 32],
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawInstruction {
    pub program_id: [u8; 32],
    pub accounts: Vec<RawAccountMeta>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct InstructionBatch {
    pub instructions: Vec<RawInstruction>,
    pub signer_pubkeys: Vec<[u8; 32]>,
}

#[derive(Clone)]
pub struct TransferProofBundle {
    pub equality_proof: Vec<u8>,
    pub validity_proof: Vec<u8>,
    pub range_proof: Vec<u8>,
    pub decryptable: [u8; AE_CIPHERTEXT_LEN],
    pub auditor_lo: [u8; ELGAMAL_CIPHERTEXT_LEN],
    pub auditor_hi: [u8; ELGAMAL_CIPHERTEXT_LEN],
    pub transfer_data: [u8; TRANSFER_DATA_LEN],
}

pub fn generate_keys() -> Result<ConfidentialKeys> {
    let elgamal = ElGamalKeypair::new_rand();
    let aes = AeKey::new_rand();
    keys_from_native(&elgamal, &aes)
}

pub fn keys_from_secret(
    elgamal_secret: &[u8; ELGAMAL_SECRET_LEN],
    aes_key: &[u8; AES_KEY_LEN],
) -> Result<ConfidentialKeys> {
    let elgamal = elgamal_from_secret(elgamal_secret)?;
    let aes = aes_from_bytes(aes_key)?;
    keys_from_native(&elgamal, &aes)
}

pub fn encrypt_aes(aes_key: &[u8; AES_KEY_LEN], amount: u64) -> Result<[u8; AE_CIPHERTEXT_LEN]> {
    let aes = aes_from_bytes(aes_key)?;
    let ciphertext = aes.encrypt(amount);
    checked_array(ciphertext.to_bytes().as_slice(), "AES ciphertext")
}

pub fn decrypt_aes(aes_key: &[u8; AES_KEY_LEN], ciphertext: &[u8]) -> Result<u64> {
    let aes = aes_from_bytes(aes_key)?;
    let bytes = checked_array::<AE_CIPHERTEXT_LEN>(ciphertext, "AES ciphertext")?;
    let value = AeCiphertext::from_bytes(&bytes).ok_or_else(|| anyhow!("AES ciphertext bytes"))?;
    value
        .decrypt(&aes)
        .ok_or_else(|| anyhow!("AES decrypt failed"))
}

pub fn elgamal_pubkey(
    elgamal_secret: &[u8; ELGAMAL_SECRET_LEN],
) -> Result<[u8; ELGAMAL_PUBKEY_LEN]> {
    let elgamal = elgamal_from_secret(elgamal_secret)?;
    checked_array(elgamal.pubkey().to_bytes().as_slice(), "ElGamal pubkey")
}

pub fn generate_pubkey_validity_proof(
    elgamal_secret: &[u8; ELGAMAL_SECRET_LEN],
) -> Result<Vec<u8>> {
    let elgamal = elgamal_from_secret(elgamal_secret)?;
    let proof = build_pubkey_validity_proof_data(&elgamal)
        .map_err(|e| anyhow!("pubkey validity proof: {e}"))?;
    proof_bytes(&proof)
}

pub fn verify_pubkey_validity_proof(proof_bytes: &[u8]) -> Result<()> {
    let proof = parse_pubkey_validity_proof(proof_bytes)?;
    proof
        .verify_proof()
        .map_err(|e| anyhow!("pubkey validity verify: {e:?}"))
}

pub fn pubkey_validity_context_len() -> usize {
    size_of::<ProofContextState<PubkeyValidityProofContext>>()
}

pub fn equality_context_len() -> usize {
    size_of::<ProofContextState<CiphertextCommitmentEqualityProofContext>>()
}

pub fn validity_context_len() -> usize {
    size_of::<ProofContextState<BatchedGroupedCiphertext3HandlesValidityProofContext>>()
}

pub fn range_context_len() -> usize {
    size_of::<ProofContextState<BatchedRangeProofContext>>()
}

pub fn encode_verify_pubkey_validity(
    context_account: [u8; 32],
    context_authority: [u8; 32],
    proof_bytes: &[u8],
) -> Result<RawInstruction> {
    let proof = parse_proof::<PubkeyValidityProofData>(proof_bytes)?;
    let account = Address::from(context_account);
    let authority = Address::from(context_authority);
    to_raw(
        ProofInstruction::VerifyPubkeyValidity
            .encode_verify_proof(Some(context_info(&account, &authority)), &proof),
    )
}

pub fn encode_close_context(
    context_account: [u8; 32],
    context_authority: [u8; 32],
    destination: [u8; 32],
) -> Result<RawInstruction> {
    let account = Address::from(context_account);
    let authority = Address::from(context_authority);
    let dest = Address::from(destination);
    to_raw(close_context_state(
        context_info(&account, &authority),
        &dest,
    ))
}

pub fn generate_transfer_proofs(
    available_balance: &[u8],
    decryptable_available: &[u8],
    amount: u64,
    source_elgamal_secret: &[u8; ELGAMAL_SECRET_LEN],
    source_aes: &[u8; AES_KEY_LEN],
    dest_elgamal_pubkey: &[u8],
) -> Result<TransferProofBundle> {
    let source = elgamal_from_secret(source_elgamal_secret)?;
    let aes = aes_from_bytes(source_aes)?;
    let available = elgamal_ciphertext_from_bytes(available_balance)?;
    let decryptable = ae_ciphertext_from_bytes(decryptable_available)?;
    let dest = elgamal_pubkey_from_bytes(dest_elgamal_pubkey)?;
    let proof_data =
        transfer_split_proof_data(&available, &decryptable, amount, &source, &aes, &dest, None)
            .map_err(|e| anyhow!("transfer split proof: {e}"))?;

    let current_plaintext = decryptable
        .decrypt(&aes)
        .ok_or_else(|| anyhow!("decrypt available confidential balance"))?;
    let new_plaintext = current_plaintext
        .checked_sub(amount)
        .ok_or_else(|| anyhow!("insufficient confidential balance"))?;
    let new_decryptable = encrypt_aes(source_aes, new_plaintext)?;
    let auditor_lo = pod_bytes(
        &proof_data
            .ciphertext_validity_proof_data_with_ciphertext
            .ciphertext_lo,
    )?;
    let auditor_hi = pod_bytes(
        &proof_data
            .ciphertext_validity_proof_data_with_ciphertext
            .ciphertext_hi,
    )?;
    let mut transfer_data = [0u8; TRANSFER_DATA_LEN];
    transfer_data[..AE_CIPHERTEXT_LEN].copy_from_slice(&new_decryptable);
    transfer_data[AE_CIPHERTEXT_LEN..100].copy_from_slice(&auditor_lo);
    transfer_data[100..164].copy_from_slice(&auditor_hi);

    Ok(TransferProofBundle {
        equality_proof: proof_bytes(&proof_data.equality_proof_data)?,
        validity_proof: proof_bytes(
            &proof_data
                .ciphertext_validity_proof_data_with_ciphertext
                .proof_data,
        )?,
        range_proof: proof_bytes(&proof_data.range_proof_data)?,
        decryptable: new_decryptable,
        auditor_lo,
        auditor_hi,
        transfer_data,
    })
}

pub fn encode_verify_equality(
    context_account: [u8; 32],
    context_authority: [u8; 32],
    proof_bytes: &[u8],
) -> Result<RawInstruction> {
    let proof = parse_proof::<CiphertextCommitmentEqualityProofData>(proof_bytes)?;
    let account = Address::from(context_account);
    let authority = Address::from(context_authority);
    to_raw(
        ProofInstruction::VerifyCiphertextCommitmentEquality
            .encode_verify_proof(Some(context_info(&account, &authority)), &proof),
    )
}

pub fn encode_verify_validity(
    context_account: [u8; 32],
    context_authority: [u8; 32],
    proof_bytes: &[u8],
) -> Result<RawInstruction> {
    let proof = parse_proof::<BatchedGroupedCiphertext3HandlesValidityProofData>(proof_bytes)?;
    let account = Address::from(context_account);
    let authority = Address::from(context_authority);
    to_raw(
        ProofInstruction::VerifyBatchedGroupedCiphertext3HandlesValidity
            .encode_verify_proof(Some(context_info(&account, &authority)), &proof),
    )
}

pub fn encode_verify_range_from_record(
    context_account: [u8; 32],
    context_authority: [u8; 32],
    record_account: [u8; 32],
) -> Result<RawInstruction> {
    let account = Address::from(context_account);
    let authority = Address::from(context_authority);
    let record = Address::from(record_account);
    to_raw(
        ProofInstruction::VerifyBatchedRangeProofU128.encode_verify_proof_from_account(
            Some(context_info(&account, &authority)),
            &record,
            RECORD_PROOF_OFFSET,
        ),
    )
}

pub fn encode_record_initialize(record: [u8; 32], authority: [u8; 32]) -> Result<RawInstruction> {
    Ok(RawInstruction {
        program_id: record_program_id(),
        accounts: vec![
            RawAccountMeta {
                pubkey: record,
                is_signer: false,
                is_writable: true,
            },
            RawAccountMeta {
                pubkey: authority,
                is_signer: false,
                is_writable: false,
            },
        ],
        data: vec![0],
    })
}

pub fn encode_record_write(
    record: [u8; 32],
    authority: [u8; 32],
    offset: u64,
    bytes: &[u8],
) -> Result<RawInstruction> {
    let len = u32::try_from(bytes.len()).context("record write length")?;
    let mut data = Vec::with_capacity(1 + 8 + 4 + bytes.len());
    data.push(1);
    data.extend_from_slice(&offset.to_le_bytes());
    data.extend_from_slice(&len.to_le_bytes());
    data.extend_from_slice(bytes);
    Ok(RawInstruction {
        program_id: record_program_id(),
        accounts: vec![
            RawAccountMeta {
                pubkey: record,
                is_signer: false,
                is_writable: true,
            },
            RawAccountMeta {
                pubkey: authority,
                is_signer: true,
                is_writable: false,
            },
        ],
        data,
    })
}

pub fn encode_record_close(
    record: [u8; 32],
    destination: [u8; 32],
    authority: [u8; 32],
) -> Result<RawInstruction> {
    Ok(RawInstruction {
        program_id: record_program_id(),
        accounts: vec![
            RawAccountMeta {
                pubkey: record,
                is_signer: false,
                is_writable: true,
            },
            RawAccountMeta {
                pubkey: authority,
                is_signer: true,
                is_writable: false,
            },
            RawAccountMeta {
                pubkey: destination,
                is_signer: false,
                is_writable: true,
            },
        ],
        data: vec![3],
    })
}

pub fn range_record_chunks(proof_bytes: &[u8]) -> Result<Vec<(u64, Vec<u8>)>> {
    anyhow::ensure!(!proof_bytes.is_empty(), "range proof had no bytes");
    let first_len = proof_bytes.len().min(RECORD_FIRST_CHUNK);
    let mut chunks = Vec::new();
    chunks.push((0, proof_bytes[..first_len].to_vec()));
    let mut offset = first_len as u64;
    for chunk in proof_bytes[first_len..].chunks(RECORD_WRITE_CHUNK) {
        chunks.push((offset, chunk.to_vec()));
        offset = offset
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| anyhow!("range proof offset overflow"))?;
    }
    Ok(chunks)
}

pub fn range_record_space(proof_len: usize) -> usize {
    proof_len + RECORD_PROOF_OFFSET as usize
}

pub fn context_account_is_zk_owned(owner: &[u8]) -> bool {
    owner == zk_proof_program_id()
}

pub fn require_unused_context(
    used: &[[u8; 32]],
    context_account: &[u8; 32],
    token_account: &[u8; 32],
) -> Result<()> {
    if used.iter().any(|item| item == context_account) {
        return Err(anyhow!(
            "proof context {} was already used and cannot configure {}",
            hex::encode(context_account),
            hex::encode(token_account)
        ));
    }
    Ok(())
}

fn keys_from_native(elgamal: &ElGamalKeypair, aes: &AeKey) -> Result<ConfidentialKeys> {
    Ok(ConfidentialKeys {
        elgamal_secret: checked_array(elgamal.secret().as_bytes(), "ElGamal secret")?,
        elgamal_pubkey: checked_array(elgamal.pubkey().to_bytes().as_slice(), "ElGamal pubkey")?,
        aes_key: {
            let bytes: [u8; AES_KEY_LEN] = aes.clone().into();
            bytes
        },
    })
}

fn elgamal_from_secret(bytes: &[u8; ELGAMAL_SECRET_LEN]) -> Result<ElGamalKeypair> {
    let secret = ElGamalSecretKey::try_from(bytes.as_slice())
        .map_err(|e| anyhow!("ElGamal secret: {e:?}"))?;
    Ok(ElGamalKeypair::new(secret))
}

fn aes_from_bytes(bytes: &[u8; AES_KEY_LEN]) -> Result<AeKey> {
    AeKey::try_from(bytes.as_slice()).map_err(|e| anyhow!("AES key: {e:?}"))
}

fn elgamal_pubkey_from_bytes(bytes: &[u8]) -> Result<ElGamalPubkey> {
    let arr = checked_array::<ELGAMAL_PUBKEY_LEN>(bytes, "ElGamal pubkey")?;
    let pod = PodElGamalPubkeyV6(arr);
    pod.try_into()
        .map_err(|e| anyhow!("ElGamal pubkey bytes: {e:?}"))
}

fn elgamal_ciphertext_from_bytes(bytes: &[u8]) -> Result<ElGamalCiphertext> {
    let arr = checked_array::<ELGAMAL_CIPHERTEXT_LEN>(bytes, "ElGamal ciphertext")?;
    let pod = PodElGamalCiphertextV6(arr);
    pod.try_into()
        .map_err(|e| anyhow!("ElGamal ciphertext bytes: {e:?}"))
}

fn ae_ciphertext_from_bytes(bytes: &[u8]) -> Result<AeCiphertext> {
    let arr = checked_array::<AE_CIPHERTEXT_LEN>(bytes, "AES ciphertext")?;
    AeCiphertext::from_bytes(&arr).ok_or_else(|| anyhow!("AES ciphertext bytes"))
}

fn parse_pubkey_validity_proof(bytes: &[u8]) -> Result<PubkeyValidityProofData> {
    parse_proof(bytes)
}

fn parse_proof<T: Pod + Copy>(bytes: &[u8]) -> Result<T> {
    let expected = size_of::<T>();
    anyhow::ensure!(
        bytes.len() == expected,
        "proof length {} != {}",
        bytes.len(),
        expected
    );
    bytemuck::try_from_bytes::<T>(bytes)
        .copied()
        .map_err(|e| anyhow!("proof bytes: {e}"))
}

fn proof_bytes<T: Pod>(value: &T) -> Result<Vec<u8>> {
    let bytes = bytemuck::bytes_of(value);
    anyhow::ensure!(!bytes.is_empty(), "proof encoded to zero bytes");
    Ok(bytes.to_vec())
}

fn pod_bytes<T: Pod, const N: usize>(value: &T) -> Result<[u8; N]> {
    checked_array(bytemuck::bytes_of(value), "POD")
}

fn context_info<'a>(
    context_account: &'a Address,
    context_authority: &'a Address,
) -> ContextStateInfo<'a> {
    ContextStateInfo {
        context_state_account: context_account,
        context_state_authority: context_authority,
    }
}

fn to_raw(ix: solana_instruction::Instruction) -> Result<RawInstruction> {
    Ok(RawInstruction {
        program_id: pubkey_field(&ix.program_id)?,
        accounts: ix
            .accounts
            .iter()
            .map(|meta| {
                Ok(RawAccountMeta {
                    pubkey: pubkey_field(&meta.pubkey)?,
                    is_signer: meta.is_signer,
                    is_writable: meta.is_writable,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        data: ix.data,
    })
}

fn pubkey_field<T>(value: &T) -> Result<[u8; 32]>
where
    T: AsRef<[u8]>,
{
    checked_array(value.as_ref(), "pubkey")
}

fn checked_array<const N: usize>(bytes: &[u8], label: &str) -> Result<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| anyhow!("{label} length {} != {N}", bytes.len()))
}

pub fn encode_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

pub fn decode_hex_array<const N: usize>(text: &str, label: &str) -> Result<[u8; N]> {
    let bytes = hex::decode(text).with_context(|| format!("{label} hex"))?;
    checked_array(&bytes, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubkey_validity_proof_verifies_and_rejects_a_flipped_byte() {
        let keys = generate_keys().unwrap();
        let mut proof = generate_pubkey_validity_proof(&keys.elgamal_secret).unwrap();
        assert!(!proof.is_empty());
        verify_pubkey_validity_proof(&proof).unwrap();
        let index = proof.len() / 2;
        proof[index] ^= 0x01;
        assert!(verify_pubkey_validity_proof(&proof).is_err());
    }

    #[test]
    fn verify_instruction_names_context_owner_and_authority() {
        let keys = generate_keys().unwrap();
        let proof = generate_pubkey_validity_proof(&keys.elgamal_secret).unwrap();
        let context = [7u8; 32];
        let authority = [9u8; 32];
        let ix = encode_verify_pubkey_validity(context, authority, &proof).unwrap();
        assert_eq!(ix.program_id, zk_proof_program_id());
        assert!(ix
            .accounts
            .iter()
            .any(|meta| meta.pubkey == context && meta.is_writable));
        assert!(ix.accounts.iter().any(|meta| meta.pubkey == authority));
    }

    #[test]
    fn context_reuse_for_another_token_account_is_rejected() {
        let context = [3u8; 32];
        let first = [1u8; 32];
        let second = [2u8; 32];
        require_unused_context(&[], &context, &first).unwrap();
        let err = require_unused_context(&[context], &context, &second).unwrap_err();
        assert!(err.to_string().contains("already used"));
    }

    #[test]
    fn transfer_proof_bytes_have_expected_lengths() {
        let source = generate_keys().unwrap();
        let dest = generate_keys().unwrap();
        let source_pair = crate::elgamal_from_secret(&source.elgamal_secret).unwrap();
        let dest_pair = crate::elgamal_from_secret(&dest.elgamal_secret).unwrap();
        let available_ct = source_pair.pubkey().encrypt(5u64);
        let available_ae = encrypt_aes(&source.aes_key, 5).unwrap();
        let bundle = generate_transfer_proofs(
            &available_ct.to_bytes(),
            &available_ae,
            1,
            &source.elgamal_secret,
            &source.aes_key,
            &dest_pair.pubkey().to_bytes(),
        )
        .unwrap();
        assert_eq!(bundle.transfer_data.len(), TRANSFER_DATA_LEN);
        assert_eq!(bundle.decryptable.len(), AE_CIPHERTEXT_LEN);
        assert!(!bundle.equality_proof.is_empty());
        assert!(!bundle.validity_proof.is_empty());
        assert!(!bundle.range_proof.is_empty());
    }

    #[test]
    fn close_context_and_record_instructions_name_the_expected_accounts() {
        let context = [7u8; 32];
        let authority = [9u8; 32];
        let dest = [8u8; 32];
        let close = encode_close_context(context, authority, dest).unwrap();
        assert_eq!(close.program_id, zk_proof_program_id());
        assert!(close
            .accounts
            .iter()
            .any(|meta| meta.pubkey == context && meta.is_writable));
        assert!(close.accounts.iter().any(|meta| meta.pubkey == authority));
        let record = [5u8; 32];
        let init = encode_record_initialize(record, authority).unwrap();
        assert_eq!(init.program_id, record_program_id());
        assert_eq!(init.data, vec![0]);
        let write = encode_record_write(record, authority, 33, &[1, 2, 3]).unwrap();
        assert_eq!(write.data[0], 1);
        assert!(write.accounts.iter().any(|meta| meta.is_signer));
        let close_record = encode_record_close(record, dest, authority).unwrap();
        assert_eq!(close_record.data, vec![3]);
    }

    #[test]
    fn encrypt_round_trip_and_length_checks() {
        let keys = generate_keys().unwrap();
        let ciphertext = encrypt_aes(&keys.aes_key, 100_000).unwrap();
        assert_eq!(ciphertext.len(), AE_CIPHERTEXT_LEN);
        assert_eq!(decrypt_aes(&keys.aes_key, &ciphertext).unwrap(), 100_000);
        assert!(encrypt_aes(&keys.aes_key, 0).is_ok());
        assert!(elgamal_pubkey(&keys.elgamal_secret).is_ok());
        assert_eq!(
            elgamal_pubkey(&keys.elgamal_secret).unwrap(),
            keys.elgamal_pubkey
        );
    }
}
