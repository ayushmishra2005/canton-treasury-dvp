use anyhow::{Context, Result};
use canton_treasury_dvp_devnet_zk::{
    encode_record_initialize, encode_record_write, encode_verify_equality,
    encode_verify_range_from_record, encode_verify_validity, equality_context_len,
    generate_transfer_proofs as generate_devnet_transfer_proofs, range_context_len,
    range_record_chunks, range_record_space, record_program_id, validity_context_len,
    zk_proof_program_id,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use solana_system_interface::instruction as system_instruction;
use spl_token_2022::extension::confidential_transfer::instruction as confidential_ix;
use spl_token_2022::extension::confidential_transfer::{
    ConfidentialTransferAccount, EncryptedBalance,
};
use spl_token_2022::extension::{BaseStateWithExtensions, StateWithExtensions};
use spl_token_2022::state::Account as TokenAccount;
use spl_token_confidential_transfer_proof_extraction::instruction::ProofLocation;

use crate::setup::{decryptable_from_bytes, raw_to_ix, ConfidentialAesKey, ConfidentialKeypair};

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
    source_elgamal: &ConfidentialKeypair,
    source_aes: &ConfidentialAesKey,
    source_token: &Pubkey,
    dest_elgamal: [u8; 32],
    amount: u64,
) -> Result<TransferProofs> {
    let source_acc = rpc.get_account(source_token)?;
    let source_state = StateWithExtensions::<TokenAccount>::unpack(&source_acc.data)?;
    let source_ext = source_state.get_extension::<ConfidentialTransferAccount>()?;
    let available = bytemuck::bytes_of(&source_ext.available_balance);
    let decryptable = bytemuck::bytes_of(&source_ext.decryptable_available_balance);
    let bundle = generate_devnet_transfer_proofs(
        available,
        decryptable,
        amount,
        source_elgamal.secret_bytes(),
        source_aes.as_bytes(),
        &dest_elgamal,
    )?;

    let equality = Keypair::new();
    let validity = Keypair::new();
    let range = Keypair::new();
    let record = Keypair::new();
    let zk_program = Pubkey::new_from_array(zk_proof_program_id());
    let record_program = Pubkey::new_from_array(record_program_id());
    let authority = payer.pubkey();

    let equality_size = equality_context_len();
    let validity_size = validity_context_len();
    let range_size = range_context_len();
    let record_space = range_record_space(bundle.range_proof.len());
    let rent_eq = rpc.get_minimum_balance_for_rent_exemption(equality_size)?;
    let rent_va = rpc.get_minimum_balance_for_rent_exemption(validity_size)?;
    let rent_ra = rpc.get_minimum_balance_for_rent_exemption(range_size)?;
    let rent_record = rpc.get_minimum_balance_for_rent_exemption(record_space)?;

    send(
        rpc,
        &[
            system_instruction::create_account(
                &authority,
                &equality.pubkey(),
                rent_eq,
                equality_size as u64,
                &zk_program,
            ),
            raw_to_ix(encode_verify_equality(
                equality.pubkey().to_bytes(),
                authority.to_bytes(),
                &bundle.equality_proof,
            )?)?,
        ],
        &[payer, &equality],
    )?;
    send(
        rpc,
        &[
            system_instruction::create_account(
                &authority,
                &validity.pubkey(),
                rent_va,
                validity_size as u64,
                &zk_program,
            ),
            raw_to_ix(encode_verify_validity(
                validity.pubkey().to_bytes(),
                authority.to_bytes(),
                &bundle.validity_proof,
            )?)?,
        ],
        &[payer, &validity],
    )?;

    let chunks = range_record_chunks(&bundle.range_proof)?;
    let mut first = vec![
        system_instruction::create_account(
            &authority,
            &record.pubkey(),
            rent_record,
            record_space as u64,
            &record_program,
        ),
        raw_to_ix(encode_record_initialize(
            record.pubkey().to_bytes(),
            authority.to_bytes(),
        )?)?,
    ];
    let (first_offset, first_bytes) = &chunks[0];
    first.push(raw_to_ix(encode_record_write(
        record.pubkey().to_bytes(),
        authority.to_bytes(),
        *first_offset,
        first_bytes,
    )?)?);
    send(rpc, &first, &[payer, &record])?;
    for (offset, bytes) in chunks.iter().skip(1) {
        send(
            rpc,
            &[raw_to_ix(encode_record_write(
                record.pubkey().to_bytes(),
                authority.to_bytes(),
                *offset,
                bytes,
            )?)?],
            &[payer],
        )?;
    }
    send(
        rpc,
        &[
            system_instruction::create_account(
                &authority,
                &range.pubkey(),
                rent_ra,
                range_size as u64,
                &zk_program,
            ),
            raw_to_ix(encode_verify_range_from_record(
                range.pubkey().to_bytes(),
                authority.to_bytes(),
                record.pubkey().to_bytes(),
            )?)?,
        ],
        &[payer, &range],
    )?;

    Ok(TransferProofs {
        equality,
        validity,
        range,
        transfer_data: bundle.transfer_data,
        decryptable: bundle.decryptable,
        auditor_lo: bundle.auditor_lo,
        auditor_hi: bundle.auditor_hi,
    })
}

pub fn decryptable_bytes(value: &[u8; 36]) -> Result<[u8; 36]> {
    anyhow::ensure!(value.len() == 36, "decryptable length {}", value.len());
    Ok(*value)
}

pub fn confidential_transfer_ixs(
    token_program: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    proofs: &TransferProofs,
) -> Result<Vec<Instruction>> {
    anyhow::ensure!(
        proofs.transfer_data[164] == 0
            && proofs.transfer_data[165] == 0
            && proofs.transfer_data[166] == 0,
        "transfer data still carries inline proof offsets"
    );
    let decryptable = decryptable_from_bytes(&proofs.decryptable)?;
    let lo = pod_ciphertext(&proofs.auditor_lo)?;
    let hi = pod_ciphertext(&proofs.auditor_hi)?;
    confidential_ix::transfer(
        token_program,
        source,
        mint,
        destination,
        &decryptable,
        &lo,
        &hi,
        authority,
        &[],
        ProofLocation::ContextStateAccount(&proofs.equality.pubkey()),
        ProofLocation::ContextStateAccount(&proofs.validity.pubkey()),
        ProofLocation::ContextStateAccount(&proofs.range.pubkey()),
    )
    .map_err(|e| anyhow::anyhow!("confidential transfer instruction: {e}"))
}

fn pod_ciphertext(bytes: &[u8; 64]) -> Result<EncryptedBalance> {
    bytemuck::try_from_bytes::<EncryptedBalance>(bytes)
        .copied()
        .map_err(|e| anyhow::anyhow!("auditor ciphertext bytes: {e}"))
}

fn send(rpc: &RpcClient, ixs: &[Instruction], signers: &[&Keypair]) -> Result<()> {
    let blockhash = rpc.get_latest_blockhash()?;
    let tx =
        Transaction::new_signed_with_payer(ixs, Some(&signers[0].pubkey()), signers, blockhash);
    rpc.send_and_confirm_transaction(&tx)
        .context("confidential proof transaction")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Signer;

    #[test]
    fn transfer_instruction_uses_context_accounts_and_rejects_inline_offsets() {
        let mut proofs = TransferProofs {
            equality: Keypair::new(),
            validity: Keypair::new(),
            range: Keypair::new(),
            transfer_data: [0u8; 167],
            decryptable: [0u8; 36],
            auditor_lo: [0u8; 64],
            auditor_hi: [0u8; 64],
        };
        let keys = canton_treasury_dvp_devnet_zk::generate_keys().unwrap();
        proofs.decryptable = canton_treasury_dvp_devnet_zk::encrypt_aes(&keys.aes_key, 0).unwrap();
        proofs.transfer_data[..36].copy_from_slice(&proofs.decryptable);
        let source = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let dest = Pubkey::new_unique();
        let authority = Pubkey::new_unique();
        let ixs = confidential_transfer_ixs(
            &spl_token_2022::id(),
            &source,
            &mint,
            &dest,
            &authority,
            &proofs,
        )
        .unwrap();
        assert_eq!(ixs.len(), 1);
        assert!(ixs[0]
            .accounts
            .iter()
            .any(|meta| meta.pubkey == proofs.equality.pubkey()));
        assert!(ixs[0]
            .accounts
            .iter()
            .any(|meta| meta.pubkey == proofs.validity.pubkey()));
        assert!(ixs[0]
            .accounts
            .iter()
            .any(|meta| meta.pubkey == proofs.range.pubkey()));
        assert!(!ixs[0]
            .accounts
            .iter()
            .any(|meta| { meta.pubkey == solana_sdk::sysvar::instructions::id() }));
        proofs.transfer_data[164] = 1;
        assert!(confidential_transfer_ixs(
            &spl_token_2022::id(),
            &source,
            &mint,
            &dest,
            &authority,
            &proofs,
        )
        .is_err());
    }
}
